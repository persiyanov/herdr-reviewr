//! Git access; the only writes are private refs, index copies, and snapshot objects (AGENTS.md, No writes).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use anyhow::{Context, Result, bail};

use crate::model::{ChangeKind, ChangedFile};

/// Every git process reviewr runs, read-only on the real index (**No writes**).
fn git_command(repo: &Path) -> std::process::Command {
    #[cfg(test)]
    GIT_COMMANDS.with(|n| n.set(n.get() + 1));
    let mut cmd = crate::proc::command("git");
    cmd.arg("-C")
        .arg(repo)
        // Single-threaded, so a refresh's wall time bounds its CPU (the pacing budget's measure).
        .args(["-c", "diff.autoRefreshIndex=false", "-c", "core.preloadIndex=false"])
        .args(["-c", "index.threads=1"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIFF_OPTS");
    cmd
}

#[cfg(test)]
thread_local! {
    /// Git processes this thread built, for tests that pin a build's spawn budget.
    pub(crate) static GIT_COMMANDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    run(git_command(repo), args)
}

/// Run `cmd` (a [`git_command`]) with `args` and return stdout. Errors on non-zero exit.
fn run(mut cmd: std::process::Command, args: &[&str]) -> Result<String> {
    let out = cmd
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!(git_error(args, "could not run", e)))?;
    if !out.status.success() {
        bail!(git_error(args, "failed", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A failed git call's message for the reviewer; the whole argv goes to the log.
fn git_error(args: &[&str], what: &str, detail: impl std::fmt::Display) -> String {
    crate::logln!("git {args:?} {what}: {detail}");
    format!("git {} {what}: {detail}", subcommand(args))
}

/// The git subcommand an argv runs, e.g. `rev-parse`.
fn subcommand<'a>(args: &[&'a str]) -> &'a str {
    let mut rest = args.iter().copied();
    while let Some(arg) = rest.next() {
        match arg {
            // A global option that takes the next word as its value.
            "-c" | "-C" => {
                rest.next();
            }
            arg if arg.starts_with('-') => {}
            arg => return arg,
        }
    }
    ""
}

/// A git query's trimmed stdout; `None` when it fails or prints nothing.
fn git_line(repo: &Path, args: &[&str]) -> Option<String> {
    let out = git_command(repo).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Whether `git -C <repo> <args>` spawns and exits zero. The predicate workhorse for existence checks.
fn git_ok(repo: &Path, args: &[&str]) -> bool {
    git_command(repo).args(args).output().is_ok_and(|o| o.status.success())
}

/// Whether `path` is inside a git work tree.
pub fn is_repo(path: &Path) -> bool {
    git_ok(path, &["rev-parse", "--is-inside-work-tree"])
}

/// `core.editor` from any config level: the editor most Windows users set.
pub fn core_editor(repo: &Path) -> Option<String> {
    git_line(repo, &["config", "--get", "core.editor"])
}

/// `path`'s git top level, `None` for no repo or no git ([`worktree_of`] tells them apart).
pub fn toplevel(path: &Path) -> Option<PathBuf> {
    match worktree_of(path) {
        Worktree::Root(root) => Some(root),
        Worktree::Outside | Worktree::Unknown => None,
    }
}

/// A directory's git top level; `Unknown` (git could not run) is never `Outside`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Worktree {
    Root(PathBuf),
    Outside,
    Unknown,
}

/// Resolve `path` to its worktree, distinguishing the two ways resolution yields no root.
pub fn worktree_of(path: &Path) -> Worktree {
    match git_command(path).args(["rev-parse", "--show-toplevel"]).output() {
        Err(_) => Worktree::Unknown,
        Ok(out) if !out.status.success() => Worktree::Outside,
        Ok(out) => match String::from_utf8_lossy(&out.stdout).trim() {
            "" => Worktree::Outside,
            root => Worktree::Root(PathBuf::from(root)),
        },
    }
}

/// The worktree's own git dir and the common dir, canonical; the same in a main checkout.
pub fn git_dirs(repo: &Path) -> Option<(PathBuf, PathBuf)> {
    let out = git(repo, &["rev-parse", "--absolute-git-dir", "--git-common-dir"]).ok()?;
    let mut lines = out.lines();
    let dir = PathBuf::from(lines.next()?);
    let common = repo.join(lines.next()?);
    Some((dir.canonicalize().ok()?, common.canonicalize().ok()?))
}

/// The global ignore file git reads: `core.excludesFile`, else git's default under the XDG dir.
pub fn global_excludes(repo: &Path) -> Option<PathBuf> {
    if let Some(path) = git_line(repo, &["config", "--path", "core.excludesFile"]) {
        return Some(PathBuf::from(path));
    }
    // Git for Windows reads its home from `USERPROFILE` when `HOME` is unset.
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("git").join("ignore"))
}

/// Tracked files git's ignore rules would ignore: force-added under an ignored directory.
pub fn tracked_ignored(repo: &Path) -> Vec<String> {
    git(repo, &["ls-files", "-z", "--cached", "--ignored", "--exclude-standard"])
        .map(|out| nul_list(&out))
        .unwrap_or_default()
}

/// The forge a repository target belongs to, part of its identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Forge {
    /// The default carries the neutral `PR` vocabulary a forgeless state renders under.
    #[default]
    GitHub,
    GitLab,
    AzureDevOps,
}

/// The per-forge display vocabulary — the CLI, noun, and reference table in
impl Forge {
    /// The forge's display name for link labels and failure wording.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
            Self::AzureDevOps => "Azure DevOps",
        }
    }

    /// The forge's full noun: the word its users say.
    pub fn noun(self) -> &'static str {
        match self {
            Self::GitHub | Self::AzureDevOps => "pull request",
            Self::GitLab => "merge request",
        }
    }

    /// The forge's noun abbreviation: `PR` on GitHub, `MR` on GitLab.
    pub fn abbr(self) -> &'static str {
        match self {
            Self::GitHub | Self::AzureDevOps => "PR",
            Self::GitLab => "MR",
        }
    }

    /// The reference sigil before a number: `#226` on GitHub, `!42` on GitLab.
    pub fn sigil(self) -> char {
        match self {
            Self::GitHub | Self::AzureDevOps => '#',
            Self::GitLab => '!',
        }
    }

    /// The forge CLI's binary name.
    pub fn cli(self) -> &'static str {
        match self {
            Self::GitHub => "gh",
            Self::GitLab => "glab",
            Self::AzureDevOps => "az",
        }
    }
}

/// The self-hosted hostnames one validated config snapshot adds, one per forge
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForgeHosts<'a> {
    pub github: Option<&'a str>,
    pub gitlab: Option<&'a str>,
    pub azure_devops: Option<&'a str>,
}

/// A canonical forge repository target: the forge, its hostname, and the repository path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoTarget {
    forge: Forge,
    host: String,
    /// `[owner, name]`, a GitLab namespace path, or `[organization, project, repository]`.
    path: Vec<String>,
}

impl RepoTarget {
    /// Build one canonical GitHub repository target from a hostname and owner/name pair.
    #[cfg(test)]
    pub(crate) fn new(host: &str, owner: &str, name: &str) -> Option<Self> {
        Self::with_path(Forge::GitHub, host, &[owner, name])
    }

    /// Build one canonical target from a forge, hostname, and validated path segments.
    pub(crate) fn with_path(forge: Forge, host: &str, segments: &[&str]) -> Option<Self> {
        let host = host.to_ascii_lowercase();
        let valid_len = match forge {
            Forge::GitHub => segments.len() == 2,
            // A `-` segment means a pasted browse link, not a deep namespace.
            Forge::GitLab => segments.len() >= 2 && !segments.contains(&"-"),
            // Always `[organization, project, repository]`, shaped by `ado_canonicalize`.
            Forge::AzureDevOps => segments.len() == 3,
        };
        // Azure DevOps names admit spaces and non-ASCII, decoded by `ado_canonicalize`.
        let valid_component: fn(&str) -> bool = match forge {
            Forge::AzureDevOps => valid_ado_component,
            _ => valid_repository_component,
        };
        let components_ok = segments.iter().all(|part| valid_component(part));
        (crate::config::valid_host_syntax(&host) && valid_len && components_ok).then(|| Self {
            forge,
            host,
            path: segments.iter().map(|part| (*part).to_string()).collect(),
        })
    }

    /// The forge this target lives on.
    pub fn forge(&self) -> Forge {
        self.forge
    }

    /// The lowercase canonical forge hostname.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The first path segment: GitHub's owner, Azure DevOps' organization.
    pub fn owner(&self) -> &str {
        &self.path[0]
    }

    /// The last path segment — the repository name at the GitHub API boundary.
    pub fn name(&self) -> &str {
        self.path.last().expect("a target has 2+ segments")
    }

    /// The full slash-joined repository path — the GitLab project identity.
    pub fn full_path(&self) -> String {
        self.path.join("/")
    }

