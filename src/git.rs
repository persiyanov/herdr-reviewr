//! Read-only git access: scopes, changed files, and diffs.
//!
//! See `specs/review-model.md`. Every call here only reads — it never commits,
//! stages, or mutates the worktree or refs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::model::{ChangeKind, ChangedFile, Scope};

/// Run `git -C <repo> <args>` and return stdout. Errors on non-zero exit.
fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !out.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like [`git`], but returns stdout even on non-zero exit (e.g. `diff --no-index`).
fn git_lenient(repo: &Path, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Run `git -C <repo> <args>` and return its trimmed stdout, or `None` if the command fails to
/// spawn, exits non-zero, or prints nothing. The one-line query workhorse for `rev-parse`/`merge-base`.
fn git_line(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Whether `git -C <repo> <args>` spawns and exits zero. The predicate workhorse for existence checks.
fn git_ok(repo: &Path, args: &[&str]) -> bool {
    Command::new("git").arg("-C").arg(repo).args(args).output().is_ok_and(|o| o.status.success())
}

/// Whether `path` is inside a git work tree.
pub fn is_repo(path: &Path) -> bool {
    git_ok(path, &["rev-parse", "--is-inside-work-tree"])
}

/// The git top-level of `path`, or `None` if it is not a repo.
pub fn toplevel(path: &Path) -> Option<PathBuf> {
    git_line(path, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// A canonical GitHub API and repository target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoTarget {
    host: String,
    owner: String,
    name: String,
}

impl RepoTarget {
    /// Build one canonical repository target from a hostname and GitHub owner/name pair.
    pub(crate) fn new(host: &str, owner: &str, name: &str) -> Option<Self> {
        let host = host.to_ascii_lowercase();
        (crate::config::valid_host_syntax(&host)
            && valid_repository_component(owner)
            && valid_repository_component(name))
        .then(|| Self { host, owner: owner.to_owned(), name: name.to_owned() })
    }

    /// The lowercase canonical forge hostname.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The repository owner used at the GitHub API boundary.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The repository name used at the GitHub API boundary.
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Host classification for one candidate repository remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepositoryIdentity {
    Repository(RepoTarget),
    Missing,
    Hostless,
    Unsupported(String),
    Malformed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteTransport {
    Ssh,
    Hosted,
    Unsupported,
}

/// Classify one repository URL against GitHub.com and the configured Enterprise host.
fn classify_remote(url: &str, enterprise: Option<&str>) -> RepositoryIdentity {
    let Some((transport, host, path, has_port)) = split_remote(url) else {
        return RepositoryIdentity::Hostless;
    };
    if host.is_empty() {
        return RepositoryIdentity::Hostless;
    }
    let host = host.to_ascii_lowercase();
    if transport == RemoteTransport::Unsupported
        || (transport == RemoteTransport::Hosted && has_port)
    {
        return RepositoryIdentity::Unsupported(host);
    }
    let Some(canonical) = canonical_supported_host(&host, enterprise) else {
        return RepositoryIdentity::Unsupported(host);
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return RepositoryIdentity::Malformed(canonical);
    };
    match RepoTarget::new(&canonical, owner, name) {
        Some(target) => RepositoryIdentity::Repository(target),
        None => RepositoryIdentity::Malformed(canonical),
    }
}

/// Return the exact GitHub.com or configured Enterprise host, lowercased.
fn canonical_supported_host(host: &str, enterprise: Option<&str>) -> Option<String> {
    let host = host.to_ascii_lowercase();
    (host == "github.com" || enterprise == Some(host.as_str())).then_some(host)
}

/// Split a Git remote URL into transport, host, and path for scheme and scp-style forms.
fn split_remote(url: &str) -> Option<(RemoteTransport, &str, &str, bool)> {
    if let Some((scheme, rest)) = url.split_once("://") {
        let rest = rest.split_once('@').map_or(rest, |(_, r)| r); // drop `user@`
        let (hostport, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = hostport.split_once(':').map_or((hostport, None), |(h, p)| (h, Some(p)));
        let transport = match scheme.to_ascii_lowercase().as_str() {
            "ssh" => RemoteTransport::Ssh,
            "http" | "https" | "git" => RemoteTransport::Hosted,
            _ => RemoteTransport::Unsupported,
        };
        Some((transport, host, path, port.is_some()))
    } else {
        // scp-like `[user@]host:path` — the first `:` splits host from path.
        let (hostpart, path) = url.split_once(':')?;
        let host = hostpart.split_once('@').map_or(hostpart, |(_, h)| h);
        Some((RemoteTransport::Ssh, host, path, false))
    }
}

// --- PR-fetch local reads (publication points) ------------------------------------
//
// See `specs/forge-host.md` "Resolution". Repository selection and
// branch-state derivation both use the same failure contract: a git command that *fails* is a
// transient [`GitFail`], never read as absence. The caller distinguishes a target read failure
// from a later branch-state failure so only an unproven target replaces the visible snapshot.

/// A git command that failed (spawn error or unexpected non-zero exit) during the PR
/// fetch's local reads — a transient failure per `specs/forge-host.md`, never absence.
#[derive(Debug)]
pub struct GitFail(pub String);

/// Spawn one PR-fetch git read. `LC_ALL=C` pins Git's messages to English — remote discovery
/// classifies a missing remote by stderr text, which Git otherwise localizes.
fn run_git(repo: &Path, args: &[&str]) -> Result<std::process::Output, GitFail> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| GitFail(format!("git {args:?}: {e}")))
}

/// Run git where exit 0 is a value, exit 1 is a designated clean absence (`--verify
/// --quiet`, `symbolic-ref --quiet`, `cat-file -e`), and anything else is a failure.
fn git_tristate(repo: &Path, args: &[&str]) -> Result<Option<String>, GitFail> {
    let out = run_git(repo, args)?;
    if out.status.success() {
        return Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()));
    }
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    Err(GitFail(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr).trim())))
}

