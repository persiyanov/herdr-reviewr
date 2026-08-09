//! Explicit, user-confirmed Git mutations.
//!
//! This is the one write boundary for commit, discard, and push. The ordinary
//! [`crate::git`] module stays read-only apart from reviewr's private refs.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};

use crate::model::ChangedFile;

static TEMP_INDEX_ID: AtomicU64 = AtomicU64::new(0);

/// One changed-file row together with the exact repository state the user saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardedChange {
    pub file: ChangedFile,
    paths: Vec<PathState>,
}

/// The guarded selection shown by a commit or discard dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardedSelection {
    head: Option<String>,
    pub changes: Vec<GuardedChange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathState {
    path: String,
    worktree: WorktreeState,
    index: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorktreeState {
    Missing,
    File { len: u64, hash: u64 },
    Symlink(OsString),
    Other,
}

/// One serialized repository mutation requested by the UI.
#[derive(Clone, Debug)]
pub enum Request {
    OpenCommit { files: Vec<ChangedFile> },
    Commit { selection: GuardedSelection, message: String },
    OpenDiscard { file: ChangedFile },
    Discard { selection: GuardedSelection },
    Push,
}

impl Request {
    pub fn kind(&self) -> Kind {
        match self {
            Self::OpenCommit { .. } => Kind::PrepareCommit,
            Self::Commit { .. } => Kind::Commit,
            Self::OpenDiscard { .. } => Kind::PrepareDiscard,
            Self::Discard { .. } => Kind::Discard,
            Self::Push => Kind::Push,
        }
    }
}

/// The operation named by a completion and by the UI's busy state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    PrepareCommit,
    Commit,
    PrepareDiscard,
    Discard,
    Push,
}

/// Successful mutation detail for the status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Success {
    CommitDialog(GuardedSelection),
    Commit { oid: String, files: usize, warning: Option<String> },
    DiscardDialog(GuardedSelection),
    Discard { path: String },
    Push { branch: String },
}

/// The worker's one-for-one answer to a request.
#[derive(Debug)]
pub struct Completion {
    pub kind: Kind,
    pub result: Result<Success, String>,
}

/// Spawn the single repository-action worker. Requests are executed in order.
pub fn spawn(
    repo: PathBuf,
    rx: Receiver<Request>,
    tx: Sender<Completion>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let kind = request.kind();
            let result = execute(&repo, request).map_err(|error| error.to_string());
            if tx.send(Completion { kind, result }).is_err() {
                break;
            }
        }
    })
}

fn execute(repo: &Path, request: Request) -> Result<Success> {
    match request {
        Request::OpenCommit { files } => Ok(Success::CommitDialog(guard(repo, &files)?)),
        Request::Commit { selection, message } => commit(repo, &selection, &message),
        Request::OpenDiscard { file } => Ok(Success::DiscardDialog(guard(repo, &[file])?)),
        Request::Discard { selection } => discard(repo, &selection),
        Request::Push => push(repo),
    }
}