    /// Whether `other` is the same repository, case-insensitively; `==` stays exact for input tags.
    pub fn is(&self, other: &Self) -> bool {
        let azure_cloud =
            |host: &str| host == "dev.azure.com" || host.ends_with(".visualstudio.com");
        let same_host = self.host == other.host
            || (self.forge == Forge::AzureDevOps
                && azure_cloud(&self.host)
                && azure_cloud(&other.host));
        self.forge == other.forge
            && same_host
            && self.path.len() == other.path.len()
            && self.path.iter().zip(&other.path).all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// The second path segment: Azure DevOps' project.
    pub fn project(&self) -> &str {
        &self.path[1]
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

/// A decoded Azure DevOps segment: no path step, leading `-`, or control character reaches `az`.
fn valid_ado_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.starts_with('-')
        && !value.contains('/')
        && value.chars().all(|c| !c.is_control())
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

/// Classify one repository URL against the built-in and configured forge hosts.
fn classify_remote(url: &str, hosts: &ForgeHosts<'_>) -> RepositoryIdentity {
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
    let Some(forge) = forge_for_host(&host, hosts) else {
        return RepositoryIdentity::Unsupported(host);
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    let target = match forge {
        Forge::AzureDevOps => ado_canonicalize(&host, &segments).and_then(|(host, segments)| {
            let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
            RepoTarget::with_path(forge, &host, &segments)
        }),
        _ => RepoTarget::with_path(forge, &host, &segments),
    };
    match target {
        Some(target) => RepositoryIdentity::Repository(target),
        None => RepositoryIdentity::Malformed(host),
    }
}

/// The forge that recognizes `host`: the one authority on built-in hosts, config included.
pub(crate) fn forge_for_host(host: &str, hosts: &ForgeHosts<'_>) -> Option<Forge> {
    if host == "github.com" || hosts.github == Some(host) {
        return Some(Forge::GitHub);
    }
    if host == "gitlab.com" || hosts.gitlab == Some(host) {
        return Some(Forge::GitLab);
    }
    if host == "dev.azure.com"
        || host == "ssh.dev.azure.com"
        || host.strip_suffix(".visualstudio.com").is_some_and(|label| !label.is_empty())
        || hosts.azure_devops == Some(host)
    {
        return Some(Forge::AzureDevOps);
    }
    None
}

/// An Azure DevOps remote's one identity: canonical host and decoded `[org, project, repo]`.
fn ado_canonicalize(host: &str, segments: &[&str]) -> Option<(String, Vec<String>)> {
    // The ssh forms carry a leading `v3` marker and their own hostnames.
    let (host, segments): (String, Vec<&str>) = match host {
        "ssh.dev.azure.com" => {
            ("dev.azure.com".to_string(), segments.strip_prefix(&["v3"])?.to_vec())
        }
        "vs-ssh.visualstudio.com" => {
            let rest = segments.strip_prefix(&["v3"])?;
            let org = rest.first()?.to_ascii_lowercase();
            (format!("{org}.visualstudio.com"), rest.to_vec())
        }
        // A legacy https host names the organization; hoist it into the path.
        _ => match host.strip_suffix(".visualstudio.com") {
            Some(org) => {
                let mut with_org = vec![org];
                with_org.extend_from_slice(segments);
                (host.to_string(), with_org)
            }
            None => (host.to_string(), segments.to_vec()),
        },
    };
    let saw_git_marker = segments.contains(&"_git");
    // `DefaultCollection` is filler only where the hostname holds the organization.
    let org_host = host.ends_with(".visualstudio.com");
    let mut path: Vec<String> = segments
        .iter()
        .copied()
        .filter(|s| *s != "_git" && !(org_host && *s == "DefaultCollection"))
        .map(percent_decode)
        .collect::<Option<_>>()?;
    // `…/{org}/_git/{repo}` is the short form for a repository named after its project.
    if path.len() == 2 && saw_git_marker {
        path.push(path[1].clone());
    }
    // The organization is case-insensitive, so every casing is one target.
    if let Some(organization) = path.first_mut() {
        *organization = organization.to_ascii_lowercase();
    }
    (path.len() == 3).then_some((host, path))
}

/// Decode `%XX` escapes; `None` when one is broken or the bytes aren't UTF-8.
fn percent_decode(segment: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(segment.len());
    let mut rest = segment.bytes();
    while let Some(byte) = rest.next() {
        if byte == b'%' {
            let hex = [rest.next()?, rest.next()?];
            let hex = std::str::from_utf8(&hex).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
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

// --- PR-fetch local reads (published heads) ------------------------------------

/// A failed git command: a transient failure, never absence.
#[derive(Debug)]
pub struct GitFail(pub String);

impl std::fmt::Display for GitFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitFail {}

/// Spawn one git read under `LC_ALL=C`, since a missing remote is read from stderr text.
fn run_git(repo: &Path, args: &[&str]) -> Result<std::process::Output, GitFail> {
    git_command(repo)
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| GitFail(git_error(args, "could not run", e)))
}

/// Run git where exit 1 is a designated clean absence and anything else past 0 a failure.
fn git_tristate(repo: &Path, args: &[&str]) -> Result<Option<String>, GitFail> {
    let out = run_git(repo, args)?;
    if out.status.success() {
        return Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()));
    }
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    Err(GitFail(git_error(args, "failed", String::from_utf8_lossy(&out.stderr).trim())))
}

/// Run git where any non-zero exit is a failure and empty output is "found nothing".
fn git_strict(repo: &Path, args: &[&str]) -> Result<String, GitFail> {
    let out = run_git(repo, args)?;
    if !out.status.success() {
        return Err(GitFail(git_error(
            args,
            "failed",
            String::from_utf8_lossy(&out.stderr).trim(),
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Everything that determines one PR fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFetchInput {
    pub repository: RepositoryIdentity,
    /// The `origin` repository, the fork on a fork clone.
    pub origin_repository: Option<RepoTarget>,
    /// The locally derived pins and published heads, read in the same pass.
    pub local: PrLocalState,
}

/// The local identity one PR fetch derives: the pins, the branch, and where its work lives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrLocalState {
    /// `HEAD` pinned once, so one fetch reads one consistent local state.
    pub head_oid: Option<String>,
    /// The base pinned once, so a base moving mid-fetch never paints.
    pub base_oid: Option<String>,
    /// The checked-out branch. `None` is a detached `HEAD`: no branch, no PR story.
    pub branch: Option<String>,
    /// Every repository and branch name this branch's work was pushed to.
    pub heads: Vec<Head>,
    /// The pull request a `gh pr checkout` or `glab mr checkout` recorded.
    pub pin: Option<PrPin>,
}

/// One published head: a branch name in a forge repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub repo: RepoTarget,
    pub name: String,
}

/// A pull request number in the repository whose pull request ref the branch tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrPin {
    pub repo: RepoTarget,
    pub number: u64,
}

impl PrLocalState {
    /// The branch's pin, when it is on `forge`.
    #[must_use]
    pub fn pin_on(&self, forge: Forge) -> Option<&PrPin> {
        self.pin.as_ref().filter(|pin| pin.repo.forge() == forge)
    }

    /// The distinct head branch names, in head order — the forge lookup's query keys.
    #[must_use]
    pub fn head_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for head in &self.heads {
            if !names.contains(&head.name) {
                names.push(head.name.clone());
            }
        }
        names
    }
}

/// The pinned `HEAD` and base, and the heads from upstream, push target, and pushed frontier.
pub(crate) fn pr_local(
    repo: &Path,
    base_flag: Option<&str>,
    hosts: &ForgeHosts<'_>,
) -> Result<PrLocalState, GitFail> {
    let Some(branch) = checked_out_branch(repo)? else {
        return Ok(PrLocalState::default());
    };
    let head_oid = git_tristate(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?;
    let resolution = resolve_base(repo, base_flag)?;
    let bases = resolution.oids();
    let config = GitConfig::read(repo)?;
    let remote_list = remote_names(&config);
    let tips = remote_tips(repo, &remote_list)?;
    let mut remotes = Remotes::new(repo, &config, hosts);
    let push_remote_record = config.get(&format!("branch.{branch}.pushremote"));
    // A nameless base is recognized by a tracking tip sitting on it.
    let tracks_base_tip = |remote: &str, name: &str| {
        tips.iter().any(|tip| tip.remote == remote && tip.name == name && bases.contains(&tip.oid))
    };

    // An upstream tracking a base is no publication, unless the branch also pushes there.
    let record = match branch_record(&config, &branch) {
        Some((remote, BranchMerge::Branch(name)))
            if push_remote_record != Some(remote.as_str())
                && (resolution.recorded.contains(&name) || tracks_base_tip(&remote, &name)) =>
        {
            None
        }
        record => record,
    };

    let mut heads: Vec<Head> = Vec::new();
    let push_head = |repo: Option<RepoTarget>, name: &str, heads: &mut Vec<Head>| {
        if let Some(repo) = repo
            && !heads.iter().any(|have| have.name == name && have.repo.is(&repo))
        {
            heads.push(Head { repo, name: name.to_string() });
        }
    };
    // Where `git push` sends the branch: git's own chain, ending at `origin` when it exists.
    let push_remote = push_remote_record
        .or_else(|| config.get("remote.pushdefault"))
        .or(record.as_ref().map(|(remote, _)| remote.as_str()))
        .or_else(|| is_named_remote(&config, "origin").then_some("origin"));
    if let Some(value) = push_remote {
        push_head(remotes.resolve(value, true)?, &branch, &mut heads);
    }
    let mut pin = None;
    if let Some((remote, merge)) = &record {
        let recorded = remotes.resolve(remote, false)?;
        match merge {
            BranchMerge::Branch(name) => push_head(recorded, name, &mut heads),
            BranchMerge::Pull(forge, number) => {
                pin = recorded
                    .filter(|repo| repo.forge() == *forge)
                    .map(|repo| PrPin { repo, number: *number });
            }
            BranchMerge::Other => {}
        }
    }
    if let Some(head) = &head_oid
        && !bases.is_empty()
    {
        for (remote, name) in frontier_names(repo, &remote_list, &tips, head, &bases)? {
            push_head(remotes.resolve(&remote, false)?, &name, &mut heads);
        }
    }
    // A frontier of many refs stays bounded, so the per-name forge queries do.
    heads.truncate(8);
    Ok(PrLocalState {
        head_oid,
        base_oid: bases.into_iter().next(),
        branch: Some(branch),
        heads,
        pin,
    })
}

/// The winning base: a branch (origin then local) or any other spelling
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedBase {
    Branch { name: String, oid: String },
    Rev { spelling: String, oid: String },
}

impl ResolvedBase {
    fn branch(name: String, oid: String) -> Self {
        Self::Branch { name, oid }
    }

    fn rev(spelling: String, oid: String) -> Self {
        Self::Rev { spelling, oid }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Branch { name, .. } => name,
            Self::Rev { spelling, .. } => spelling,
        }
    }

    #[must_use]
    pub fn oid(&self) -> &str {
        match self {
            Self::Branch { oid, .. } | Self::Rev { oid, .. } => oid,
        }
    }
}

/// The base header: the winner, and any skipped choice, even when nothing resolves.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseStatus {
    pub winner: Option<ResolvedBase>,
    pub skipped: Option<String>,
}

/// One pass over the base chain: every resolved source, and every name it considered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseResolution {
    pub status: BaseStatus,
    /// The default branch this pass ran against.
    pub default: Option<String>,
    candidates: Vec<ResolvedBase>,
    recorded: Vec<String>,
}

impl BaseResolution {
    fn oids(&self) -> Vec<String> {
        self.candidates.iter().map(|c| c.oid().to_string()).collect()
    }
}

/// The base chain: `--base`, then the pick, then the default branch; unresolved sources skip.
pub fn resolve_base(repo: &Path, base_flag: Option<&str>) -> Result<BaseResolution, GitFail> {
    let mut candidates: Vec<ResolvedBase> = Vec::new();
    let mut recorded: Vec<String> = Vec::new();
    let mut skipped: Option<String> = None;
    let default = default_branch_name(repo)?;
    let push = |c: ResolvedBase, list: &mut Vec<ResolvedBase>| {
        if !list.iter().any(|x| x.oid() == c.oid()) {
            list.push(c);
        }
    };
    let record = |name: String, r: &mut Vec<String>| {
        if !name.is_empty() && !r.contains(&name) {
            r.push(name);
        }
    };
    if let Some(flag) = base_flag.filter(|b| !b.is_empty()) {
        let (hit, skip) = classify_flag(repo, flag)?;
        if let Some(c) = hit {
            record(c.name().to_string(), &mut recorded);
            push(c, &mut candidates);
        }
        if let Some(s) = skip {
            record(s.clone(), &mut recorded);
            skipped = Some(s);
        }
    }
    if let Some(pick) = read_base_pick(repo)? {
        record(pick.clone(), &mut recorded);
        match resolve_spelling(repo, &pick)? {
            Some(c) => push(c, &mut candidates),
            None if candidates.is_empty() => skipped = skipped.or(Some(pick)),
            None => {}
        }
    }
    if let Some(name) = &default {
        record(name.clone(), &mut recorded);
        if let Some(oid) = resolve_base_entry(repo, name)? {
            push(ResolvedBase::branch(name.clone(), oid), &mut candidates);
        }
    }
    let winner = candidates.first().cloned();
    Ok(BaseResolution { status: BaseStatus { winner, skipped }, default, candidates, recorded })
}

/// `origin/HEAD`'s branch, else the first existing of `init.defaultBranch`, `main`, `master`.
pub fn default_branch_name(repo: &Path) -> Result<Option<String>, GitFail> {
    if let Some(name) = origin_default_branch(repo)? {
        return Ok(Some(name));
    }
    let configured = git_tristate(repo, &["config", "--get", "init.defaultBranch"])?
        .filter(|name| is_branch_label(name));
    let names: Vec<&str> =
        configured.iter().map(String::as_str).chain(["main", "master"]).collect();
    // Exact refs from one listing: `rev-parse` would match `Main` on a case-insensitive disk.
    let patterns: Vec<String> = names
        .iter()
        .flat_map(|name| BRANCH_REF_PREFIXES.iter().map(move |prefix| format!("{prefix}{name}")))
        .collect();
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend(patterns.iter().map(String::as_str));
    let out = git_strict(repo, &args)?;
    let listed: std::collections::HashSet<&str> = out.lines().collect();
    Ok(names
        .into_iter()
        .find(|name| {
            BRANCH_REF_PREFIXES.iter().any(|p| listed.contains(format!("{p}{name}").as_str()))
        })
        .map(str::to_string))
}

/// The branch `origin/HEAD` points at, by symref or by matching tip.
fn origin_default_branch(repo: &Path) -> Result<Option<String>, GitFail> {
    let target = git_tristate(repo, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?;
    if let Some(name) =
        target.and_then(|t| t.strip_prefix("refs/remotes/origin/").map(str::to_string))
    {
        // `fetch --prune` can leave the symref dangling: no default then.
        let probe = format!("refs/remotes/origin/{name}^{{commit}}");
        let resolves = git_tristate(repo, &["rev-parse", "--verify", "--quiet", &probe])?;
        return Ok(resolves.map(|_| name));
    }
    let Some(oid) = git_tristate(
        repo,
        &["rev-parse", "--verify", "--quiet", "refs/remotes/origin/HEAD^{commit}"],
    )?
    else {
        return Ok(None);
    };
    Ok(origin_tips(repo)?.into_iter().find_map(|(tip, name)| (tip == oid).then_some(name)))
}

/// Strip the ref prefixes a `--base` branch name may carry.
pub(crate) fn strip_base_prefix(entry: &str) -> String {
    ["refs/remotes/origin/", "refs/heads/", "origin/"]
        .iter()
        .find_map(|p| entry.strip_prefix(p))
        .unwrap_or(entry)
        .to_string()
}

/// One base picker row: a bare branch name and the unix time of its tip commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchRow {
    pub name: String,
    pub tip_secs: u64,
}

/// The base picker's branches, local and origin merged by name (origin wins), newest first.
pub fn list_branches(repo: &Path) -> Result<Vec<BranchRow>, GitFail> {
    let out = git_strict(
        repo,
        &[
            "for-each-ref",
            "refs/remotes/origin",
            "refs/heads",
            "--sort=-committerdate",
            "--format=%(refname)%00%(committerdate:unix)",
        ],
    )?;
    // Origin's rows first, so origin wins by rule, not by date.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut rows: Vec<BranchRow> = Vec::new();
    for prefix in BRANCH_REF_PREFIXES {
        for line in out.lines() {
            let Some((refname, secs)) = line.split_once('\0') else { continue };
            let Some(name) = refname.strip_prefix(prefix) else { continue };
            if name == "HEAD" || !seen.insert(name) {
                continue;
            }
            rows.push(BranchRow { name: name.to_string(), tip_secs: secs.parse().unwrap_or(0) });
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.tip_secs));
    Ok(rows)
}

/// The checked-out branch's bare name, `None` when `HEAD` is detached.
pub fn checked_out_branch(repo: &Path) -> Result<Option<String>, GitFail> {
    git_tristate(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

/// The `origin` remote-tracking tips as `(OID, bare name)`, `origin/HEAD` excluded.
fn origin_tips(repo: &Path) -> Result<Vec<(String, String)>, GitFail> {
    Ok(remote_tips(repo, &["origin"])?.into_iter().map(|tip| (tip.oid, tip.name)).collect())
}

/// One remote-tracking tip: its OID, its remote, and its branch name there.
struct RemoteTip {
    oid: String,
    remote: String,
    name: String,
}

/// The tracking tips of `remotes`, given longest first so a `/` in a name splits right.
fn remote_tips(repo: &Path, remotes: &[&str]) -> Result<Vec<RemoteTip>, GitFail> {
    let out =
        git_strict(repo, &["for-each-ref", "refs/remotes", "--format=%(objectname) %(refname)"])?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let (oid, refname) = line.split_once(' ')?;
            let rest = refname.strip_prefix("refs/remotes/")?;
            let (remote, name) = remotes.iter().find_map(|remote| {
                Some((*remote, rest.strip_prefix(remote)?.strip_prefix('/')?))
            })?;
            (name != "HEAD").then(|| RemoteTip {
                oid: oid.to_string(),
                remote: remote.to_string(),
                name: name.to_string(),
            })
        })
        .collect())
}

/// The `(remote, name)` tips at the pushed frontier beyond every base, within 32 boundary commits.
fn frontier_names(
    repo: &Path,
    remotes: &[&str],
    tips: &[RemoteTip],
    head: &str,
    bases: &[String],
) -> Result<Vec<(String, String)>, GitFail> {
    if tips.is_empty() {
        // Nothing published: `--not --remotes` would walk unbounded.
        return Ok(Vec::new());
    }
    // Only configured remotes bound the walk; a removed remote's leftovers don't.
    let excluded: Vec<String> = remotes.iter().map(|r| format!("--remotes={r}")).collect();
    let mut args = vec!["rev-list", "--boundary", head, "--not"];
    args.extend(excluded.iter().map(String::as_str));
    let out = git_strict(repo, &args)?;
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
    let mut names: Vec<(String, String)> = Vec::new();
    for oid in oids {
        // The caller keeps at most 8 heads, so stop paying git calls past that.
        if names.len() >= 8 {
            break;
        }
        if !beyond_all_bases(repo, &oid, bases)? {
            continue;
        }
        for tip in tips {
            let pair = (tip.remote.clone(), tip.name.clone());
            if tip.oid == oid && !names.contains(&pair) {
                names.push(pair);
            }
        }
    }
    Ok(names)
}

/// Whether `commit` is an ancestor of (or equal to) `of`.
fn is_ancestor(repo: &Path, commit: &str, of: &str) -> Result<bool, GitFail> {
    Ok(git_tristate(repo, &["merge-base", "--is-ancestor", commit, of])?.is_some())
}

/// Whether `head` contains `commit`; an unfetched commit is not contained.
pub fn contains_commit(repo: &Path, head: &str, commit: &str) -> Result<bool, GitFail> {
    if git_tristate(repo, &["cat-file", "-e", commit])?.is_none() {
        return Ok(false);
    }
    is_ancestor(repo, commit, head)
}

/// Whether `oid` is an ancestor of no base.
fn beyond_all_bases(repo: &Path, oid: &str, bases: &[String]) -> Result<bool, GitFail> {
    for base in bases {
        if is_ancestor(repo, oid, base)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The target (`upstream`, else `origin`) and the `origin` identity; read errors never fall back.
pub(crate) fn remote_identities(
    repo: &Path,
    hosts: &ForgeHosts<'_>,
) -> Result<(RepositoryIdentity, Option<RepoTarget>), GitFail> {
    let upstream = remote_identity(repo, "upstream", hosts)?;
    let origin = remote_identity(repo, "origin", hosts);
    let origin_target = match &origin {
        Ok(RepositoryIdentity::Repository(target)) => Some(target.clone()),
        _ => None,
    };
    let repository =
        if matches!(upstream, RepositoryIdentity::Repository(_)) { upstream } else { origin? };
    Ok((repository, origin_target))
}

/// Classify one remote's fetch URL; a missing remote is clean, other failures transient.
pub(crate) fn remote_identity(
    repo: &Path,
    remote: &str,
    hosts: &ForgeHosts<'_>,
) -> Result<RepositoryIdentity, GitFail> {
    let args = ["remote", "get-url", "--", remote];
    let out = run_git(repo, &args)?;
    if out.status.success() {
        let url = std::str::from_utf8(&out.stdout)
            .map_err(|e| GitFail(git_error(&args, "returned invalid UTF-8", e)))?;
        return Ok(classify_remote(url.trim(), hosts));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.to_lowercase().contains("no such remote") {
        return Ok(RepositoryIdentity::Missing);
    }
    Err(GitFail(git_error(&args, "failed", stderr.trim())))
}

/// Peel `rev` to a commit; a leading `-` or an ambiguous SHA is a miss.
pub fn resolve_commit(repo: &Path, rev: &str) -> Result<Option<String>, GitFail> {
    if rev.is_empty() || rev.starts_with('-') {
        return Ok(None);
    }
    let probe = format!("{rev}^{{commit}}");
    git_tristate(repo, &["rev-parse", "--verify", "--quiet", &probe])
}

/// The abbreviated object id the header and the picker paint.
#[must_use]
pub fn abbreviate_oid(oid: &str) -> String {
    const N: usize = 7;
    if oid.len() <= N { oid.to_string() } else { oid[..N].to_string() }
}

/// Whether `spelling` is a hex prefix of `oid`.
#[must_use]
pub fn spelling_is_sha_prefix(spelling: &str, oid: &str) -> bool {
    let s = spelling.to_ascii_lowercase();
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_hexdigit())
        && oid.to_ascii_lowercase().starts_with(&s)
}

/// A non-branch spelling's shown name and SHA mark; a SHA prefix paints once.
#[must_use]
pub fn rev_paint(spelling: &str, oid: &str) -> (String, Option<String>) {
    let abbrev = abbreviate_oid(oid);
    if spelling_is_sha_prefix(spelling, oid) {
        (abbrev, None)
    } else {
        (spelling.to_string(), Some(abbrev))
    }
}

/// Complete a SHA prefix to the abbreviation; a longer one is kept.
#[must_use]
pub fn complete_sha_prefix(spelling: &str, oid: &str) -> String {
    let abbrev = abbreviate_oid(oid);
    if spelling_is_sha_prefix(spelling, oid) && spelling.len() < abbrev.len() {
        abbrev
    } else {
        spelling.to_string()
    }
}

/// A branch name the picker would list, not `HEAD` and not a rev-walk.
#[must_use]
pub fn is_branch_label(value: &str) -> bool {
    branch_name_shaped(value) && !value.eq_ignore_ascii_case("HEAD")
}

/// Origin then local, else a verbatim commit.
pub(crate) fn resolve_spelling(
    repo: &Path,
    spelling: &str,
) -> Result<Option<ResolvedBase>, GitFail> {
    if let Some(oid) = resolve_base_entry(repo, spelling)? {
        return Ok(Some(ResolvedBase::branch(spelling.to_string(), oid)));
    }
    Ok(resolve_commit(repo, spelling)?.map(|oid| ResolvedBase::rev(spelling.to_string(), oid)))
}

/// `--base` verbatim, else prefix-stripped as a branch.
fn classify_flag(
    repo: &Path,
    flag: &str,
) -> Result<(Option<ResolvedBase>, Option<String>), GitFail> {
    let entry = strip_base_prefix(flag);
    let verbatim = resolve_commit(repo, flag)?;
    let via_branch = resolve_base_entry(repo, &entry)?;
    Ok(match (verbatim, via_branch) {
        (Some(oid), None) => (Some(ResolvedBase::rev(flag.to_string(), oid)), None),
        (Some(oid), Some(_)) | (None, Some(oid)) => (Some(ResolvedBase::branch(entry, oid)), None),
        (None, None) => {
            let skip = if is_branch_label(&entry) { entry } else { flag.to_string() };
            (None, Some(skip))
        }
    })
}

/// Where a bare branch name is looked up; origin wins, as the PR sees it.
const BRANCH_REF_PREFIXES: [&str; 2] = ["refs/remotes/origin/", "refs/heads/"];

fn resolve_base_entry(repo: &Path, name: &str) -> Result<Option<String>, GitFail> {
    if !is_branch_label(name) {
        return Ok(None);
    }
    for prefix in BRANCH_REF_PREFIXES {
        let probe = format!("{prefix}{name}^{{commit}}");
        if let Some(oid) = git_tristate(repo, &["rev-parse", "--verify", "--quiet", &probe])? {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

/// One read of the effective config; only subsections keep their case.
struct GitConfig(Vec<(String, String)>);

impl GitConfig {
    fn read(repo: &Path) -> Result<Self, GitFail> {
        let out = git_strict(repo, &["config", "-z", "--list"])?;
        Ok(Self(
            out.split('\0')
                .filter(|entry| !entry.is_empty())
                .map(|entry| match entry.split_once('\n') {
                    Some((key, value)) => (key.to_string(), value.to_string()),
                    // A bare boolean key carries no value line.
                    None => (entry.to_string(), String::new()),
                })
                .collect(),
        ))
    }

    /// The last value of `key` — git's own precedence for a single-valued key.
    fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The first value of a multi-valued key — git's upstream is the first `merge`.
    fn first(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// What `branch.<name>.merge` names on the recorded remote.
#[derive(Debug)]
enum BranchMerge {
    /// A branch on that remote (`refs/heads/<name>`, or a bare `<name>` as git reads it).
    Branch(String),
    /// A pull request ref, as `gh pr checkout` or `glab mr checkout` records it.
    Pull(Forge, u64),
    /// Any other ref: a remote, but no branch or pull request on it.
    Other,
}

/// The branch's upstream remote and first `merge`; `None` when absent or local.
fn branch_record(config: &GitConfig, branch: &str) -> Option<(String, BranchMerge)> {
    let remote = config.get(&format!("branch.{branch}.remote"))?;
    if remote.is_empty() || remote == "." {
        return None;
    }
    let merge = config.first(&format!("branch.{branch}.merge")).unwrap_or_default();
    let pull =
        |prefix: &str| merge.strip_prefix(prefix)?.strip_suffix("/head")?.parse::<u64>().ok();
    let kind = if let Some(number) = pull("refs/pull/") {
        BranchMerge::Pull(Forge::GitHub, number)
    } else if let Some(number) = pull("refs/merge-requests/") {
        BranchMerge::Pull(Forge::GitLab, number)
    } else if let Some(name) = merge.strip_prefix("refs/heads/").filter(|name| !name.is_empty()) {
        BranchMerge::Branch(name.to_string())
    } else if !merge.is_empty() && !merge.starts_with("refs/") {
        BranchMerge::Branch(merge.to_string())
    } else {
        BranchMerge::Other
    };
    Some((remote.to_string(), kind))
}

/// git's own rule for a remote-valued setting: a configured remote name, else a URL.
fn is_named_remote(config: &GitConfig, value: &str) -> bool {
    config.get(&format!("remote.{value}.url")).is_some()
}

/// The configured remote names, longest first.
fn remote_names(config: &GitConfig) -> Vec<&str> {
    let mut names: Vec<&str> = config
        .0
        .iter()
        .filter_map(|(key, _)| key.strip_prefix("remote.")?.strip_suffix(".url"))
        .collect();
    names.sort_unstable();
    names.dedup();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names
}

/// Resolves remote-valued settings to forge repositories, once each per derivation.
struct Remotes<'a> {
    repo: &'a Path,
    config: &'a GitConfig,
    hosts: &'a ForgeHosts<'a>,
    seen: Vec<((String, bool), Option<RepoTarget>)>,
}

impl<'a> Remotes<'a> {
    fn new(repo: &'a Path, config: &'a GitConfig, hosts: &'a ForgeHosts<'a>) -> Self {
        Self { repo, config, hosts, seen: Vec::new() }
    }

    /// The forge repository a remote or URL names, with git's rewrites; push falls back to fetch.
    fn resolve(&mut self, value: &str, push: bool) -> Result<Option<RepoTarget>, GitFail> {
        let key = (value.to_string(), push);
        if let Some((_, repo)) = self.seen.iter().find(|(k, _)| *k == key) {
            return Ok(repo.clone());
        }
        let url = if push && is_named_remote(self.config, value) {
            git_strict(self.repo, &["remote", "get-url", "--push", "--", value])?
        } else {
            // A deleted remote's name prints back verbatim and names no host.
            git_strict(self.repo, &["ls-remote", "--get-url", "--", value])?
        };
        let repo = match classify_remote(url.trim(), self.hosts) {
            RepositoryIdentity::Repository(target) => Some(target),
            _ if push && is_named_remote(self.config, value) => self.resolve(value, false)?,
            _ => None,
        };
        self.seen.push((key, repo.clone()));
        Ok(repo)
    }
}

/// Commits `local` is ahead and behind `other`; `None` when `other` was never fetched.
pub fn ahead_behind_oids(
    repo: &Path,
    local: &str,
    other: &str,
) -> Result<Option<(u32, u32)>, GitFail> {
    // No `^{commit}` peel: a missing object would exit 128, not 1.
    if git_tristate(repo, &["cat-file", "-e", other])?.is_none() {
        return Ok(None);
    }
    let range = format!("{local}...{other}");
    let args = ["rev-list", "--left-right", "--count", range.as_str()];
    let out = git_strict(repo, &args)?;
    let mut it = out.split_whitespace();
    let parse = |s: Option<&str>| {
        s.and_then(|v| v.parse().ok())
            .ok_or_else(|| GitFail(git_error(&args, "returned unexpected output", out.trim())))
    };
    let ahead = parse(it.next())?;
    let behind = parse(it.next())?;
    Ok(Some((ahead, behind)))
}

/// The merge-base commit of the resolved base OID and `HEAD`
pub fn merge_base(repo: &Path, base_oid: &str) -> Option<String> {
    git_line(repo, &["merge-base", base_oid, "HEAD"])
}

// --- diff sides: both read from one full-context `git diff`, so they match what git compares.

/// One file's diff sides, or git's verdict that the change has no text diff.
#[derive(Debug, PartialEq, Eq)]
pub enum DiffSides {
    Text {
        old: String,
        new: String,
    },
    /// "Binary files … differ": binary content, or a path whose `diff` attribute is unset.
    Binary,
}

/// `path`'s sides from `old` to `new` (the worktree when `None`); a rename or copy reads `source`.
pub fn diff_sides(
    repo: &Path,
    old: &str,
    new: Option<&str>,
    path: &str,
    source: Option<&str>,
) -> Result<DiffSides> {
    // Git before 2.50 overflows twice a wider context on Windows; a longer side opens too large.
    let context = format!("-U{}", crate::diff::MAX_LINES);
    let mut args = vec![
        // An empty context line prints as a lone space, whatever the user set.
        "-c",
        "diff.suppressBlankEmpty=false",
        // A user's huge value would overflow git's hunk split the same way as the context.
        "-c",
        "diff.interHunkContext=0",
        // A path is a path, never a glob: `a[1].txt` must not match `a1.txt`.
        "--literal-pathspecs",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        // A submodule's sides are its commit lines, whatever `diff.submodule` the user set.
        "--submodule=short",
        // So a rename prints its source's deletion and its target's addition in full.
        "--no-renames",
        // Names bare and unquoted where git allows, so a section matches its path exactly.
        "--no-prefix",
        // So the one hunk is the whole file at any length reviewr shows.
        &context,
        old,
    ];
    args.extend(new);
    args.push("--");
    args.extend(source);
    args.push(path);
    let out = git(repo, &args)?;
    let sections = sections(&out);
    // A pathspec matches below a directory of the same name too, so each side is its own section.
    let side = |name: &str, new_side: bool| -> Side {
        let quoted = quote_path(name);
        let holds = |s: &&Section<'_>| if new_side { s.adds() } else { s.removes() };
        match sections.iter().filter(holds).find(|s| s.names(&quoted)).map(|s| parse_sides(s.body))
        {
            None | Some(None) => Side::Absent,
            Some(Some(DiffSides::Binary)) => Side::Binary,
            Some(Some(DiffSides::Text { old, new })) => {
                Side::Text(if new_side { new } else { old })
            }
        }
    };
    // A rename's old side is its source's, never what stands at its path now.
    let (old_side, new_side) = (side(source.unwrap_or(path), false), side(path, true));
    let quoted = quote_path(path);
    let own = sections.iter().find(|s| s.is_for(&quoted));
    Ok(match (old_side, new_side, source) {
        (Side::Binary, _, _) | (_, Side::Binary, _) => DiffSides::Binary,
        // An empty file added or deleted prints no hunk, so neither side holds text.
        (Side::Absent, Side::Absent, None) if own.is_some_and(Section::adds_or_deletes) => {
            DiffSides::Text { old: String::new(), new: String::new() }
        }
        // Only its mode changed: one text both sides, which must read.
        (Side::Absent, Side::Absent, None) if own.is_some() => {
            let text = git(repo, &["show", &format!("{}:{path}", new.unwrap_or(old))])?;
            DiffSides::Text { old: text.clone(), new: text }
        }
        // git found it the same at both ends, so where `show` misses it, both ends lack it.
        (Side::Absent, Side::Absent, None) => {
            let shown = git(repo, &["show", &format!("{}:{path}", new.unwrap_or(old))]);
            let text = shown.unwrap_or_default();
            DiffSides::Text { old: text.clone(), new: text }
        }
        (old_side, new_side, source) => DiffSides::Text {
            old: match (old_side, source) {
                (Side::Text(text), _) => text,
                // A copy's source is unchanged, so git printed nothing for it; it must exist.
                (Side::Absent, Some(source)) => git(repo, &["show", &format!("{old}:{source}")])?,
                _ => String::new(),
            },
            new: match new_side {
                Side::Text(text) => text,
                _ => String::new(),
            },
        },
    })
}

/// One side of a file as `git diff` printed it.
enum Side {
    /// No section names the file on this side with a hunk: unchanged, a mode change, an empty file.
    Absent,
    Text(String),
    Binary,
}

/// One file's section of a `--no-prefix` `git diff`.
struct Section<'a> {
    body: &'a str,
}

impl Section<'_> {
    /// The lines before the first hunk, where no body line can pass for a header.
    fn header(&self) -> impl Iterator<Item = &str> {
        self.body.lines().take_while(|l| !l.starts_with("@@ "))
    }

    /// Whether the section is `quoted`'s: its `---`/`+++` names or its binary verdict name it.
    fn names(&self, quoted: &str) -> bool {
        let name = |line: &str| line.trim_end_matches(['\n', '\t']) == quoted;
        self.header().any(|l| {
            l.strip_prefix("--- ").or_else(|| l.strip_prefix("+++ ")).is_some_and(name)
                || l.strip_prefix("Binary files ").is_some_and(|rest| {
                    rest.starts_with(&format!("{quoted} and "))
                        || rest.ends_with(&format!(" and {quoted} differ"))
                })
        })
    }

    /// Whether the section is `quoted`'s by its `diff --git` line, hunk or not (no renames).
    fn is_for(&self, quoted: &str) -> bool {
        self.body.starts_with(&format!("diff --git {quoted} {quoted}\n"))
    }

    /// Whether the section adds or deletes its file.
    fn adds_or_deletes(&self) -> bool {
        self.header().any(|l| l.starts_with("new file mode") || l.starts_with("deleted file mode"))
    }

    /// Whether the section gives the file content on the new side.
    fn adds(&self) -> bool {
        !self.header().any(|l| l == "+++ /dev/null" || l.starts_with("deleted file mode"))
    }

    /// Whether the section had the file on the old side.
    fn removes(&self) -> bool {
        !self.header().any(|l| l == "--- /dev/null" || l.starts_with("new file mode"))
    }
}

/// A `git diff` output cut at each file's `diff --git` line.
fn sections(out: &str) -> Vec<Section<'_>> {
    let mut starts: Vec<usize> = out.match_indices("diff --git ").map(|(i, _)| i).collect();
    starts.retain(|&i| i == 0 || out.as_bytes()[i - 1] == b'\n');
    starts
        .iter()
        .enumerate()
        .map(|(k, &i)| Section { body: &out[i..*starts.get(k + 1).unwrap_or(&out.len())] })
        .collect()
}

/// `path` as git spells it in a header under `core.quotePath=false`: C-quoted only when it must.
fn quote_path(path: &str) -> String {
    if !path.chars().any(|c| c == '"' || c == '\\' || c.is_ascii_control()) {
        return path.to_string();
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\{:03o}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A hunk header `@@ -l,s +l,s @@`: each side's (first line, count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HunkHeader {
    pub(crate) old: (u32, u32),
    pub(crate) new: (u32, u32),
}

impl HunkHeader {
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let range = |r: &str| -> Option<(u32, u32)> {
            match r.split_once(',') {
                Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
                None => Some((r.parse().ok()?, 1)),
            }
        };
        let mut parts = line.strip_prefix("@@ ")?.split_whitespace();
        let old = range(parts.next()?.strip_prefix('-')?)?;
        let new = range(parts.next()?.strip_prefix('+')?)?;
        Some(Self { old, new })
    }
}

/// The sides a full-context `git diff` spells, `None` when it printed no hunk.
fn parse_sides(out: &str) -> Option<DiffSides> {
    let (mut old, mut new) = (String::new(), String::new());
    let mut hunks = false;
    // Lines the open hunk still holds on each side.
    let (mut old_left, mut new_left) = (0u32, 0u32);
    // The side or sides the last body line went to: (old, new).
    let mut last = (false, false);
    for line in out.split_inclusive('\n') {
        if line.starts_with('\\') {
            for (side, took) in [(&mut old, last.0), (&mut new, last.1)] {
                if took && side.ends_with('\n') {
                    side.pop();
                }
            }
            continue;
        }
        if old_left + new_left > 0 {
            let (tag, body) = line.split_at(line.len().min(1));
            last = match tag {
                "-" => (true, false),
                "+" => (false, true),
                // A context line; a bare newline is an empty one.
                _ => (true, true),
            };
            let body = if tag == "\n" { line } else { body };
            if last.0 {
                old.push_str(body);
                old_left = old_left.saturating_sub(1);
            }
            if last.1 {
                new.push_str(body);
                new_left = new_left.saturating_sub(1);
            }
        } else if line.starts_with("@@ ") {
            let hunk = HunkHeader::parse(line)?;
            (old_left, new_left) = (hunk.old.1, hunk.new.1);
            hunks = true;
        } else if line.starts_with("Binary files ") {
            return Some(DiffSides::Binary);
        }
    }
    hunks.then_some(DiffSides::Text { old, new })
}

// --- base pick: one spelling per worktree, a blob under a worktree-private ref.

const BASE_PICK_REF: &str = "refs/worktree/reviewr/base-pick";
const TURN_BASE_REF: &str = "refs/worktree/reviewr/turn-base";

/// What this process last wrote to each private ref (`None`: deleted), so the watcher drops its own writes.
static OWN_REF_WRITES: Mutex<Vec<(PathBuf, &str, Option<String>)>> = Mutex::new(Vec::new());

/// Note this process's write to the private ref `name` of `repo`, keyed by its canonical git dir.
fn record_own_ref(repo: &Path, name: &'static str, value: Option<&str>) {
    let Ok(dir) = git_dir(repo) else { return };
    let dir = dir.canonicalize().unwrap_or(dir);
    let mut writes = OWN_REF_WRITES.lock().unwrap_or_else(PoisonError::into_inner);
    writes.retain(|(d, written, _)| !(*d == dir && *written == name));
    writes.push((dir, name, value.map(str::to_string)));
}

/// Whether `value` (`None`: the ref is gone) is what this process last wrote to the private ref
/// `name` in the canonical git dir `dir`.
pub(crate) fn is_own_ref_write(dir: &Path, name: &str, value: Option<&str>) -> bool {
    let writes = OWN_REF_WRITES.lock().unwrap_or_else(PoisonError::into_inner);
    writes.iter().any(|(d, written, last)| d == dir && *written == name && last.as_deref() == value)
}

/// The recorded pick's spelling in one git call, `None` when none is recorded or readable.
pub fn read_base_pick(repo: &Path) -> Result<Option<String>, GitFail> {
    let out = run_git(repo, &["cat-file", "blob", BASE_PICK_REF])?;
    if !out.status.success() {
        return Ok(None);
    }
    let name = String::from_utf8_lossy(&out.stdout);
    let name = name.trim();
    Ok(pick_spelling_shaped(name).then(|| name.to_string()))
}

/// One printable line, not a git option.
fn pick_spelling_shaped(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.bytes().all(|byte| byte > b' ' && byte != 0x7f)
}

/// A branch name the origin-then-local walk accepts; never `HEAD` or a rev-walk spelling.
fn branch_name_shaped(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains(['~', '^', ':', '?', '*', '[', '\\'])
        && value.bytes().all(|byte| byte > b' ' && byte != 0x7f)
}

/// Record `name` as this worktree's pick; the default branch deletes the pick instead.
pub fn write_base_pick(repo: &Path, name: &str) -> Result<(), GitFail> {
    // Read here, never from the picker's rows, which a fetch can leave stale.
    if Some(name) == default_branch_name(repo)?.as_deref() {
        return delete_base_pick(repo);
    }
    let blob = git_stdin(repo, &["hash-object", "-w", "--stdin"], name)?;
    record_own_ref(repo, BASE_PICK_REF, Some(blob.trim()));
    git_strict(repo, &["update-ref", BASE_PICK_REF, blob.trim()])?;
    Ok(())
}

/// Forget this worktree's pick; idempotent.
pub fn delete_base_pick(repo: &Path) -> Result<(), GitFail> {
    record_own_ref(repo, BASE_PICK_REF, None);
    git_strict(repo, &["update-ref", "-d", BASE_PICK_REF])?;
    Ok(())
}

/// Run git with `input` on stdin; any non-zero exit is a failure.
fn git_stdin(repo: &Path, args: &[&str], input: &str) -> Result<String, GitFail> {
    let out = git_stdin_output(repo, args, input)?;
    if !out.status.success() {
        return Err(GitFail(git_error(
            args,
            "failed",
            String::from_utf8_lossy(&out.stderr).trim(),
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Which of `paths` (relative to `repo`) git ignores, in one `check-ignore`; `None` when it failed.
pub(crate) fn check_ignore<'a>(
    repo: &Path,
    paths: impl Iterator<Item = &'a str>,
) -> Option<HashSet<String>> {
    // Each line is a pathspec, and `check-ignore` takes no `literal` magic: `./` keeps `:!x` itself.
    let input = paths.fold(String::new(), |mut input, p| {
        input.push_str("./");
        input.push_str(p);
        input.push('\0');
        input
    });
    let args = ["check-ignore", "--stdin", "-z"];
    let out = git_stdin_output(repo, &args, &input).ok()?;
    // 0: some are ignored; 1: none are.
    if out.status.code().is_none_or(|c| c > 1) {
        git_error(&args, "failed", String::from_utf8_lossy(&out.stderr).trim());
        return None;
    }
    let ignored = String::from_utf8_lossy(&out.stdout);
    Some(ignored.split('\0').filter_map(|p| p.strip_prefix("./")).map(str::to_string).collect())
}

/// Run git with `input` on stdin, written from its own thread so a full pipe can't deadlock.
fn git_stdin_output(
    repo: &Path,
    args: &[&str],
    input: &str,
) -> Result<std::process::Output, GitFail> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = git_command(repo)
        .env("LC_ALL", "C")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GitFail(git_error(args, "could not run", e)))?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let owned = input.to_string();
    // git may exit before reading it all; the exit status decides.
    let writer = std::thread::spawn(move || drop(stdin.write_all(owned.as_bytes())));
    let out = child.wait_with_output().map_err(|e| GitFail(git_error(args, "could not run", e)))?;
    let _ = writer.join();
    Ok(out)
}

// --- turn baseline (last-turn scope) -------------------------------------------

/// The worktree as a tree object, via `add -A` on a private [`IndexCopy`].
pub fn snapshot_worktree(repo: &Path) -> Result<String> {
    // A fresh copy each time: `add -A` stages into it, and a killed git leaves its lock behind.
    let mut index = IndexCopy::new(repo)?;
    // Seeded from the session copy's file, never its lock, so `add -A` hashes only what changed.
    let session = session(repo)?;
    let copied = session.copy_file.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let seed = copied.filter(|copy| copy.exists()).unwrap_or_else(|| session.git_dir.join("index"));
    index.seed(&seed)?;
    index.git(repo, &["add", "-A"])?;
    Ok(index.git(repo, &["write-tree"])?.trim().to_string())
}

/// The tree `seed` with `paths` re-read from the worktree: a full snapshot's tree when nothing
/// outside `paths` changed since `seed`. `None` when the files under `paths` pass the byte cap.
pub fn snapshot_worktree_in(repo: &Path, seed: &str, paths: &[String]) -> Result<Option<String>> {
    let index = IndexCopy::new(repo)?;
    index.git(repo, &["read-tree", seed])?;
    // `add` refuses a pathspec that matches nothing, so each names a file the index or the
    // worktree holds; an ignored file has nothing to stage.
    let specs = literal_pathspecs(paths);
    let args = vec!["ls-files", "-z", "--cached", "--others", "--exclude-standard"];
    let files = nul_list(&index.git(repo, &with_pathspecs(args, &specs))?);
    if !fits_command_line(&files) {
        return Ok(None);
    }
    if !files.is_empty() {
        index.git(repo, &with_pathspecs(vec!["add", "-A"], &literal_pathspecs(&files)))?;
    }
    Ok(Some(index.git(repo, &["write-tree"])?.trim().to_string()))
}

/// A cheap fingerprint of what the index stages, read from the index alone.
pub fn staged_fingerprint(repo: &Path) -> Result<u64> {
    use std::hash::{Hash, Hasher};
    let staged = git(repo, &["ls-files", "--stage", "-z"])?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    staged.hash(&mut hasher);
    Ok(hasher.finish())
}

/// The most pathspec bytes one git call takes: Windows caps a command line at 32,767 characters.
const PATHSPEC_BYTES: usize = 16 * 1024;

/// Whether `paths`, as literal pathspecs, fit [`PATHSPEC_BYTES`].
pub fn fits_command_line<'a>(paths: impl IntoIterator<Item = &'a String>) -> bool {
    paths.into_iter().map(|p| p.len() + LITERAL.len() + 1).sum::<usize>() <= PATHSPEC_BYTES
}

/// A `-z` listing's entries.
fn nul_list(out: &str) -> Vec<String> {
    out.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect()
}

/// The magic that makes a pathspec match only its own name: no globs, no leading `:` magic.
const LITERAL: &str = ":(literal)";

/// Each path as a literal pathspec; a directory still matches everything under it.
fn literal_pathspecs(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| format!("{LITERAL}{p}")).collect()
}

/// `args`, then `--` and `specs`.
fn with_pathspecs<'a>(mut args: Vec<&'a str>, specs: &'a [String]) -> Vec<&'a str> {
    args.push("--");
    args.extend(specs.iter().map(String::as_str));
    args
}

/// Whether `path` is one of `paths` or lies under one of them, by whole components.
pub fn covered<'a>(paths: impl IntoIterator<Item = &'a String>, path: &str) -> bool {
    paths.into_iter().any(|p| under_or_eq(path, p))
}

/// Whether `path` is `dir` or lies under it, by whole components.
pub fn under_or_eq(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The prefix of every index copy's directory under [`copies_dir`].
const COPY_PREFIX: &str = "index-";

/// Where `repo`'s index copies live: its own git dir, on disk beside the index they copy.
fn copies_dir(repo: &Path) -> Result<PathBuf> {
    Ok(git_dir(repo)?.join("reviewr"))
}

/// A private index copy under the git dir, never the real index; its `lock` marks it live.
struct IndexCopy {
    /// The lock that marks this copy live, released on drop (before the dir) or with the process.
    _live: std::fs::File,
    dir: tempfile::TempDir,
    /// The real index's stamp at the last copy.
    seeded: Option<Stamp>,
}

impl IndexCopy {
    /// Run `f` on `repo`'s session-long diff copy, re-copied only when the real index changed.
    fn with<T>(repo: &Path, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        let session = session(repo)?;
        let real = session.git_dir.join("index");
        // A panic mid-use leaves the copy suspect, so a poisoned slot starts over.
        let mut kept = session.copy.lock().unwrap_or_else(|poisoned| {
            session.copy.clear_poison();
            let mut kept = poisoned.into_inner();
            *kept = None;
            kept
        });
        // A worktree removed and re-added, or a re-clone, took the copy's dir with its git dir.
        if kept.as_ref().is_none_or(|copy| !copy.dir.path().exists()) {
            let copy = Self::new(repo)?;
            *session.copy_file.lock().unwrap_or_else(PoisonError::into_inner) = Some(copy.path());
            *kept = Some(copy);
        }
        let copy = kept.as_mut().expect("filled above");
        copy.seed(&real)?;
        f(copy)
    }

    /// A new, empty copy of `repo`'s index.
    fn new(repo: &Path) -> Result<Self> {
        let home = copies_dir(repo)?;
        // Never `create_dir_all`: a pruned git dir must stay gone, not come back holding copies.
        match std::fs::create_dir(&home) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(e).context("creating the index copies' dir");
            }
            _ => {}
        }
        let dir = tempfile::Builder::new()
            .prefix(COPY_PREFIX)
            .tempdir_in(home)
            .context("creating the index copy")?;
        // Locked before it is named `lock`, so a sweep never takes a live copy.
        let pending = dir.path().join("lock.new");
        let live = std::fs::File::create(&pending).context("creating the copy's lock")?;
        live.lock().context("locking the index copy")?;
        std::fs::rename(&pending, dir.path().join("lock")).context("placing the copy's lock")?;
        Ok(Self { dir, _live: live, seeded: None })
    }

    /// Copy the index at `real` again if it changed since the last copy, keeping its mtime.
    fn seed(&mut self, real: &Path) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut from = match std::fs::File::open(real) {
            Ok(file) => file,
            // A fresh repository has no index yet, and git reads a missing one as empty.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_file(self.path());
                self.seeded = None;
                return Ok(());
            }
            // Any other failure is no answer: an empty index would list every file deleted.
            Err(e) => return Err(e).context("reading the index"),
        };
        // The index's trailing checksum names its content even within one mtime tick.
        let meta = from.metadata().context("reading the index")?;
        let mut tail = vec![0; usize::try_from(meta.len().min(32)).unwrap_or(0)];
        from.seek(SeekFrom::End(-(tail.len() as i64))).context("reading the index")?;
        from.read_exact(&mut tail).context("reading the index")?;
        let stamp = Stamp { modified: meta.modified().context("reading the index")?, tail };
        if self.seeded.as_ref() == Some(&stamp) {
            return Ok(());
        }
        from.rewind().context("reading the index")?;
        // Written beside and renamed over, so a snapshot copying it unlocked never reads half a file.
        let pending = self.dir.path().join("index.new");
        let mut to = std::fs::File::create(&pending).context("creating the index copy")?;
        std::io::copy(&mut from, &mut to).context("copying the index")?;
        // The copy keeps the index's mtime, which git's racy-clean check reads.
        let _ = to.set_modified(stamp.modified);
        drop(to);
        std::fs::rename(&pending, self.path()).context("placing the index copy")?;
        self.seeded = Some(stamp);
        Ok(())
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("index")
    }

    /// Like [`git`], on the copy, with the diff refresh on.
    fn git(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let mut cmd = git_command(repo);
        // Unsplit, so no write lands a shared index beside the real one.
        cmd.args(["-c", "diff.autoRefreshIndex=true", "-c", "core.splitIndex=false"])
            .env("GIT_INDEX_FILE", self.path());
        run(cmd, args)
    }
}

/// What identifies one version of the real index: its mtime and its trailing checksum.
#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    modified: std::time::SystemTime,
    tail: Vec<u8>,
}