/// Run git where any non-zero exit is a failure. Exit 0 with empty output is a clean
/// "found nothing" (e.g. `for-each-ref` matching no refs).
fn git_strict(repo: &Path, args: &[&str]) -> Result<String, GitFail> {
    let out = run_git(repo, args)?;
    if !out.status.success() {
        return Err(GitFail(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Everything that determines one PR fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFetchInput {
    pub repository: RepositoryIdentity,
    /// The `origin` repository, when it is a usable GitHub identity — the home of the
    /// worktree's published commits, so the association query runs there.
    pub origin_repository: Option<RepoTarget>,
    /// The locally derived pins, points, and tiebreak, read in the same pass.
    pub local: PrLocalState,
}

/// One published commit that nominates PRs, with the `origin` branch names at its tip
/// (the closed-unmerged epilogue's exact-identity lookup).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationPoint {
    pub oid: String,
    pub names: Vec<String>,
}

/// The local identity one PR fetch derives: pins, publication points, and the upstream name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrLocalState {
    /// `HEAD` pinned to an OID at the start of the pass; every ancestry test, distance,
    /// and the `sync` count use this pin, so one fetch reads one consistent local state.
    pub head_oid: Option<String>,
    /// The winning base entry pinned to an OID — the paint guard keys on it, so a base
    /// moving mid-fetch never paints a stale verdict. Point filtering uses every resolved
    /// base, not this pin alone.
    pub base_oid: Option<String>,
    /// The publication points: the nearest ancestors of the pinned `HEAD` present on
    /// `origin`, each beyond every resolved base. Empty means no reviewable work is published.
    pub points: Vec<PublicationPoint>,
    /// The published-but-absorbed candidates: base-history commits this worktree is
    /// parked on, kept only when no point survives. A merged PR whose head is exactly
    /// one of them still resolves as the epilogue (`specs/forge-host.md`).
    pub absorbed: Vec<String>,
    /// The pinned `HEAD` nominates by exact identity: it sits outside every resolved
    /// base's history, so a PR whose head is exactly this commit is provably this
    /// worktree's — the exact-identity epilogue (`specs/forge-host.md`).
    pub head_nominates: bool,
    /// The recorded upstream's bare branch name, the last open-PR tiebreak. A record
    /// naming a configured base is tracking, not publication, and is dropped.
    pub upstream: Option<String>,
    /// `HEAD` is detached — post-merge cleanup, not a review seat.
    pub detached: bool,
}

/// Derive the pinned `HEAD`, pinned bases, and publication points (`specs/forge-host.md`).
pub fn pr_local(
    repo: &Path,
    base_flag: Option<&str>,
    config_bases: &[String],
) -> Result<PrLocalState, GitFail> {
    let Some(branch) = git_tristate(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])? else {
        return Ok(PrLocalState { detached: true, ..PrLocalState::default() });
    };
    let head_oid = git_tristate(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?;
    let bases = resolve_bases(repo, base_flag, config_bases)?;
    let (points, absorbed) = match &head_oid {
        // With no base resolvable, no point is provable (`specs/forge-host.md`).
        Some(head) if !bases.is_empty() => publication_points(repo, head, &bases)?,
        _ => (Vec::new(), Vec::new()),
    };
    let head_nominates = match &head_oid {
        Some(head) if !bases.is_empty() => beyond_all_bases(repo, head, &bases)?,
        _ => false,
    };
    let upstream = recorded_upstream(repo, &branch, base_flag, config_bases)?;
    Ok(PrLocalState {
        head_oid,
        base_oid: bases.into_iter().next(),
        points,
        absorbed,
        head_nominates,
        upstream,
        detached: false,
    })
}

/// Every resolved base OID in precedence order, deduped: the `--base` flag (verbatim rev
/// first, then as a canonical entry), each canonical `config_bases` entry, then the default
/// branch `origin/HEAD` names (`specs/review-model.md`). Points must lie beyond all of them.
fn resolve_bases(
    repo: &Path,
    base_flag: Option<&str>,
    config_bases: &[String],
) -> Result<Vec<String>, GitFail> {
    let mut out: Vec<String> = Vec::new();
    let push = |oid: Option<String>, out: &mut Vec<String>| {
        if let Some(oid) = oid
            && !out.contains(&oid)
        {
            out.push(oid);
        }
    };
    if let Some(flag) = base_flag.filter(|b| !b.is_empty()) {
        // The flag is the power-user escape hatch: any rev works verbatim (a SHA, a tag,
        // `upstream/main`), and a branch-entry spelling falls back to canonical resolution.
        let probe = format!("{flag}^{{commit}}");
        let verbatim = git_tristate(repo, &["rev-parse", "--verify", "--quiet", &probe])?;
        if let Some(oid) = verbatim {
            push(Some(oid), &mut out);
        } else {
            let entry = crate::config::canonical_base(flag);
            push(resolve_base_entry(repo, &entry)?, &mut out);
        }
    }
    for entry in config_bases {
        push(resolve_base_entry(repo, entry)?, &mut out);
    }
    let default_branch = git_tristate(
        repo,
        &["rev-parse", "--verify", "--quiet", "refs/remotes/origin/HEAD^{commit}"],
    )?;
    push(default_branch, &mut out);
    Ok(out)
}

/// The `origin` remote-tracking tips as `(OID, bare name)`, `origin/HEAD` excluded — one
/// listing per pass serves every point's names and the published-at-all short-circuit.
fn origin_tips(repo: &Path) -> Result<Vec<(String, String)>, GitFail> {
    let out = git_strict(
        repo,
        &["for-each-ref", "refs/remotes/origin", "--format=%(objectname) %(refname)"],
    )?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let (oid, refname) = line.split_once(' ')?;
            let name = refname.strip_prefix("refs/remotes/origin/")?;
            (name != "HEAD").then(|| (oid.to_string(), name.to_string()))
        })
        .collect())
}