/// Capture the opening state for changed-file rows. Each row carries both sides of a rename.
pub fn guard(repo: &Path, files: &[ChangedFile]) -> Result<GuardedSelection> {
    let head = head(repo)?;
    let changes = files
        .iter()
        .map(|file| {
            let paths = change_paths(file)
                .into_iter()
                .map(|path| path_state(repo, path))
                .collect::<Result<Vec<_>>>()?;
            Ok(GuardedChange { file: file.clone(), paths })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(GuardedSelection { head, changes })
}

impl GuardedSelection {
    /// Keep only picker rows selected by the user while retaining their opening guards.
    #[must_use]
    pub fn selected(&self, selected: &[bool]) -> Self {
        let changes = self
            .changes
            .iter()
            .zip(selected)
            .filter(|(_, picked)| **picked)
            .map(|(change, _)| change.clone())
            .collect();
        Self { head: self.head.clone(), changes }
    }

    fn verify(&self, repo: &Path) -> Result<()> {
        if head(repo)? != self.head {
            bail!("HEAD changed; reopen the dialog and try again");
        }
        for change in &self.changes {
            for expected in &change.paths {
                if path_state(repo, &expected.path)? != *expected {
                    bail!("{} changed; reopen the dialog and try again", change.file.path);
                }
            }
        }
        Ok(())
    }
}

fn head(repo: &Path) -> Result<Option<String>> {
    let out = run(repo, &["rev-parse", "--verify", "HEAD"])?;
    Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

fn change_paths(file: &ChangedFile) -> Vec<&str> {
    let mut paths = Vec::with_capacity(2);
    if let Some(previous) = file.previous_path.as_deref() {
        paths.push(previous);
    }
    paths.push(&file.path);
    paths
}

fn literal(path: &str) -> String {
    format!(":(top,literal){path}")
}

fn path_state(repo: &Path, path: &str) -> Result<PathState> {
    let absolute = repo.join(path);
    let worktree = match std::fs::symlink_metadata(&absolute) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => WorktreeState::Missing,
        Err(error) => return Err(error).with_context(|| format!("reading {path}")),
        Ok(meta) if meta.file_type().is_symlink() => {
            WorktreeState::Symlink(std::fs::read_link(&absolute)?.into_os_string())
        }
        Ok(meta) if meta.is_file() => {
            let bytes = std::fs::read(&absolute).with_context(|| format!("reading {path}"))?;
            let mut hasher = DefaultHasher::new();
            bytes.hash(&mut hasher);
            WorktreeState::File { len: meta.len(), hash: hasher.finish() }
        }
        Ok(_) => WorktreeState::Other,
    };
    let spec = literal(path);
    let out = run(repo, &["ls-files", "--stage", "-z", "--", &spec])?;
    if !out.status.success() {
        bail!("git ls-files failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(PathState { path: path.to_string(), worktree, index: out.stdout })
}

fn commit(repo: &Path, selection: &GuardedSelection, message: &str) -> Result<Success> {
    if selection.changes.is_empty() {
        bail!("select at least one file");
    }
    if message.trim().is_empty() {
        bail!("commit message required");
    }
    selection.verify(repo)?;

    let temp = TempIndex::new();
    if selection.head.is_some() {
        git_index(repo, &temp.path, &["read-tree", "HEAD"], None)?;
    } else {
        git_index(repo, &temp.path, &["read-tree", "--empty"], None)?;
    }
    let specs = selection_pathspecs(selection);
    let mut add = vec!["add".to_string(), "-A".to_string(), "--".to_string()];
    add.extend(specs.iter().cloned());
    git_index_owned(repo, &temp.path, &add, None)?;

    // Verify once more after the temporary index captured the content. A later edit is not in
    // the commit tree and remains visible after the real index is reconciled.
    selection.verify(repo)?;
    git_index(repo, &temp.path, &["commit", "--quiet", "-F", "-"], Some(message.as_bytes()))?;

    let oid = git_stdout(repo, &["rev-parse", "--short", "HEAD"])?;
    let mut reset =
        vec!["reset".to_string(), "--quiet".to_string(), "HEAD".to_string(), "--".to_string()];
    reset.extend(specs);
    let warning = run_owned(repo, &reset)
        .and_then(|out| {
            out.status.success().then_some(()).ok_or_else(|| {
                anyhow!(
                    "index reconciliation failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )
            })
        })
        .err()
        .map(|error| error.to_string());
    Ok(Success::Commit { oid: oid.trim().to_string(), files: selection.changes.len(), warning })
}

fn selection_pathspecs(selection: &GuardedSelection) -> Vec<String> {
    let mut specs = Vec::new();
    for change in &selection.changes {
        for path in change_paths(&change.file) {
            let spec = literal(path);
            if !specs.contains(&spec) {
                specs.push(spec);
            }
        }
    }
    specs
}

fn discard(repo: &Path, selection: &GuardedSelection) -> Result<Success> {
    if selection.changes.len() != 1 {
        bail!("discard requires one file");
    }
    selection.verify(repo)?;
    let change = &selection.changes[0];
    let mut restore = Vec::new();
    let mut remove = Vec::new();
    for state in &change.paths {
        if in_head(repo, &state.path)? {
            restore.push(literal(&state.path));
        } else {
            remove.push(state);
        }
    }
    if !restore.is_empty() {
        let mut args = vec![
            "restore".to_string(),
            "--source=HEAD".to_string(),
            "--staged".to_string(),
            "--worktree".to_string(),
            "--".to_string(),
        ];
        args.extend(restore);
        git_owned(repo, &args)?;
    }
    for state in remove {
        if !state.index.is_empty() {
            git_owned(
                repo,
                &[
                    "rm".into(),
                    "--quiet".into(),
                    "--cached".into(),
                    "-f".into(),
                    "--".into(),
                    literal(&state.path),
                ],
            )?;
        }
        let path = repo.join(&state.path);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
        .with_context(|| format!("removing {}", state.path))?;
    }
    Ok(Success::Discard { path: change.file.path.clone() })
}

fn in_head(repo: &Path, path: &str) -> Result<bool> {
    if head(repo)?.is_none() {
        return Ok(false);
    }
    let spec = literal(path);
    let out = run(repo, &["ls-tree", "-z", "HEAD", "--", &spec])?;
    if !out.status.success() {
        bail!("git ls-tree failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(!out.stdout.is_empty())
}

fn push(repo: &Path) -> Result<Success> {
    let branch = git_stdout(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .context("cannot push a detached HEAD")?;
    let branch = branch.trim();
    let format = "%(upstream:remotename)%00%(upstream:remoteref)";
    let reference = format!("refs/heads/{branch}");
    let upstream = git_stdout(repo, &["for-each-ref", &reference, "--format", format])?;
    let mut fields = upstream.trim_end().split('\0');
    let remote = fields.next().unwrap_or_default();
    let remote_ref = fields.next().unwrap_or_default();
    if remote.is_empty() || remote_ref.is_empty() {
        bail!("{branch} has no configured upstream");
    }
    let refspec = format!("HEAD:{remote_ref}");
    git(repo, &["push", "--porcelain", "--", remote, &refspec])?;
    Ok(Success::Push { branch: branch.to_string() })
}

struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    fn new() -> Self {
        let id = TEMP_INDEX_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("herdr-reviewr-index-{}-{id}", std::process::id()));
        Self { path }
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let mut lock = self.path.as_os_str().to_owned();
        lock.push(".lock");
        let _ = std::fs::remove_file(Path::new(&lock));
    }
}

fn base_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotepath=false"])
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn run(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    base_command(repo).args(args).output().with_context(|| format!("running git {args:?}"))
}

fn run_owned(repo: &Path, args: &[String]) -> Result<std::process::Output> {
    base_command(repo).args(args).output().with_context(|| format!("running git {args:?}"))
}

fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let out = run(repo, args)?;
    if !out.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn git_owned(repo: &Path, args: &[String]) -> Result<()> {
    let out = run_owned(repo, args)?;
    if !out.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn git_stdout(repo: &Path, args: &[&str]) -> Result<String> {
    let out = run(repo, args)?;
    if !out.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_index(repo: &Path, index: &Path, args: &[&str], input: Option<&[u8]>) -> Result<()> {
    let args = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
    git_index_owned(repo, index, &args, input)
}

fn git_index_owned(repo: &Path, index: &Path, args: &[String], input: Option<&[u8]>) -> Result<()> {
    let mut command = base_command(repo);
    command.env("GIT_INDEX_FILE", index).args(args);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running git {args:?}"))?;
    if let Some(input) = input {
        child.stdin.take().expect("piped stdin").write_all(input)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChangeKind;
    use tempfile::TempDir;

    fn repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]).unwrap();
        git(dir.path(), &["config", "user.name", "Test"]).unwrap();
        git(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        dir
    }

    fn write(repo: &Path, path: &str, content: &str) {
        let target = repo.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(target, content).unwrap();
    }

    fn changed(path: &str, kind: ChangeKind) -> ChangedFile {
        ChangedFile { path: path.into(), kind, additions: 0, deletions: 0, previous_path: None }
    }

    #[test]
    fn commit_uses_worktree_selection_and_preserves_unrelated_index() {
        let repo = repo();
        write(repo.path(), "a", "old\n");
        write(repo.path(), "b", "old\n");
        git(repo.path(), &["add", "-A"]).unwrap();
        git(repo.path(), &["commit", "-qm", "init"]).unwrap();

        write(repo.path(), "a", "staged\n");
        git(repo.path(), &["add", "a"]).unwrap();
        write(repo.path(), "a", "worktree\n");
        write(repo.path(), "b", "staged b\n");
        git(repo.path(), &["add", "b"]).unwrap();
        write(repo.path(), "new file", "new\n");
        let selection = guard(
            repo.path(),
            &[changed("a", ChangeKind::Modified), changed("new file", ChangeKind::Untracked)],
        )
        .unwrap();
        let result = commit(repo.path(), &selection, "picked").unwrap();
        assert!(matches!(result, Success::Commit { files: 2, warning: None, .. }));
        assert_eq!(git_stdout(repo.path(), &["show", "HEAD:a"]).unwrap(), "worktree\n");
        assert_eq!(git_stdout(repo.path(), &["show", "HEAD:new file"]).unwrap(), "new\n");
        let staged = git_stdout(repo.path(), &["diff", "--cached", "--name-only"]).unwrap();
        assert_eq!(staged.trim(), "b");
    }

    #[test]
    fn changed_selection_is_rejected() {
        let repo = repo();
        write(repo.path(), "a", "old\n");
        git(repo.path(), &["add", "a"]).unwrap();
        git(repo.path(), &["commit", "-qm", "init"]).unwrap();
        write(repo.path(), "a", "seen\n");
        let selection = guard(repo.path(), &[changed("a", ChangeKind::Modified)]).unwrap();
        write(repo.path(), "a", "newer\n");
        assert!(
            commit(repo.path(), &selection, "nope").unwrap_err().to_string().contains("changed")
        );
    }

    #[test]
    fn discard_restores_a_rename_and_removes_untracked() {
        let repo = repo();
        write(repo.path(), "old", "content\n");
        git(repo.path(), &["add", "old"]).unwrap();
        git(repo.path(), &["commit", "-qm", "init"]).unwrap();
        std::fs::rename(repo.path().join("old"), repo.path().join("new")).unwrap();
        git(repo.path(), &["add", "-A"]).unwrap();
        let mut rename = changed("new", ChangeKind::Renamed);
        rename.previous_path = Some("old".into());
        let selection = guard(repo.path(), &[rename]).unwrap();
        discard(repo.path(), &selection).unwrap();
        assert_eq!(std::fs::read_to_string(repo.path().join("old")).unwrap(), "content\n");
        assert!(!repo.path().join("new").exists());
        assert!(git_stdout(repo.path(), &["status", "--short"]).unwrap().is_empty());

        write(repo.path(), "scratch", "x\n");
        let selection = guard(repo.path(), &[changed("scratch", ChangeKind::Untracked)]).unwrap();
        discard(repo.path(), &selection).unwrap();
        assert!(!repo.path().join("scratch").exists());
    }

    #[test]
    fn push_requires_and_uses_the_configured_upstream() {
        let repo = repo();
        write(repo.path(), "a", "a\n");
        git(repo.path(), &["add", "a"]).unwrap();
        git(repo.path(), &["commit", "-qm", "init"]).unwrap();
        assert!(push(repo.path()).unwrap_err().to_string().contains("no configured upstream"));

        let remote = TempDir::new().unwrap();
        git(remote.path(), &["init", "-q", "--bare"]).unwrap();
        git(repo.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]).unwrap();
        git(repo.path(), &["push", "-qu", "origin", "main"]).unwrap();
        write(repo.path(), "a", "b\n");
        git(repo.path(), &["commit", "-qam", "next"]).unwrap();
        assert_eq!(push(repo.path()).unwrap(), Success::Push { branch: "main".into() });
        let local = git_stdout(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        let remote_head = git_stdout(remote.path(), &["rev-parse", "refs/heads/main"]).unwrap();
        assert_eq!(local, remote_head);
    }
}