/// Remove `repo`'s copies whose `lock` is free, or that never got one within an hour: their
/// process died.
pub fn sweep_dead_copies(repo: &Path) {
    let Ok(home) = copies_dir(repo) else { return };
    let Ok(entries) = std::fs::read_dir(home) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(COPY_PREFIX) {
            continue;
        }
        let dir = entry.path();
        let dead = match std::fs::File::open(dir.join("lock")) {
            Ok(lock) => lock.try_lock().is_ok(),
            Err(_) => entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|at| at.elapsed().is_ok_and(|age| age.as_secs() > 3600)),
        };
        if dead {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// `repo`'s git dir, asked once per worktree: it is fixed for the session.
fn git_dir(repo: &Path) -> Result<PathBuf> {
    Ok(session(repo)?.git_dir.clone())
}

/// What reviewr keeps about one worktree for the session: its git dir, its index copy, its last
/// build's untracked counts, and the blob sizes it has asked for.
#[derive(Default)]
struct RepoSession {
    /// What `<repo>/.git` held when the git dir was read: a linked worktree's pointer, else `None`.
    pointer: Option<String>,
    git_dir: PathBuf,
    copy: Mutex<Option<IndexCopy>>,
    /// Where the session copy's index file lives, so a snapshot seeds from it without its lock.
    copy_file: Mutex<Option<PathBuf>>,
    counts: Mutex<Arc<Counts>>,
    sizes: Mutex<HashMap<String, u64>>,
}

/// Every live session by worktree.
static SESSIONS: OnceLock<Mutex<HashMap<PathBuf, Arc<RepoSession>>>> = OnceLock::new();

/// `repo`'s session, made on first use and again when `.git` points elsewhere (a re-added worktree).
fn session(repo: &Path) -> Result<Arc<RepoSession>> {
    let sessions = SESSIONS.get_or_init(Mutex::default);
    let pointer = std::fs::read_to_string(repo.join(".git")).ok();
    let current = |s: &&Arc<RepoSession>| s.pointer == pointer;
    if let Some(found) =
        sessions.lock().unwrap_or_else(PoisonError::into_inner).get(repo).filter(current)
    {
        return Ok(Arc::clone(found));
    }
    let git_dir = PathBuf::from(git(repo, &["rev-parse", "--absolute-git-dir"])?.trim());
    let mut sessions = sessions.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = sessions.get(repo).filter(current) {
        return Ok(Arc::clone(found));
    }
    let made = RepoSession { pointer, git_dir, ..RepoSession::default() };
    let made = Arc::new(made);
    sessions.insert(repo.to_path_buf(), Arc::clone(&made));
    Ok(made)
}

/// Drop every session, so each index copy no other thread holds leaves the git dir now.
pub fn end_sessions() {
    if let Some(sessions) = SESSIONS.get() {
        sessions.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

/// The persisted turn baseline tree for this worktree, if a baseline exists.
pub fn read_baseline_ref(repo: &Path) -> Option<String> {
    git_line(repo, &["rev-parse", "--verify", "--quiet", TURN_BASE_REF])
}

/// Forget the turn baseline: a turn whose start raced its first write leaves `last-turn` empty.
pub fn delete_baseline_ref(repo: &Path) -> Result<()> {
    record_own_ref(repo, TURN_BASE_REF, None);
    git(repo, &["update-ref", "-d", TURN_BASE_REF])?;
    Ok(())
}

/// Persist the turn baseline atomically under this worktree's private ref.
pub fn write_baseline_ref(repo: &Path, sha: &str) -> Result<()> {
    record_own_ref(repo, TURN_BASE_REF, Some(sha));
    git(repo, &["update-ref", TURN_BASE_REF, sha])?;
    Ok(())
}

/// git's well-known empty-tree object, used as the diff base when a repo has no commits.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The old end of `uncommitted`: `head`, the [`head_oid`] read, else the empty tree.
#[must_use]
pub fn diff_base(head: Option<String>) -> String {
    head.unwrap_or_else(|| EMPTY_TREE.to_string())
}

/// The changeset from `base` to the worktree, untracked files included.
pub fn changed_from(repo: &Path, base: &str) -> Result<Vec<ChangedFile>> {
    let out = IndexCopy::with(repo, |index| index.git(repo, &diff_args(&[base])))?;
    assemble(repo, &out, true, None)
}

/// [`changed_from`] limited to `paths`, each a file or a directory.
pub fn changed_from_in(repo: &Path, base: &str, paths: &[String]) -> Result<Vec<ChangedFile>> {
    let specs = literal_pathspecs(paths);
    let args = with_pathspecs(diff_args(&[base]), &specs);
    let out = IndexCopy::with(repo, |index| index.git(repo, &args))?;
    assemble(repo, &out, true, Some(paths))
}

/// The changeset between two trees: `commits`, and `last-turn` against its snapshot.
pub fn changed_between(repo: &Path, old: &str, new: &str) -> Result<Vec<ChangedFile>> {
    assemble(repo, &git(repo, &diff_args(&[old, new]))?, false, None)
}

/// [`changed_between`] limited to `paths`.
pub fn changed_between_in(
    repo: &Path,
    old: &str,
    new: &str,
    paths: &[String],
) -> Result<Vec<ChangedFile>> {
    let specs = literal_pathspecs(paths);
    let out = git(repo, &with_pathspecs(diff_args(&[old, new]), &specs))?;
    assemble(repo, &out, false, Some(paths))
}

/// One `git diff` per changeset: raw records, then line counts.
fn diff_args<'a>(ends: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["diff"];
    args.extend(ends);
    args.extend(["--raw", "--numstat", "--no-abbrev", "-z"]);
    args
}

/// `sha`'s first parent, or the empty tree for a root; a missing parent is named, not taken for a root.
pub fn parent_or_empty(repo: &Path, sha: &str) -> Option<String> {
    let object = git(repo, &["cat-file", "-p", &format!("{sha}^{{commit}}")]).ok()?;
    let parent = object
        .lines()
        .take_while(|l| !l.is_empty())
        .find_map(|l| l.strip_prefix("parent "))
        .map_or(EMPTY_TREE, str::trim);
    Some(parent.to_string())
}

/// `HEAD`'s commit, `None` while unborn.
pub fn head_oid(repo: &Path) -> Option<String> {
    git_line(repo, &["rev-parse", "--verify", "-q", "HEAD"])
}

/// `sha`'s subject line, for the header paint.
pub fn commit_subject(repo: &Path, sha: &str) -> Option<String> {
    git_line(repo, &["log", "-1", "--format=%s", sha])
}

/// Whether `sha` names a commit the repository still holds (`gone`).
pub fn commit_exists(repo: &Path, sha: &str) -> bool {
    git_ok(repo, &["cat-file", "-e", &format!("{sha}^{{commit}}")])
}

/// Whether `sha` is reachable from `HEAD`; a missing commit is not.
pub fn is_reachable(repo: &Path, sha: &str) -> bool {
    git_ok(repo, &["merge-base", "--is-ancestor", sha, "HEAD"])
}

/// Commits in `oldest..=newest` along `newest`'s first parents, `None` when not a run.
pub fn run_length(repo: &Path, oldest: &str, newest: &str) -> Option<usize> {
    let old = parent_or_empty(repo, oldest)?;
    run_length_from(repo, &old, oldest, newest)
}

/// [`run_length`] with `oldest`'s parent already resolved.
pub fn run_length_from(repo: &Path, old: &str, oldest: &str, newest: &str) -> Option<usize> {
    if oldest == newest {
        return Some(1);
    }
    let mut args = vec!["rev-list", "--count", "--first-parent", newest];
    let exclude;
    if old != EMPTY_TREE {
        if !git_ok(repo, &["merge-base", "--is-ancestor", oldest, newest]) {
            return None;
        }
        exclude = format!("^{old}");
        args.push(&exclude);
    }
    git_line(repo, &args)?.parse().ok()
}

/// One commit picker row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommitRow {
    pub sha: String,
    pub subject: String,
    pub time: u64,
    pub author: String,
    /// The refs pointing at the commit, `HEAD` and the checked-out branch dropped.
    pub refs: Vec<CommitRef>,
    pub merge: bool,
}

/// A ref a picker row shows, ranked by kind, not by spelling.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommitRef {
    /// A remote-tracking tip, shown as `origin/feature`.
    Remote(String),
    /// A tag, shown as `tag: v1`.
    Tag(String),
    /// A local branch other than the one checked out, shown by name.
    Branch(String),
}