/// The publication points: the boundary of the unpushed range — the nearest ancestors of
/// `head` present on any `origin/*` ref — or `head` itself when nothing is unpushed. Points
/// that are ancestors of any resolved base prove nothing for an open PR and become
/// `absorbed` candidates instead, kept only when no point survives — a merged PR whose
/// head is exactly one of them is still this worktree's epilogue. Capped at 8 survivors
/// and 4 absorbed from at most 32 boundary commits, so a merge-heavy frontier stays bounded.
fn publication_points(
    repo: &Path,
    head: &str,
    bases: &[String],
) -> Result<(Vec<PublicationPoint>, Vec<String>), GitFail> {
    let tips = origin_tips(repo)?;
    if tips.is_empty() {
        // Nothing is published at all; skip the history walk, which `--not
        // --remotes=origin` would otherwise run unbounded.
        return Ok((Vec::new(), Vec::new()));
    }
    let out = git_strict(repo, &["rev-list", "--boundary", head, "--not", "--remotes=origin"])?;
    let mut oids: Vec<String> = Vec::new();
    let mut saw_unpushed = false;
    for line in out.lines() {
        match line.strip_prefix('-') {
            Some(boundary) => oids.push(boundary.to_string()),
            None if !line.is_empty() => saw_unpushed = true,
            None => {}
        }
    }
    if !saw_unpushed && oids.is_empty() {
        // Nothing is unpushed: HEAD itself is published.
        oids.push(head.to_string());
    }
    oids.truncate(32);
    let mut points = Vec::new();
    let mut absorbed = Vec::new();
    for oid in oids {
        if points.len() >= 8 {
            break;
        }
        if !beyond_all_bases(repo, &oid, bases)? {
            if absorbed.len() < 4 {
                absorbed.push(oid);
            }
            continue;
        }
        let names =
            tips.iter().filter(|(tip, _)| tip == &oid).map(|(_, name)| name.clone()).collect();
        points.push(PublicationPoint { oid, names });
    }
    // Absorbed candidates matter only when nothing provable remains; a live point
    // owns the resolution outright.
    if !points.is_empty() {
        absorbed.clear();
    }
    Ok((points, absorbed))
}

/// Whether `commit` is an ancestor of (or equal to) `of`.
fn is_ancestor(repo: &Path, commit: &str, of: &str) -> Result<bool, GitFail> {
    Ok(git_tristate(repo, &["merge-base", "--is-ancestor", commit, of])?.is_some())
}