impl CommitRef {
    pub fn label(&self) -> String {
        match self {
            Self::Remote(r) | Self::Branch(r) => r.clone(),
            Self::Tag(t) => format!("tag: {t}"),
        }
    }
}

/// The commit picker's first-parent rows from `HEAD`: back to `merge_base`, else 50.
pub fn list_commits(repo: &Path, merge_base: Option<&str>) -> Result<Vec<CommitRow>> {
    if head_oid(repo).is_none() {
        return Ok(Vec::new());
    }
    let range = merge_base.map(|mb| format!("{mb}..HEAD"));
    let mut args = vec![
        "log",
        "--first-parent",
        "--decorate=full",
        "--format=%H%x00%s%x00%ct%x00%an%x00%D%x00%P",
        "-z",
    ];
    match &range {
        Some(r) => args.push(r),
        None => args.extend(["-50", "HEAD"]),
    }
    let out = git(repo, &args)?;
    Ok(parse_commit_log(&out))
}

/// Parse `git log -z` with six NUL-separated fields per commit.
fn parse_commit_log(out: &str) -> Vec<CommitRow> {
    let fields: Vec<&str> = out.split('\0').collect();
    fields
        .chunks(6)
        .filter(|c| c.len() == 6 && !c[0].is_empty())
        .map(|c| CommitRow {
            sha: c[0].to_string(),
            subject: c[1].to_string(),
            time: c[2].trim().parse().unwrap_or(0),
            author: c[3].to_string(),
            refs: parse_decorations(c[4]),
            merge: c[5].split_whitespace().count() > 1,
        })
        .collect()
}

/// `%D` as typed refs, minus `HEAD` and its branch.
fn parse_decorations(d: &str) -> Vec<CommitRef> {
    d.split(", ")
        .map(str::trim)
        .filter(|r| !r.is_empty() && *r != "HEAD" && !r.starts_with("HEAD -> "))
        .filter_map(|r| {
            if let Some(t) = r.strip_prefix("tag: refs/tags/") {
                Some(CommitRef::Tag(t.to_string()))
            } else if let Some(t) = r.strip_prefix("refs/remotes/") {
                Some(CommitRef::Remote(t.to_string()))
            } else {
                r.strip_prefix("refs/heads/").map(|b| CommitRef::Branch(b.to_string()))
            }
        })
        .collect()
}

/// One `All files` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: String,
    pub ignored: bool,
    pub is_dir: bool,
}

/// Every worktree entry, sorted; an ignored directory collapses to one placeholder.
pub fn all_files(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    all_files_listed(repo, &[])
}

/// [`all_files`] limited to `paths`.
pub fn all_files_in(repo: &Path, paths: &[String]) -> Result<Vec<WorktreeEntry>> {
    all_files_listed(repo, &literal_pathspecs(paths))
}

/// The worktree's entries under `specs`, or all of them when there are none.
fn all_files_listed(repo: &Path, specs: &[String]) -> Result<Vec<WorktreeEntry>> {
    let limit = |args: Vec<&'static str>| -> Vec<&str> {
        if specs.is_empty() { args } else { with_pathspecs(args, specs) }
    };
    // Tracked and untracked in one spawn, with the same excludes `changed_from` uses.
    let listed =
        git(repo, &limit(vec!["ls-files", "--cached", "--others", "--exclude-standard", "-z"]))?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in nul_list(&listed) {
        if seen.insert(path.clone()) {
            out.push(WorktreeEntry { path, ignored: false, is_dir: false });
        }
    }
    let ignored = limit(vec![
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
        "--no-empty-directory",
        "-z",
    ]);
    for (path, is_dir) in ignored_entries(repo, &ignored)? {
        if seen.insert(path.clone()) {
            out.push(WorktreeEntry { path, ignored: true, is_dir });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The ignored entries, pruned at each ignored directory instead of walking inside it.
fn ignored_entries(repo: &Path, args: &[&str]) -> Result<Vec<(String, bool)>> {
    let out = git(repo, args)?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|path| match path.strip_suffix('/') {
            Some(dir) => (dir.to_string(), true),
            None => (path.to_string(), false),
        })
        .collect())
}