/// Whether `oid` lies beyond every resolved base — an ancestor of none of them. A point
/// filter and the `HEAD` nomination both ask this one question, so they never disagree.
fn beyond_all_bases(repo: &Path, oid: &str, bases: &[String]) -> Result<bool, GitFail> {
    for base in bases {
        if is_ancestor(repo, oid, base)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The target and origin identities from one read of each remote. The target resolves from
/// a readable supported `upstream`, falling back to `origin`; unusable identities fall
/// back, read errors do not. The origin identity rides along for the association query —
/// the worktree's commits live where it pushes (`specs/forge-host.md`). A usable `upstream`
/// already fixes the target, so an `origin` read that fails then costs only that fetch's
/// association source, not the whole read.
pub(crate) fn remote_identities(
    repo: &Path,
    github_host: Option<&str>,
) -> Result<(RepositoryIdentity, Option<RepoTarget>), GitFail> {
    let upstream = remote_identity(repo, "upstream", github_host)?;
    let origin = remote_identity(repo, "origin", github_host);
    let origin_target = match &origin {
        Ok(RepositoryIdentity::Repository(target)) => Some(target.clone()),
        _ => None,
    };
    let repository =
        if matches!(upstream, RepositoryIdentity::Repository(_)) { upstream } else { origin? };
    Ok((repository, origin_target))
}

/// Classify one rewritten primary fetch URL. A missing remote is a clean state; every other
/// `remote get-url` failure is transient. The command applies `url.*.insteadOf` rewrites.
fn remote_identity(
    repo: &Path,
    remote: &str,
    github_host: Option<&str>,
) -> Result<RepositoryIdentity, GitFail> {
    let args = ["remote", "get-url", "--", remote];
    let out = run_git(repo, &args)?;
    if out.status.success() {
        let url = std::str::from_utf8(&out.stdout)
            .map_err(|_| GitFail(format!("git remote get-url {remote}: invalid UTF-8")))?;
        return Ok(classify_remote(url.trim(), github_host));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.to_lowercase().contains("no such remote") {
        return Ok(RepositoryIdentity::Missing);
    }
    Err(GitFail(format!("git {args:?}: {}", stderr.trim())))
}

/// One canonical `base_branches` entry pinned to an OID: `refs/remotes/origin/<name>`,
/// then `refs/heads/<name>` (`specs/config.md`).
fn resolve_base_entry(repo: &Path, name: &str) -> Result<Option<String>, GitFail> {
    for prefix in ["refs/remotes/origin/", "refs/heads/"] {
        let probe = format!("{prefix}{name}^{{commit}}");
        if let Some(oid) = git_tristate(repo, &["rev-parse", "--verify", "--quiet", &probe])? {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

/// git's recorded upstream for `branch` (`branch.<name>.remote`/`merge`) as a bare branch
/// name, or `None` when unset, not under a remote, or naming a configured base — the record
/// `git switch -c work origin/main` auto-writes is tracking, not publication. `for-each-ref`
/// exits 0 with an empty field when unset, so absence never reads as failure (`rev-parse
/// @{u}` exits 128 for both). `%(push)` is deliberately not consulted: with any remote
/// present git *computes* a destination even with nothing recorded, which would shadow a
/// real record.
fn recorded_upstream(
    repo: &Path,
    branch: &str,
    base_flag: Option<&str>,
    config_bases: &[String],
) -> Result<Option<String>, GitFail> {
    let out = git_strict(
        repo,
        &["for-each-ref", &format!("refs/heads/{branch}"), "--format=%(upstream)"],
    )?;
    let dest = out.lines().next().unwrap_or("").trim();
    let Some(rest) = dest.strip_prefix("refs/remotes/") else { return Ok(None) };
    let Some((_, name)) = rest.split_once('/') else { return Ok(None) };
    let names_a_base = config_bases.iter().any(|entry| entry == name)
        || base_flag.is_some_and(|flag| crate::config::canonical_base(flag) == name);
    Ok((!name.is_empty() && !names_a_base).then(|| name.to_string()))
}

/// Commits `local` (the pinned `HEAD` OID) is ahead and behind `other` (the PR head OID).
/// `Ok(None)` when `other` is not in the object database — the PR head was never fetched
/// locally, a clean absence. Backs the PR `sync` indicator (`specs/forge-host.md`).
pub fn ahead_behind_oids(
    repo: &Path,
    local: &str,
    other: &str,
) -> Result<Option<(u32, u32)>, GitFail> {
    // Plain `-e` (no `^{commit}` peel): peeling a missing object exits 128, not the
    // clean-absence 1 this check relies on.
    if git_tristate(repo, &["cat-file", "-e", other])?.is_none() {
        return Ok(None);
    }
    let out =
        git_strict(repo, &["rev-list", "--left-right", "--count", &format!("{local}...{other}")])?;
    let mut it = out.split_whitespace();
    let parse = |s: Option<&str>| {
        s.and_then(|v| v.parse().ok())
            .ok_or_else(|| GitFail(format!("rev-list --left-right returned {out:?}")))
    };
    let ahead = parse(it.next())?;
    let behind = parse(it.next())?;
    Ok(Some((ahead, behind)))
}

/// The merge-base commit of the winning base and `HEAD` using one base-config snapshot.
/// Base entries resolve per `specs/review-model.md` precedence, `origin/HEAD` last.
pub fn merge_base(repo: &Path, base: Option<&str>, config_bases: &[String]) -> Option<String> {
    let base = resolve_bases(repo, base, config_bases).ok()?.into_iter().next()?;
    git_line(repo, &["merge-base", &base, "HEAD"])
}

/// The content of `path` at `rev` (`git show <rev>:<path>`). Empty when the path does
/// not exist at that rev — an added file against its old side, say.
pub fn file_content(repo: &Path, rev: &str, path: &str) -> String {
    git_lenient(repo, &["show", &format!("{rev}:{path}")])
}

// --- turn baseline (last-turn scope) -------------------------------------------
//
// See `specs/herdr-host.md`. The snapshot is non-disruptive: it writes a tree object
// from the worktree through a temporary index, never touching the real index, the
// worktree, or any branch, and persists the baseline under a private `refs/reviewr/`
// ref keyed by the worktree path.

/// A non-disruptive snapshot of the worktree as a tree object. Seeds a temporary index
/// from the repo's real index so unchanged files keep their cached hash, then `add -A`
/// and `write-tree`. Captures staged, unstaged, and untracked content alike. Touches
/// only the object database and the temp index — never the real index or any ref.
pub fn snapshot_worktree(repo: &Path) -> Result<String> {
    let git_dir = PathBuf::from(git(repo, &["rev-parse", "--absolute-git-dir"])?.trim());
    let tmp_index = git_dir.join("reviewr-turn-index");
    let real_index = git_dir.join("index");
    // Clear whatever a prior hard crash left — the temp index and the `.lock` git holds
    // while writing it (a leftover lock fails every later `add` with "File exists") — then
    // drop both on every exit path via the guard, so even a failed snapshot leaves nothing
    // behind in the git dir.
    let guard = TempIndex(&tmp_index);
    guard.clear();
    // Seed from the real index so git's stat cache lets unchanged files skip hashing;
    // a fresh repo may have no index yet, so start empty in that case.
    if real_index.exists() {
        std::fs::copy(&real_index, &tmp_index).context("seeding the snapshot index")?;
    }
    git_with_index(repo, &tmp_index, &["add", "-A"])?;
    let tree = git_with_index(repo, &tmp_index, &["write-tree"])?;
    Ok(tree.trim().to_string())
}

/// Removes a temporary index and its git lock file on drop, so a snapshot that fails midway
/// never leaves either behind.
struct TempIndex<'a>(&'a Path);

impl TempIndex<'_> {
    /// Removes the index and the `<index>.lock` git creates beside it while writing. Safe at
    /// any point we run: the lock's only legitimate holder is a live `git add` this process
    /// spawned and has already waited on.
    fn clear(&self) {
        let _ = std::fs::remove_file(self.0);
        let mut lock = self.0.as_os_str().to_owned();
        lock.push(".lock");
        let _ = std::fs::remove_file(Path::new(&lock));
    }
}

impl Drop for TempIndex<'_> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Like [`git`], but runs against a throwaway index via `GIT_INDEX_FILE` so the snapshot
/// never disturbs the repo's real index.
fn git_with_index(repo: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !out.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A stable per-worktree key for the baseline ref, from the absolute top-level path, so
/// sibling worktrees sharing one ref store do not collide. FNV-1a keeps it deterministic
/// across rebuilds — a std `DefaultHasher` is seeded per process and is not.
pub fn worktree_key(repo: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in repo.to_string_lossy().bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The private ref holding a worktree's turn baseline — outside `refs/heads`, so it
/// never appears in a branch list.
fn baseline_ref(key: &str) -> String {
    format!("refs/reviewr/turn-base/{key}")
}

/// The persisted turn baseline tree for this worktree, if a baseline exists.
pub fn read_baseline_ref(repo: &Path, key: &str) -> Option<String> {
    git_line(repo, &["rev-parse", "--verify", "--quiet", &baseline_ref(key)])
}

/// Persist the turn baseline tree under the worktree's private ref. `update-ref` is
/// atomic, so the baseline is never half-written.
pub fn write_baseline_ref(repo: &Path, key: &str, sha: &str) -> Result<()> {
    git(repo, &["update-ref", &baseline_ref(key), sha])?;
    Ok(())
}

/// git's well-known empty-tree object, used as the diff base when a repo has no commits.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// `HEAD` when the repo has a commit, else the empty tree (a commitless repo has no HEAD).
fn diff_base(repo: &Path) -> String {
    if git(repo, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok() {
        "HEAD".to_string()
    } else {
        EMPTY_TREE.to_string()
    }
}

/// The changed files for `scope`, sorted by path. `base` overrides the base-config snapshot.
/// `last-turn` is resolved separately by [`changed_against_tree`], so it lists nothing here.
pub fn changed_files(
    repo: &Path,
    scope: Scope,
    base: Option<&str>,
    config_bases: &[String],
) -> Result<Vec<ChangedFile>> {
    let (numstat, name_status) = match scope {
        Scope::Uncommitted => {
            // A repo with no commits has no HEAD; diff against the empty tree so a fresh
            // `git init` lists its files instead of erroring (which would kill the process).
            let base = diff_base(repo);
            (
                git(repo, &["diff", &base, "--numstat", "-z"])?,
                git(repo, &["diff", &base, "--name-status", "-z"])?,
            )
        }
        Scope::Branch => match merge_base(repo, base, config_bases) {
            Some(r) => (
                git(repo, &["diff", &r, "--numstat", "-z"])?,
                git(repo, &["diff", &r, "--name-status", "-z"])?,
            ),
            None => return Ok(Vec::new()),
        },
        Scope::LastTurn => return Ok(Vec::new()),
    };
    // Branch diffs against the worktree, so like uncommitted it carries untracked files
    // that `git diff` never reports.
    let include_untracked = matches!(scope, Scope::Uncommitted | Scope::Branch);
    assemble(repo, &numstat, &name_status, include_untracked)
}

/// The changed files between the turn baseline `tree` and the live worktree, for
/// `last-turn`. Snapshots the worktree now and diffs tree-against-tree, so staged,
/// unstaged, untracked, and committed-this-turn changes all show, with no phantom
/// deletion for a file that is untracked at both ends (which a tree-vs-worktree diff
/// would mis-report). Untracked files ride in the current snapshot, so no separate
/// untracked pass is needed.
pub fn changed_against_tree(repo: &Path, tree: &str) -> Result<Vec<ChangedFile>> {
    let current = snapshot_worktree(repo)?;
    let numstat = git(repo, &["diff", tree, &current, "--numstat", "-z"])?;
    let name_status = git(repo, &["diff", tree, &current, "--name-status", "-z"])?;
    assemble(repo, &numstat, &name_status, false)
}

/// One entry in the `All files` worktree listing: a path plus whether git ignores it and
/// whether it is a (lazily-expanded) directory placeholder (specs/file-list.md).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: String,
    pub ignored: bool,
    pub is_dir: bool,
}

/// Every entry in the worktree for the `All files` tab (specs/file-list.md): tracked and
/// untracked-not-ignored files from one `ls-files --cached --others` pass, and the ignored
/// entries from [`ignored_entries`] — a wholly-ignored directory collapsed to one `is_dir`
/// placeholder, an individually-ignored file as itself. `.git` is never reported. Deduped and
/// sorted; `-z` keeps paths with spaces or special characters verbatim.
pub fn all_files(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    // One spawn for tracked + untracked. `--others --exclude-standard` applies the same
    // standard exclude rules as the `status` untracked pass `changed_files` runs, so the
    // untracked sets match without a status walk.
    let listed = git(repo, &["ls-files", "--cached", "--others", "--exclude-standard", "-z"])?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in listed.split('\0').filter(|s| !s.is_empty()) {
        if seen.insert(path.to_string()) {
            out.push(WorktreeEntry { path: path.to_string(), ignored: false, is_dir: false });
        }
    }
    for (path, is_dir) in ignored_entries(repo)? {
        if seen.insert(path.clone()) {
            out.push(WorktreeEntry { path, ignored: true, is_dir });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The ignored entries: a wholly-ignored directory comes back as `dir/` (mapped to
/// `is_dir = true`), an individually-ignored file as itself.
///
/// `ls-files --directory` prunes at each ignored directory instead of walking inside it, where
/// `git status --ignored` enumerates the whole tree — seconds against a large `node_modules`.
/// `--no-empty-directory` matches `status`'s output exactly, which skips empty ignored dirs.
fn ignored_entries(repo: &Path) -> Result<Vec<(String, bool)>> {
    let out = git(
        repo,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
            "-z",
        ],
    )?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|path| match path.strip_suffix('/') {
            Some(dir) => (dir.to_string(), true),
            None => (path.to_string(), false),
        })
        .collect())
}

/// The immediate children of a wholly-ignored directory, for lazy expansion in `All files`
/// (specs/file-list.md). Everything under an ignored directory is ignored, so this reads the
/// filesystem directly; sub-directories come back as `is_dir` placeholders to expand in turn.
/// An unreadable directory yields no children rather than failing the reload, so expansion is
/// best-effort.
pub fn list_ignored_dir(repo: &Path, dir: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(repo.join(dir)) else { return out };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        out.push(WorktreeEntry { path: format!("{dir}/{name}"), ignored: true, is_dir });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Build the sorted `ChangedFile` list from `git diff` numstat + name-status output,
/// optionally appending untracked files (which a `git diff` never reports).
fn assemble(
    repo: &Path,
    numstat: &str,
    name_status: &str,
    include_untracked: bool,
) -> Result<Vec<ChangedFile>> {
    let counts = parse_numstat(numstat);
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for (kind, path, previous_path) in parse_name_status(name_status) {
        if !seen.insert(path.clone()) {
            continue;
        }
        let (additions, deletions) = counts.get(&path).copied().unwrap_or((0, 0));
        files.push(ChangedFile { path, kind, additions, deletions, previous_path });
    }

    if include_untracked {
        // Untracked-not-ignored files list as additions. One `ls-files --others` pass — the
        // same definition of untracked `all_files` uses, so the two views can't disagree.
        // `-z` keeps paths with spaces or special characters verbatim, and files inside a
        // brand-new directory list individually (.gitignore still applies).
        let others = git(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        for path in others.split('\0').filter(|s| !s.is_empty()) {
            let path = path.to_string();
            if seen.insert(path.clone()) {
                let additions = untracked_additions(repo, &path);
                files.push(ChangedFile {
                    path,
                    kind: ChangeKind::Untracked,
                    additions,
                    deletions: 0,
                    previous_path: None,
                });
            }
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Addition count of an untracked file: its line count, which is what `git diff` against
/// nothing reports (0 for empty or binary). Read locally rather than shelling
/// `git diff --no-index` per file — with `--untracked-files=all` a large untracked tree
/// would otherwise fork git once per file on every poll and freeze the UI.
fn untracked_additions(repo: &Path, path: &str) -> u32 {
    let Ok(bytes) = std::fs::read(repo.join(path)) else { return 0 };
    if bytes.is_empty() || bytes.contains(&0) {
        return 0; // empty, or binary (a NUL byte) — git reports no line additions
    }
    // Lines = newline count, plus one for a final line with no trailing newline. A plain
    // byte count is fine for one already-read file; no need for the bytecount crate.
    #[allow(clippy::naive_bytecount)]
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let trailing = usize::from(bytes.last() != Some(&b'\n'));
    (newlines + trailing) as u32
}

// --- pure parsers (unit-tested without a repo) ---------------------------------

/// Map of new-path to `(additions, deletions)` from `git diff --numstat -z`.
///
/// Under `-z` a non-rename record is `ADDS\tDELS\tPATH\0`; a rename/copy record is
/// `ADDS\tDELS\t\0OLD\0NEW\0` — the counts ride the front, then old and new arrive as
/// their own NUL fields (no `=>` arrow, no brace factoring). Binary files emit `-`/`-`,
/// which parse to 0. The counts key under the new path, matching `parse_name_status`.
fn parse_numstat(out: &str) -> HashMap<String, (u32, u32)> {
    let mut map = HashMap::new();
    let mut it = out.split('\0');
    while let Some(field) = it.next() {
        // `splitn(3)` keeps any tabs inside the path (verbatim under `-z`) intact.
        let mut parts = field.splitn(3, '\t');
        let add = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let del = parts.next().unwrap_or("0").parse().unwrap_or(0);
        match parts.next() {
            // Non-rename: the path rode this same field.
            Some(path) if !path.is_empty() => {
                map.insert(path.to_string(), (add, del));
            }
            // Rename/copy: the next two fields are the old and new paths.
            Some(_) => {
                let _old = it.next();
                if let Some(new) = it.next().filter(|n| !n.is_empty()) {
                    map.insert(new.to_string(), (add, del));
                }
            }
            // No tab fields — a trailing empty record after the final NUL.
            None => {}
        }
    }
    map
}

/// `(kind, path, previous_path)` from `git diff --name-status -z`. Under `-z` each record is
/// `STATUS\0PATH\0`, except a rename/copy is `R<score>\0OLD\0NEW\0` (status, then old and new
/// as separate fields). A rename or copy takes the new path and carries its old path; every
/// other kind has `previous_path == None`. Copy folds into `Renamed` — a copy's old content
/// lives at the old path exactly like a rename, which is what `content_sides` reads.
fn parse_name_status(out: &str) -> Vec<(ChangeKind, String, Option<String>)> {
    let mut rows = Vec::new();
    let mut it = out.split('\0');
    while let Some(status) = it.next() {
        let row = match status.chars().next() {
            Some('A') => it.next().map(|p| (ChangeKind::Added, p.to_string(), None)),
            Some('D') => it.next().map(|p| (ChangeKind::Deleted, p.to_string(), None)),
            Some('R' | 'C') => {
                let old = it.next();
                it.next().map(|new| (ChangeKind::Renamed, new.to_string(), old.map(str::to_string)))
            }
            // Modified, type-changed, etc.; also skips the trailing empty record.
            Some(_) => it.next().map(|p| (ChangeKind::Modified, p.to_string(), None)),
            None => None,
        };
        if let Some((kind, path, prev)) = row
            && !path.is_empty()
        {
            rows.push((kind, path, prev));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{
        ChangeKind, RepoTarget, RepositoryIdentity, classify_remote, parse_name_status,
        parse_numstat,
    };

    #[test]
    fn repository_identity_parses_github_and_enterprise_remote_forms() {
        let repo = |host: &str, owner: &str, name: &str| {
            RepositoryIdentity::Repository(RepoTarget::new(host, owner, name).unwrap())
        };
        // HTTPS, with and without `.git` and a trailing slash.
        assert_eq!(
            classify_remote("https://github.com/owner/repo.git", None),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("https://github.com/owner/repo", None),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("https://github.com/owner/repo/", None),
            repo("github.com", "owner", "repo")
        );
        // scp-like SSH, and the `ssh://` scheme form with a port.
        assert_eq!(
            classify_remote("git@github.com:owner/repo.git", None),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("ssh://git@github.com/owner/repo.git", None),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("ssh://git@github.com:22/owner/repo.git", None),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("git://github.com/owner/repo", None),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote(
                "https://github.company.com/owner/repo.git",
                Some("github.company.com")
            ),
            repo("github.company.com", "owner", "repo")
        );
    }

    #[test]
    fn repository_identity_rejects_aliases_and_keeps_failure_states_distinct() {
        assert_eq!(
            classify_remote("git@github.com-work:owner/repo.git", None),
            RepositoryIdentity::Unsupported("github.com-work".to_string())
        );
        assert_eq!(
            classify_remote(
                "git@github.company.com-work:owner/repo.git",
                Some("github.company.com")
            ),
            RepositoryIdentity::Unsupported("github.company.com-work".to_string())
        );
        assert_eq!(
            classify_remote("https://github.com-attacker/owner/repo", None),
            RepositoryIdentity::Unsupported("github.com-attacker".to_string())
        );
        assert_eq!(
            classify_remote(
                "https://github.company.com-work/owner/repo",
                Some("github.company.com")
            ),
            RepositoryIdentity::Unsupported("github.company.com-work".to_string())
        );
        assert_eq!(
            classify_remote("git@gitlab.com:owner/repo.git", Some("github.company.com")),
            RepositoryIdentity::Unsupported("gitlab.com".to_string())
        );
        assert_eq!(
            classify_remote("https://github.com/owner", None),
            RepositoryIdentity::Malformed("github.com".to_string())
        );
        assert_eq!(
            classify_remote("https://github.com", None),
            RepositoryIdentity::Malformed("github.com".to_string())
        );
        assert_eq!(
            classify_remote("https://gitlab.com", None),
            RepositoryIdentity::Unsupported("gitlab.com".to_string())
        );
        assert_eq!(
            classify_remote(
                "https://github.company.com:8443/owner/repo.git",
                Some("github.company.com")
            ),
            RepositoryIdentity::Unsupported("github.company.com".to_string())
        );
        assert_eq!(classify_remote("/tmp/repo", None), RepositoryIdentity::Hostless);
        assert_eq!(classify_remote("file:///tmp/repo", None), RepositoryIdentity::Hostless);
        assert_eq!(
            classify_remote("file://github.com/owner/repo", None),
            RepositoryIdentity::Unsupported("github.com".to_string())
        );
        assert_eq!(
            classify_remote("ftp://github.com/owner/repo", None),
            RepositoryIdentity::Unsupported("github.com".to_string())
        );
    }

    #[test]
    fn repository_target_enforces_its_canonical_shape() {
        let target = RepoTarget::new("GitHub.COM", "owner", "repo").unwrap();
        assert_eq!(target.host(), "github.com");
        assert_eq!(target.owner(), "owner");
        assert_eq!(target.name(), "repo");
        assert!(RepoTarget::new("bad host", "owner", "repo").is_none());
        assert!(RepoTarget::new("github.com", ".", "repo").is_none());
        assert!(RepoTarget::new("github.com", "owner/name", "repo").is_none());
        assert!(RepoTarget::new("github.com", "owner", "bad\nname").is_none());
        assert!(RepoTarget::new("github.com", "owner", "bad\u{202e}name").is_none());
    }

    #[test]
    fn numstat_parses_counts_and_ignores_binary() {
        let m = parse_numstat("18\t8\tsrc/a.rs\0-\t-\tassets/logo.png\0");
        assert_eq!(m["src/a.rs"], (18, 8));
        assert_eq!(m["assets/logo.png"], (0, 0));
    }

    #[test]
    fn numstat_keys_renames_under_the_new_path() {
        // Under `-z` a rename is `ADDS\tDELS\t\0OLD\0NEW`: old and new are their own fields,
        // no `=>` arrow or brace form. Counts must key under the new path.
        let m = parse_numstat("3\t1\t\0src/old.rs\0src/new.rs\0");
        assert_eq!(m["src/new.rs"], (3, 1));
        assert!(!m.contains_key("src/old.rs"));
    }

    #[test]
    fn numstat_dir_removing_rename_has_no_double_slash() {
        // Regression: the old brace parser produced `a//file.rs` here, so counts never matched.
        let m = parse_numstat("4\t2\t\0a/b/file.rs\0a/file.rs\0");
        assert_eq!(m["a/file.rs"], (4, 2));
        assert!(!m.contains_key("a//file.rs"));
    }

    #[test]
    fn numstat_handles_a_mixed_stream() {
        // binary, plain, rename, in sequence — the rename lookahead must stay aligned.
        // `\x00` (= NUL) is used as the separator so the digits after it read clearly.
        let m = parse_numstat("-\t-\tlogo.png\x009\t1\tsrc/a.rs\x005\t4\t\x00o.rs\x00n.rs\x00");
        assert_eq!(m["logo.png"], (0, 0));
        assert_eq!(m["src/a.rs"], (9, 1));
        assert_eq!(m["n.rs"], (5, 4));
    }

    #[test]
    fn name_status_kinds_and_rename_target() {
        let rows =
            parse_name_status("M\0src/a.rs\0A\0src/b.rs\0D\0src/c.rs\0R100\0old.rs\0new.rs\0");
        assert_eq!(rows[0], (ChangeKind::Modified, "src/a.rs".to_string(), None));
        assert_eq!(rows[1], (ChangeKind::Added, "src/b.rs".to_string(), None));
        assert_eq!(rows[2], (ChangeKind::Deleted, "src/c.rs".to_string(), None));
        assert_eq!(
            rows[3],
            (ChangeKind::Renamed, "new.rs".to_string(), Some("old.rs".to_string()))
        );
    }

    #[test]
    fn name_status_copy_keeps_the_new_path() {
        // A copy carries old + new like a rename; it must key under the new path, not collapse
        // to a Modified entry on the source path.
        let rows = parse_name_status("C75\0orig.rs\0copy.rs\0");
        assert_eq!(
            rows[0],
            (ChangeKind::Renamed, "copy.rs".to_string(), Some("orig.rs".to_string()))
        );
    }
}