/// An ignored directory's children, read from disk; unreadable means none.
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

/// The sorted changeset from one [`diff_args`] run, untracked files appended for a worktree diff.
/// With `paths`, untracked files are listed under them only, and their counts merge into the cache.
fn assemble(
    repo: &Path,
    out: &str,
    worktree: bool,
    paths: Option<&[String]>,
) -> Result<Vec<ChangedFile>> {
    let (rows, numstat) = parse_raw(out);
    let counts = parse_numstat(numstat);
    let blobs: Vec<&str> = rows
        .iter()
        .flat_map(|row| [Some(row.old_oid.as_str()), (!worktree).then_some(row.new_oid.as_str())])
        .flatten()
        .collect();
    let sizes = blob_sizes(repo, &blobs)?;
    let size = |oid: &str| sizes.get(oid).copied().unwrap_or(0);
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for row in rows {
        if !seen.insert(row.path.clone()) {
            continue;
        }
        let verdict = counts.get(&row.path).copied().unwrap_or(Some((0, 0)));
        let (additions, deletions) = verdict.unwrap_or((0, 0));
        files.push(ChangedFile {
            kind: row.kind,
            additions,
            deletions,
            binary: verdict.is_none(),
            old_size: size(&row.old_oid),
            new_size: (!worktree).then(|| size(&row.new_oid)),
            path: row.path,
            previous_path: row.previous_path,
        });
    }

    if worktree {
        // Untracked files are additions, by the same listing `all_files` uses.
        let specs = literal_pathspecs(paths.unwrap_or_default());
        let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
        if paths.is_some() {
            args = with_pathspecs(args, &specs);
        }
        let others = git(repo, &args)?;
        let new_paths: Vec<&str> =
            others.split('\0').filter(|p| !p.is_empty() && !seen.contains(*p)).collect();
        // A failed attribute read costs the verdict, never the whole changeset.
        let undiffable = diff_unset(repo, &new_paths).unwrap_or_default();
        let mut buf = vec![0; 64 * 1024];
        // Counts carry from the last build to this one, read without taking them from a build
        // running beside this one.
        let session = session(repo)?;
        let known = Arc::clone(&session.counts.lock().unwrap_or_else(PoisonError::into_inner));
        // A path-limited build keeps every count outside its paths.
        let mut fresh: Counts = match paths {
            Some(paths) => known
                .iter()
                .filter(|((p, _), _)| !covered(paths, p))
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            None => Counts::new(),
        };
        for path in new_paths {
            let path = path.to_string();
            if !seen.insert(path.clone()) {
                continue;
            }
            // An unset `diff` attribute counts no lines, as for a tracked path.
            let additions = if undiffable.contains(path.as_str()) {
                None
            } else {
                untracked_additions(repo, &path, &mut buf, &known, &mut fresh)
            };
            let binary = additions.is_none();
            files.push(ChangedFile {
                path,
                kind: ChangeKind::Untracked,
                additions: additions.unwrap_or(0),
                deletions: 0,
                previous_path: None,
                binary,
                old_size: 0,
                new_size: None,
            });
        }
        *session.counts.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(fresh);
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Of untracked `paths`, those whose `diff` attribute is unset, in one `check-attr -z`.
fn diff_unset(repo: &Path, paths: &[&str]) -> Result<HashSet<String>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut input = String::new();
    for path in paths {
        input.push_str(path);
        input.push('\0');
    }
    let out = git_stdin(repo, &["check-attr", "-z", "--stdin", "diff"], &input)?;
    let mut fields = out.split('\0');
    let mut unset = HashSet::new();
    while let (Some(path), Some(_attr), Some(value)) = (fields.next(), fields.next(), fields.next())
    {
        if value == "unset" {
            unset.insert(path.to_string());
        }
    }
    Ok(unset)
}

/// reviewr's own read bound, at git's default `core.bigFileThreshold`: past it, binary unread.
const BIG_FILE_THRESHOLD: u64 = 512 * 1024 * 1024;

/// Line counts by repo-relative path and stat: what an untracked file held when counted.
type Counts = HashMap<(String, Stat), Option<u32>>;

/// The stat fields git's index keys a file's content on: size, mtime, and ctime where kept.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Stat {
    size: u64,
    modified: Option<std::time::SystemTime>,
    changed: Option<(i64, i64)>,
}

impl Stat {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let changed = {
            use std::os::unix::fs::MetadataExt;
            Some((meta.ctime(), meta.ctime_nsec()))
        };
        // Windows keeps no change time.
        #[cfg(not(unix))]
        let changed = None;
        Self { size: meta.len(), modified: meta.modified().ok(), changed }
    }
}

/// An untracked file's line count, `None` where git would call it binary; a count `known` from the
/// last build is reused, and every count read lands in `fresh`.
fn untracked_additions(
    repo: &Path,
    path: &str,
    buf: &mut [u8],
    known: &Counts,
    fresh: &mut Counts,
) -> Option<u32> {
    let at = repo.join(path);
    // Only a regular file has lines: a link to a device would read without end.
    let Some(meta) = std::fs::metadata(&at).ok().filter(std::fs::Metadata::is_file) else {
        return Some(0);
    };
    // Past the bound: binary, unread.
    if meta.len() > BIG_FILE_THRESHOLD {
        return None;
    }
    // A file unchanged since its last count is not read again.
    let stat = Stat::of(&meta);
    let modified = stat.modified;
    let key = (path.to_string(), stat);
    let count = match known.get(&key) {
        Some(&count) => count,
        // A failed read counts zero for now and is read again next refresh, never remembered.
        None => match count_lines(&at, buf) {
            Ok(count) => count,
            Err(_) => return Some(0),
        },
    };
    // A file written in the last two seconds could change again within its mtime's tick, as
    // git's racy-clean rule says, so its count waits to be remembered.
    let settled = modified.and_then(|m| m.elapsed().ok()).is_some_and(|age| age.as_secs() >= 2);
    if settled {
        fresh.insert(key, count);
    }
    count
}

/// `at`'s line count, `None` where git would call it binary; `Err` when it could not be read.
fn count_lines(at: &Path, buf: &mut [u8]) -> std::io::Result<Option<u32>> {
    use std::io::Read;
    let mut file = std::fs::File::open(at)?;
    // Counted a buffer at a time, so a large file never sits in memory whole.
    let (mut newlines, mut read, mut last) = (0usize, 0usize, None);
    loop {
        let n = match file.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let chunk = &buf[..n];
        // git's own sniff: a NUL within the first 8000 bytes.
        if read < 8000 && chunk[..n.min(8000 - read)].contains(&0) {
            return Ok(None); // binary — git reports no line additions
        }
        #[allow(clippy::naive_bytecount)]
        let count = chunk.iter().filter(|&&b| b == b'\n').count();
        newlines += count;
        read += n;
        last = chunk.last().copied();
    }
    // Lines = newline count, plus one for a final line with no trailing newline.
    let trailing = usize::from(last.is_some_and(|b| b != b'\n'));
    Ok(Some(u32::try_from(newlines + trailing).unwrap_or(u32::MAX)))
}

// --- pure parsers (unit-tested without a repo) ---------------------------------

/// New path to line counts from `git diff --numstat -z`; `None` is git's `-`/`-` binary verdict.
fn parse_numstat(out: &str) -> HashMap<String, Option<(u32, u32)>> {
    let mut map = HashMap::new();
    let mut it = out.split('\0');
    while let Some(field) = it.next() {
        // `splitn(3)` keeps any tabs inside the path (verbatim under `-z`) intact.
        let mut parts = field.splitn(3, '\t');
        let add = parts.next().unwrap_or("");
        let del = parts.next().unwrap_or("");
        // Either side reading `-` is the whole record's verdict; git never mixes them.
        let counts = match (add.parse(), del.parse()) {
            (Ok(a), Ok(d)) => Some((a, d)),
            _ => None,
        };
        match parts.next() {
            // Non-rename: the path rode this same field.
            Some(path) if !path.is_empty() => {
                map.insert(path.to_string(), counts);
            }
            // Rename/copy: the next two fields are the old and new paths.
            Some(_) => {
                let _old = it.next();
                if let Some(new) = it.next().filter(|n| !n.is_empty()) {
                    map.insert(new.to_string(), counts);
                }
            }
            // No tab fields — a trailing empty record after the final NUL.
            None => {}
        }
    }
    map
}

/// One record of `git diff --raw --no-abbrev -z`.
#[derive(Debug, PartialEq, Eq)]
struct RawRow {
    kind: ChangeKind,
    path: String,
    /// The old path of a rename or copy, whose content is the old side.
    previous_path: Option<String>,
    /// Each side's blob, all zeros where the side is absent or is the worktree.
    old_oid: String,
    new_oid: String,
}

/// The raw records of a [`diff_args`] run, and the numstat records after them.
fn parse_raw(out: &str) -> (Vec<RawRow>, &str) {
    /// One NUL-terminated field off the front of `rest`.
    fn field<'a>(rest: &mut &'a str) -> Option<&'a str> {
        let (head, tail) = rest.split_once('\0')?;
        *rest = tail;
        Some(head)
    }
    let mut rows = Vec::new();
    let mut rest = out;
    while rest.starts_with(':') {
        let mut next = rest;
        let Some(meta) = field(&mut next) else { break };
        let fields: Vec<&str> = meta[1..].split(' ').collect();
        let [_, _, old_oid, new_oid, status] = fields[..] else { break };
        let (kind, previous_path) = match status.chars().next() {
            Some('A') => (ChangeKind::Added, None),
            Some('D') => (ChangeKind::Deleted, None),
            Some(code @ ('R' | 'C')) => {
                let kind = if code == 'R' { ChangeKind::Renamed } else { ChangeKind::Copied };
                let Some(source) = field(&mut next) else { break };
                (kind, Some(source.to_string()))
            }
            // Modified, type-changed, etc.
            _ => (ChangeKind::Modified, None),
        };
        let Some(path) = field(&mut next) else { break };
        rest = next;
        rows.push(RawRow {
            kind,
            path: path.to_string(),
            previous_path,
            old_oid: old_oid.to_string(),
            new_oid: new_oid.to_string(),
        });
    }
    (rows, rest)
}

/// Each blob's size by id, asked once while the cache holds it.
fn blob_sizes(repo: &Path, oids: &[&str]) -> Result<HashMap<String, u64>> {
    let session = session(repo)?;
    let known = &session.sizes;
    // The result is this call's own: the cache hits now, then what git answers, so another
    // thread clearing the cache never drops a size from it.
    let mut sizes = HashMap::new();
    let mut unknown = Vec::new();
    {
        let known = known.lock().unwrap_or_else(PoisonError::into_inner);
        for oid in oids.iter().copied().filter(|oid| oid.bytes().any(|b| b != b'0')) {
            match known.get(oid) {
                Some(&size) => drop(sizes.insert(oid.to_string(), size)),
                None => unknown.push(oid),
            }
        }
    }
    if !unknown.is_empty() {
        let input = unknown.join("\n") + "\n";
        let args = ["cat-file", "--batch-check=%(objectname) %(objectsize)"];
        let out = git_stdin(repo, &args, &input)?;
        for (oid, size) in out.lines().filter_map(|line| line.split_once(' ')) {
            if let Ok(size) = size.parse() {
                sizes.insert(oid.to_string(), size);
            }
        }
        let mut known = known.lock().unwrap_or_else(PoisonError::into_inner);
        // A turn's snapshots mint new blobs every write, so the cache stays bounded.
        if known.len() >= 100_000 {
            known.clear();
        }
        known.extend(sizes.iter().map(|(oid, &size)| (oid.clone(), size)));
    }
    Ok(sizes)
}

#[cfg(test)]
mod tests {
    use super::{
        ChangeKind, Forge, ForgeHosts, RepoTarget, RepositoryIdentity, classify_remote,
        parse_numstat, parse_raw,
    };

    const NONE: ForgeHosts<'_> = ForgeHosts { github: None, gitlab: None, azure_devops: None };

    #[test]
    fn a_path_limited_build_merges_untracked_counts_and_never_shrinks_them() {
        let (dir, git) = crate::test_support::test_repo();
        std::fs::write(dir.path().join("t.txt"), "t\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        // Written long enough ago that their counts settle.
        let past = std::time::SystemTime::now() - std::time::Duration::from_mins(1);
        for (name, body) in [("a.txt", "1\n2\n"), ("b.txt", "1\n2\n3\n")] {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(past).unwrap();
        }
        let counted = |repo: &std::path::Path| -> Vec<String> {
            let session = super::session(repo).unwrap();
            let counts = session.counts.lock().unwrap();
            let mut paths: Vec<String> = counts.keys().map(|(p, _)| p.clone()).collect();
            paths.sort();
            paths
        };
        let head = git(&["rev-parse", "HEAD"]);
        super::changed_from(dir.path(), &head).unwrap();
        assert_eq!(counted(dir.path()), ["a.txt", "b.txt"]);
        let only_a = super::changed_from_in(dir.path(), &head, &["a.txt".to_string()]).unwrap();
        assert_eq!(only_a.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["a.txt"]);
        assert_eq!(
            counted(dir.path()),
            ["a.txt", "b.txt"],
            "b.txt's count outlives a batch on a.txt"
        );
    }

    #[test]
    fn a_path_limited_read_reads_only_the_named_paths() {
        let (dir, git) = crate::test_support::test_repo();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(dir.path().join(name), "one\n").unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        let old = git(&["rev-parse", "HEAD"]);
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), "two\n").unwrap();
        }
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/app"), "x\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "two"]);
        let new = git(&["rev-parse", "HEAD"]);
        let named = |list: &[&str]| list.iter().map(|p| (*p).to_string()).collect::<Vec<_>>();
        let paths_of = |files: Vec<crate::model::ChangedFile>| {
            files.into_iter().map(|f| f.path).collect::<Vec<_>>()
        };

        let between = super::changed_between_in(dir.path(), &old, &new, &named(&["a.txt"]));
        assert_eq!(paths_of(between.unwrap()), ["a.txt"], "one commit range, one path");
        let gone = super::changed_between_in(dir.path(), &old, &new, &named(&["zzz.txt"]));
        assert!(gone.unwrap().is_empty(), "a path the range never touched reads as unchanged");

        let entries = super::all_files_in(dir.path(), &named(&["b.txt", "target"])).unwrap();
        let listed: Vec<(&str, bool)> =
            entries.iter().map(|e| (e.path.as_str(), e.ignored)).collect();
        assert_eq!(listed, [("b.txt", false), ("target", true)], "only the named entries");
    }

    #[test]
    fn a_path_limited_snapshot_reads_only_the_named_paths() {
        let (dir, git) = crate::test_support::test_repo();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(dir.path().join(name), "one\n").unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        let seed = super::snapshot_worktree(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        let only_a = super::snapshot_worktree_in(dir.path(), &seed, &["a.txt".to_string()]);
        let only_a = only_a.unwrap().expect("under the cap");
        assert_ne!(only_a, seed, "the named change is read");
        assert_ne!(only_a, super::snapshot_worktree(dir.path()).unwrap(), "b.txt is not");
        std::fs::write(dir.path().join("b.txt"), "one\n").unwrap();
        assert_eq!(only_a, super::snapshot_worktree(dir.path()).unwrap(), "a.txt alone moved");
    }

    #[test]
    fn a_turn_snapshot_never_waits_on_the_session_copy() {
        let (dir, git) = crate::test_support::test_repo();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        let head = git(&["rev-parse", "HEAD"]);
        super::changed_from(dir.path(), &head).unwrap();
        let session = super::session(dir.path()).unwrap();
        // A diff on the session copy holds its lock for the whole read.
        let busy = session.copy.lock().unwrap();
        let repo = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(super::snapshot_worktree(&repo).is_ok()));
        let done = rx.recv_timeout(std::time::Duration::from_secs(10));
        drop(busy);
        assert_eq!(done, Ok(true), "the snapshot finished while the copy was held");
        assert_eq!(
            super::snapshot_worktree(dir.path()).unwrap(),
            git(&["rev-parse", "HEAD^{tree}"])
        );
    }

    #[test]
    fn check_ignore_reads_every_name_as_itself_and_a_failed_run_answers_nothing() {
        let (dir, _) = crate::test_support::test_repo();
        std::fs::write(dir.path().join(".gitignore"), "node_modules/\n:!x\n").unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        let names = ["node_modules", ":!x", "a*b", "ok.txt"];
        let ignored = super::check_ignore(dir.path(), names.into_iter()).expect("git answered");
        let expected: std::collections::HashSet<String> =
            ["node_modules", ":!x"].map(str::to_string).into();
        assert_eq!(ignored, expected, "a name with pathspec magic is only a name");
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(super::check_ignore(outside.path(), names.into_iter()), None);
    }

    #[test]
    fn a_git_error_names_the_subcommand_not_the_argv() {
        assert_eq!(super::subcommand(&["-c", "x=y", "rev-parse", "--verify", "HEAD"]), "rev-parse");
        assert_eq!(super::subcommand(&["for-each-ref"]), "for-each-ref");
    }

    #[test]
    fn repo_identity_ignores_case_but_not_forge_host_or_depth() {
        let gl =
            |host: &str, path: &[&str]| RepoTarget::with_path(Forge::GitLab, host, path).unwrap();
        let acme = RepoTarget::new("github.com", "Acme", "Widgets").unwrap();
        assert!(acme.is(&RepoTarget::new("github.com", "acme", "widgets").unwrap()));
        assert!(!acme.is(&RepoTarget::new("ghe.corp.test", "acme", "widgets").unwrap()));
        assert!(!acme.is(&gl("github.com", &["acme", "widgets"])), "another forge");
        assert!(
            !gl("gitlab.com", &["group", "sub", "repo"]).is(&gl("gitlab.com", &["group", "sub"]))
        );
        let ado = |host: &str| {
            RepoTarget::with_path(Forge::AzureDevOps, host, &["org", "proj", "app"]).unwrap()
        };
        assert!(ado("org.visualstudio.com").is(&ado("dev.azure.com")), "one cloud org, two hosts");
        assert!(!ado("ado.corp.test").is(&ado("dev.azure.com")), "a server is its own namespace");
    }

    fn github(host: &str) -> ForgeHosts<'_> {
        ForgeHosts { github: Some(host), ..NONE }
    }

    fn gitlab(host: &str) -> ForgeHosts<'_> {
        ForgeHosts { gitlab: Some(host), ..NONE }
    }

    fn azure_devops(host: &str) -> ForgeHosts<'_> {
        ForgeHosts { azure_devops: Some(host), ..NONE }
    }

    #[test]
    fn worktree_of_distinguishes_a_repo_from_a_plain_directory() {
        use super::{Worktree, worktree_of};
        // A plain directory git can read but that holds no worktree.
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(worktree_of(outside.path()), Worktree::Outside);
        // Compared through std canonicalization, an oracle independent of `worktree_of`.
        let (repo, _) = crate::test_support::test_repo();
        let Worktree::Root(root) = worktree_of(repo.path()) else {
            panic!("a fresh repository resolves to a worktree root");
        };
        let canonical = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(canonical(&root), canonical(repo.path()));
    }

    #[test]
    fn a_file_at_the_line_budget_edited_at_both_ends_reads_whole() {
        let (repo, git) = crate::test_support::test_repo();
        let old = (0..crate::diff::MAX_LINES).map(|i| i.to_string() + "\n").collect::<String>();
        std::fs::write(repo.path().join("f.txt"), &old).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        let new = format!("first\n{}last\n", &old[2..old.len() - 6]);
        std::fs::write(repo.path().join("f.txt"), &new).unwrap();
        let sides = super::diff_sides(repo.path(), "HEAD", None, "f.txt", None).unwrap();
        assert!(sides == super::DiffSides::Text { old, new }, "the sides are not the whole file");
    }

    #[test]
    fn core_editor_reads_the_configured_value_verbatim() {
        // The repository's own level, so the test reads the same on every runner.
        let (repo, git) = crate::test_support::test_repo();
        let value = r#""C:\Program Files\Microsoft VS Code\Code.exe" --wait"#;
        git(&["config", "core.editor", value]);
        assert_eq!(super::core_editor(repo.path()).as_deref(), Some(value));
    }

    #[test]
    fn repository_identity_parses_github_and_enterprise_remote_forms() {
        let repo = |host: &str, owner: &str, name: &str| {
            RepositoryIdentity::Repository(RepoTarget::new(host, owner, name).unwrap())
        };
        // HTTPS, with and without `.git` and a trailing slash.
        assert_eq!(
            classify_remote("https://github.com/owner/repo.git", &NONE),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("https://github.com/owner/repo", &NONE),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("https://github.com/owner/repo/", &NONE),
            repo("github.com", "owner", "repo")
        );
        // scp-like SSH, and the `ssh://` scheme form with a port.
        assert_eq!(
            classify_remote("git@github.com:owner/repo.git", &NONE),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("ssh://git@github.com/owner/repo.git", &NONE),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("ssh://git@github.com:22/owner/repo.git", &NONE),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote("git://github.com/owner/repo", &NONE),
            repo("github.com", "owner", "repo")
        );
        assert_eq!(
            classify_remote(
                "https://github.company.com/owner/repo.git",
                &github("github.company.com")
            ),
            repo("github.company.com", "owner", "repo")
        );
    }

    #[test]
    fn repository_identity_parses_gitlab_remote_forms() {
        let repo = |host: &str, segments: &[&str]| {
            RepositoryIdentity::Repository(
                RepoTarget::with_path(Forge::GitLab, host, segments).unwrap(),
            )
        };
        assert_eq!(
            classify_remote("https://gitlab.com/owner/repo.git", &NONE),
            repo("gitlab.com", &["owner", "repo"])
        );
        assert_eq!(
            classify_remote("git@gitlab.com:owner/repo.git", &NONE),
            repo("gitlab.com", &["owner", "repo"])
        );
        // Nested groups keep the full namespace path.
        assert_eq!(
            classify_remote("https://gitlab.com/group/subgroup/project.git", &NONE),
            repo("gitlab.com", &["group", "subgroup", "project"])
        );
        assert_eq!(
            classify_remote("git@git.corp.example:team/sub/repo.git", &gitlab("git.corp.example")),
            repo("git.corp.example", &["team", "sub", "repo"])
        );
        // A GitHub target and a GitLab target on the same path are different targets.
        let RepositoryIdentity::Repository(on_github) =
            classify_remote("https://github.com/owner/repo", &NONE)
        else {
            panic!("expected a repository identity");
        };
        let RepositoryIdentity::Repository(on_gitlab) =
            classify_remote("https://gitlab.com/owner/repo", &NONE)
        else {
            panic!("expected a repository identity");
        };
        assert_ne!(on_github, on_gitlab);
        assert_eq!(on_github.forge(), Forge::GitHub);
        assert_eq!(on_gitlab.forge(), Forge::GitLab);
        // A single-segment GitLab path is malformed, not unsupported.
        assert_eq!(
            classify_remote("https://gitlab.com/owner", &NONE),
            RepositoryIdentity::Malformed("gitlab.com".to_string())
        );
    }

    #[test]
    fn repository_identity_rejects_aliases_and_keeps_failure_states_distinct() {
        assert_eq!(
            classify_remote("git@github.com-work:owner/repo.git", &NONE),
            RepositoryIdentity::Unsupported("github.com-work".to_string())
        );
        assert_eq!(
            classify_remote(
                "git@github.company.com-work:owner/repo.git",
                &github("github.company.com")
            ),
            RepositoryIdentity::Unsupported("github.company.com-work".to_string())
        );
        assert_eq!(
            classify_remote("https://github.com-attacker/owner/repo", &NONE),
            RepositoryIdentity::Unsupported("github.com-attacker".to_string())
        );
        assert_eq!(
            classify_remote(
                "https://github.company.com-work/owner/repo",
                &github("github.company.com")
            ),
            RepositoryIdentity::Unsupported("github.company.com-work".to_string())
        );
        assert_eq!(
            classify_remote("git@gitlab.com-work:owner/repo.git", &NONE),
            RepositoryIdentity::Unsupported("gitlab.com-work".to_string())
        );
        assert_eq!(
            classify_remote("https://bitbucket.org/owner/repo", &NONE),
            RepositoryIdentity::Unsupported("bitbucket.org".to_string())
        );
        assert_eq!(
            classify_remote("https://github.com/owner", &NONE),
            RepositoryIdentity::Malformed("github.com".to_string())
        );
        assert_eq!(
            classify_remote("https://github.com", &NONE),
            RepositoryIdentity::Malformed("github.com".to_string())
        );
        assert_eq!(
            classify_remote(
                "https://github.company.com:8443/owner/repo.git",
                &github("github.company.com")
            ),
            RepositoryIdentity::Unsupported("github.company.com".to_string())
        );
        assert_eq!(classify_remote("/tmp/repo", &NONE), RepositoryIdentity::Hostless);
        assert_eq!(classify_remote("file:///tmp/repo", &NONE), RepositoryIdentity::Hostless);
        assert_eq!(
            classify_remote("file://github.com/owner/repo", &NONE),
            RepositoryIdentity::Unsupported("github.com".to_string())
        );
        assert_eq!(
            classify_remote("ftp://github.com/owner/repo", &NONE),
            RepositoryIdentity::Unsupported("github.com".to_string())
        );
    }

    #[test]
    fn repository_identity_parses_azure_devops_remote_forms_to_one_target() {
        let repo = |host: &str, org: &str, project: &str, name: &str| {
            RepositoryIdentity::Repository(
                RepoTarget::with_path(Forge::AzureDevOps, host, &[org, project, name]).unwrap(),
            )
        };
        // The https `_git` form, with and without `.git`, plus case-insensitive hosts.
        assert_eq!(
            classify_remote("https://dev.azure.com/org/project/_git/repo", &NONE),
            repo("dev.azure.com", "org", "project", "repo")
        );
        assert_eq!(
            classify_remote("https://DEV.AZURE.COM/org/project/_git/repo.git", &NONE),
            repo("dev.azure.com", "org", "project", "repo")
        );
        // A repository named after its project omits the project segment.
        assert_eq!(
            classify_remote("https://dev.azure.com/org/_git/repo", &NONE),
            repo("dev.azure.com", "org", "repo", "repo")
        );
        // The v3 ssh forms normalize to the https host, so both clones are one target.
        assert_eq!(
            classify_remote("git@ssh.dev.azure.com:v3/org/project/repo", &NONE),
            repo("dev.azure.com", "org", "project", "repo")
        );
        assert_eq!(
            classify_remote("ssh://git@ssh.dev.azure.com/v3/org/project/repo", &NONE),
            repo("dev.azure.com", "org", "project", "repo")
        );
        // The legacy organization hosts, with the wildcard match and the org hoist.
        assert_eq!(
            classify_remote("https://org.visualstudio.com/project/_git/repo", &NONE),
            repo("org.visualstudio.com", "org", "project", "repo")
        );
        assert_eq!(
            classify_remote(
                "https://org.visualstudio.com/DefaultCollection/project/_git/repo",
                &NONE
            ),
            repo("org.visualstudio.com", "org", "project", "repo")
        );
        assert_eq!(
            classify_remote("org@vs-ssh.visualstudio.com:v3/org/project/repo", &NONE),
            repo("org.visualstudio.com", "org", "project", "repo")
        );
        // A self-hosted server recognized through `azure_devops_host`, collection first.
        assert_eq!(
            classify_remote(
                "https://tfs.corp.example/collection/project/_git/repo",
                &azure_devops("tfs.corp.example")
            ),
            repo("tfs.corp.example", "collection", "project", "repo")
        );
        // A project named with a space travels percent-encoded and is addressed decoded.
        assert_eq!(
            classify_remote("https://dev.azure.com/extruct/Extruct%20AI/_git/reviewr-qa", &NONE),
            repo("dev.azure.com", "extruct", "Extruct AI", "reviewr-qa")
        );
        // Every casing and clone form is one target.
        assert_eq!(
            classify_remote("https://dev.azure.com/Extruct/project/_git/repo", &NONE),
            repo("dev.azure.com", "extruct", "project", "repo")
        );
        assert_eq!(
            classify_remote("Org@vs-ssh.visualstudio.com:v3/Extruct/project/repo", &NONE),
            repo("extruct.visualstudio.com", "extruct", "project", "repo")
        );
        // Self-hosted, `DefaultCollection` is the collection and survives.
        assert_eq!(
            classify_remote(
                "https://tfs.corp.example/DefaultCollection/proj/_git/repo",
                &azure_devops("tfs.corp.example")
            ),
            repo("tfs.corp.example", "defaultcollection", "proj", "repo")
        );
        // A broken escape is a malformed remote, not a silent misread.
        assert_eq!(
            classify_remote("https://dev.azure.com/org/Bad%2/_git/repo", &NONE),
            RepositoryIdentity::Malformed("dev.azure.com".to_string())
        );
    }

    #[test]
    fn repository_identity_rejects_malformed_azure_devops_paths() {
        // A project URL is not a repository, and extra segments are not an identity.
        assert_eq!(
            classify_remote("https://dev.azure.com/org/project", &NONE),
            RepositoryIdentity::Malformed("dev.azure.com".to_string())
        );
        assert_eq!(
            classify_remote("https://dev.azure.com/org/project/_git/repo/extra", &NONE),
            RepositoryIdentity::Malformed("dev.azure.com".to_string())
        );
        // A virtual directory's four-part path is malformed, never misread.
        assert_eq!(
            classify_remote(
                "https://tfs.corp.example/tfs/collection/project/_git/repo",
                &azure_devops("tfs.corp.example")
            ),
            RepositoryIdentity::Malformed("tfs.corp.example".to_string())
        );
        assert_eq!(
            classify_remote("https://dev.azure.com", &NONE),
            RepositoryIdentity::Malformed("dev.azure.com".to_string())
        );
        // The wildcard needs an organization label; the bare domain stays unsupported.
        assert_eq!(
            classify_remote("https://visualstudio.com/org/project/_git/repo", &NONE),
            RepositoryIdentity::Unsupported("visualstudio.com".to_string())
        );
        // An unrecognized host never reaches the Azure DevOps path shaping.
        assert_eq!(
            classify_remote("https://dev.azure.com.evil.example/org/project/_git/repo", &NONE),
            RepositoryIdentity::Unsupported("dev.azure.com.evil.example".to_string())
        );
        // An option-shaped segment can never become an `az` argument.
        assert_eq!(
            classify_remote("https://dev.azure.com/org/--project/_git/repo", &NONE),
            RepositoryIdentity::Malformed("dev.azure.com".to_string())
        );
    }

    #[test]
    fn azure_devops_vocabulary_matches_the_provider_contract() {
        assert_eq!(Forge::AzureDevOps.display_name(), "Azure DevOps");
        assert_eq!(Forge::AzureDevOps.noun(), "pull request");
        assert_eq!(Forge::AzureDevOps.abbr(), "PR");
        assert_eq!(Forge::AzureDevOps.sigil(), '#');
        assert_eq!(Forge::AzureDevOps.cli(), "az");
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
        // A GitHub path is exactly two segments; a GitLab path is two or more.
        assert!(RepoTarget::with_path(Forge::GitHub, "github.com", &["a", "b", "c"]).is_none());
        let nested =
            RepoTarget::with_path(Forge::GitLab, "gitlab.com", &["group", "sub", "repo"]).unwrap();
        assert_eq!(nested.full_path(), "group/sub/repo");
        assert_eq!(nested.name(), "repo");
        assert!(RepoTarget::with_path(Forge::GitLab, "gitlab.com", &["only"]).is_none());
    }

    #[test]
    fn numstat_parses_counts_and_keeps_the_no_text_diff_verdict() {
        let m = parse_numstat("18\t8\tsrc/a.rs\0-\t-\tassets/logo.png\0");
        assert_eq!(m["src/a.rs"], Some((18, 8)));
        // `-`/`-` is git refusing to text-diff, not zero lines.
        assert_eq!(m["assets/logo.png"], None);
    }

    #[test]
    fn numstat_separates_the_no_text_diff_verdict_from_an_empty_change() {
        let m = parse_numstat("0\t0\tsrc/touched.rs\0-\t-\tflake.lock\0");
        assert_eq!(m["src/touched.rs"], Some((0, 0)));
        assert_eq!(m["flake.lock"], None);
    }

    #[test]
    fn numstat_keys_renames_under_the_new_path() {
        // A `-z` rename is `ADDS\tDELS\t\0OLD\0NEW`, keyed under the new path.
        let m = parse_numstat("3\t1\t\0src/old.rs\0src/new.rs\0");
        assert_eq!(m["src/new.rs"], Some((3, 1)));
        assert!(!m.contains_key("src/old.rs"));
    }

    #[test]
    fn numstat_dir_removing_rename_has_no_double_slash() {
        // Regression: the old brace parser produced `a//file.rs` here, so counts never matched.
        let m = parse_numstat("4\t2\t\0a/b/file.rs\0a/file.rs\0");
        assert_eq!(m["a/file.rs"], Some((4, 2)));
        assert!(!m.contains_key("a//file.rs"));
    }

    #[test]
    fn numstat_handles_a_mixed_stream() {
        // Binary, plain, rename in sequence: the rename lookahead stays aligned.
        let m = parse_numstat("-\t-\tlogo.png\x009\t1\tsrc/a.rs\x005\t4\t\x00o.rs\x00n.rs\x00");
        assert_eq!(m["logo.png"], None);
        assert_eq!(m["src/a.rs"], Some((9, 1)));
        assert_eq!(m["n.rs"], Some((5, 4)));
    }

    #[test]
    fn a_raw_record_reads_its_kind_its_path_and_a_renames_source() {
        let meta =
            |status: &str| format!(":100644 100644 {} {} {status}", "a".repeat(40), "0".repeat(40));
        let raw = [
            meta("M"),
            "src/a.rs".into(),
            meta("A"),
            "src/b.rs".into(),
            meta("D"),
            "src/c.rs".into(),
            meta("R100"),
            "old.rs".into(),
            "new.rs".into(),
            meta("M"),
            "with\nnewline".into(),
            meta("M"),
            ":colon-led".into(),
            // The numstat records that follow the raw ones in the same run.
            "1\t1\tsrc/a.rs".into(),
            String::new(),
        ]
        .join("\0");
        let (rows, numstat) = parse_raw(&raw);
        assert_eq!(numstat, "1\t1\tsrc/a.rs\0");
        assert_eq!(rows[0].old_oid, "a".repeat(40));
        let rows: Vec<_> = rows.into_iter().map(|r| (r.kind, r.path, r.previous_path)).collect();
        assert_eq!(rows[0], (ChangeKind::Modified, "src/a.rs".to_string(), None));
        assert_eq!(rows[1], (ChangeKind::Added, "src/b.rs".to_string(), None));
        assert_eq!(rows[2], (ChangeKind::Deleted, "src/c.rs".to_string(), None));
        assert_eq!(
            rows[3],
            (ChangeKind::Renamed, "new.rs".to_string(), Some("old.rs".to_string()))
        );
        assert_eq!(rows[4], (ChangeKind::Modified, "with\nnewline".to_string(), None));
        assert_eq!(rows[5], (ChangeKind::Modified, ":colon-led".to_string(), None));
    }

    #[test]
    fn a_copy_keys_under_its_new_path() {
        // A copy keys under its new path, like a rename.
        let raw = format!(":100644 100644 {0} {0} C75\0orig.rs\0copy.rs\0", "b".repeat(40));
        let row = &parse_raw(&raw).0[0];
        assert_eq!(
            (row.kind, row.path.as_str(), row.previous_path.as_deref()),
            (ChangeKind::Copied, "copy.rs", Some("orig.rs"))
        );
    }

    #[test]
    fn a_full_context_diff_spells_both_sides_exactly() {
        use super::{DiffSides, parse_sides};
        let text = |old: &str, new: &str| {
            Some(DiffSides::Text { old: old.to_string(), new: new.to_string() })
        };
        let header = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n";
        let cases: &[(&str, Option<DiffSides>)] = &[
            // A body line that looks like a header is still body.
            (" a\n--- x\n+b\n c\n", text("a\n-- x\nc\n", "a\nb\nc\n")),
            // A bare newline is an empty context line.
            (" a\n\n-b\n", text("a\n\nb\n", "a\n\n")),
            // The marker takes the newline off the line before it, on that line's side only.
            (" x\n-y\n\\ No newline at end of file\n+z\n", text("x\ny", "x\nz\n")),
            (" x\n\\ No newline at end of file\n", text("x", "x")),
            // A CR git keeps is the line's text.
            ("-one\n+one\r\n", text("one\n", "one\r\n")),
        ];
        for (body, want) in cases {
            let lines = body.lines().filter(|l| !l.starts_with('\\')).count();
            let olds = body.lines().filter(|l| !l.starts_with(['+', '\\'])).count();
            let news = lines - body.lines().filter(|l| l.starts_with('-')).count();
            let out = format!("{header}@@ -1,{olds} +1,{news} @@\n{body}");
            assert_eq!(parse_sides(&out), *want, "{body:?}");
        }
        // An added file, its one-line hunk count left out.
        let added =
            "diff --git a/f b/f\nnew file mode 100644\n--- /dev/null\n+++ b/f\n@@ -0,0 +1 @@\n+a\n";
        assert_eq!(parse_sides(added), text("", "a\n"));
        assert_eq!(
            parse_sides(&format!("{header}Binary files a/f and b/f differ\n")),
            Some(DiffSides::Binary)
        );
        // No hunk: the sides are equal, and the caller reads the one blob.
        assert_eq!(parse_sides("diff --git a/f b/g\nsimilarity index 100%\n"), None);
        assert_eq!(parse_sides(""), None);
    }

    #[test]
    fn a_hunk_header_reads_both_ranges_a_count_of_one_left_out() {
        use super::HunkHeader;
        let rows = [
            ("@@ -105,11 +105,12 @@\n", Some(((105, 11), (105, 12)))),
            ("@@ -0,0 +1 @@\n", Some(((0, 0), (1, 1)))),
            ("@@ -7 +7,0 @@ fn main() {\n", Some(((7, 1), (7, 0)))),
            // A forge payload's doubled space still reads.
            ("@@ -3,2  +3,2 @@\n", Some(((3, 2), (3, 2)))),
            ("@@ +1 -1 @@\n", None),
            ("@@@ -1 -1 +1 @@@\n", None),
        ];
        for (line, want) in rows {
            let got = HunkHeader::parse(line).map(|h| (h.old, h.new));
            assert_eq!(got, want, "{line:?}");
        }
    }
}
