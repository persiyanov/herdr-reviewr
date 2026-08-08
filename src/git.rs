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

/// The git top-level of `path`, or `None` if it is not a repo. Collapses "git ran and said no"
/// and "git could not run" — use [`worktree_of`] when that difference matters.
pub fn toplevel(path: &Path) -> Option<PathBuf> {
    match worktree_of(path) {
        Worktree::Root(root) => Some(root),
        Worktree::Outside | Worktree::Unknown => None,
    }
}

/// A directory's git top level, keeping "git ran and it is outside any worktree" (`Outside`, a
/// determination) apart from "git could not be run at all" (`Unknown`, the absence of one — a
/// spawn error under load). A caller deciding membership must hold on `Unknown` rather than read
/// it as `Outside` (`specs/herdr-host.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Worktree {
    Root(PathBuf),
    Outside,
    Unknown,
}

/// Resolve `path` to its worktree, distinguishing the two ways resolution yields no root.
pub fn worktree_of(path: &Path) -> Worktree {
    match Command::new("git").arg("-C").arg(path).args(["rev-parse", "--show-toplevel"]).output() {
        Err(_) => Worktree::Unknown,
        Ok(out) if !out.status.success() => Worktree::Outside,
        Ok(out) => match String::from_utf8_lossy(&out.stdout).trim() {
            "" => Worktree::Outside,
            root => Worktree::Root(PathBuf::from(root)),
        },
    }
}

/// The forge a repository target belongs to. Part of the target's identity: the same path on
/// a different forge is a different target (`specs/forge-host.md`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Forge {
    /// The default carries the neutral `PR` vocabulary a forgeless state renders under.
    #[default]
    GitHub,
    GitLab,
    AzureDevOps,
}

/// The per-forge display vocabulary — the CLI, noun, and reference table in
/// `specs/forge-providers.md`.
impl Forge {
    /// The forge's display name for link labels and failure wording.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
            Self::AzureDevOps => "Azure DevOps",
        }
    }

    /// The forge's full noun: the word its users say (`specs/forge-providers.md`).
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
/// (`specs/forge-host.md`).
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
    /// Exactly `[owner, name]` for GitHub; the full namespace path (2+ segments) for GitLab;
    /// exactly `[organization, project, repository]` for Azure DevOps.
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
            // GitLab reserves `-` as the separator between a project path and the rest of a web
            // URL, so a pasted browse link is a malformed remote, not a deep namespace.
            Forge::GitLab => segments.len() >= 2 && !segments.contains(&"-"),
            // Always `[organization, project, repository]`, shaped by `ado_canonicalize`.
            Forge::AzureDevOps => segments.len() == 3,
        };
        // Azure DevOps project and repository names admit spaces and non-ASCII characters,
        // which arrive percent-encoded and are decoded by `ado_canonicalize`.
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

    /// The first path segment — the owner at the GitHub API boundary, the organization at
    /// the Azure DevOps one.
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

    /// The second path segment — the project at the Azure DevOps API boundary, whose
    /// targets always carry `[organization, project, repository]`.
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

/// An Azure DevOps identity segment after percent-decoding: any visible name, so long as it
/// cannot smuggle a path step, an option-shaped token, or a control sequence into a CLI
/// argument. A segment reaches `az` as the argv token after `--project`/`--repository`, so a
/// leading `-` must never pass — the same rule git applies to its own refnames.
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

/// Classify one repository URL against the built-in forge hosts and the configured
/// self-hosted keys (`specs/forge-host.md`).
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

/// The forge that recognizes `host`, if any — the one authority for the built-in host set.
/// Config validation asks it with default hosts, so the sets cannot drift. Config validation
/// also keeps the host sets disjoint, so at most one forge matches (`specs/config.md`).
/// `*.visualstudio.com` is the one built-in wildcard, matching every legacy Azure DevOps
/// organization host by suffix (`specs/forge-host.md`).
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

/// Canonicalize an Azure DevOps remote into its one target identity: the canonical host and
/// the `[organization, project, repository]` path (`specs/forge-providers.md`). The ssh hosts
/// fold into their https equivalents, the `v3` and `_git` URL markers drop, a legacy
/// `{org}.visualstudio.com` host contributes the organization segment, and each segment
/// percent-decodes — a project named with a space travels as `%20` in the remote URL but is
/// addressed decoded at the CLI boundary.
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
    // `DefaultCollection` is URL filler only on the legacy organization hosts, whose
    // organization lives in the hostname. On every other host the first segment is the
    // organization or collection identity and stays.
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
    // Azure DevOps treats the organization case-insensitively, and the legacy host form
    // derives it from the lowercased hostname — lowercase it everywhere, so every clone
    // form and casing of one repository is one target.
    if let Some(organization) = path.first_mut() {
        *organization = organization.to_ascii_lowercase();
    }
    (path.len() == 3).then_some((host, path))
}

/// Decode `%XX` escapes in one URL path segment, or `None` when an escape is broken or the
/// bytes are not UTF-8. A segment with no escapes passes through unchanged.
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

// --- PR-fetch local reads (branch names) ------------------------------------
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
    /// The `origin` repository, when it is a usable forge identity — on a fork clone it
    /// is the fork, queried beside the target (`specs/forge-host.md`).
    pub origin_repository: Option<RepoTarget>,
    /// The locally derived pins and branch names, read in the same pass.
    pub local: PrLocalState,
}

/// The local identity one PR fetch derives: the pins and the branch's forge names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrLocalState {
    /// `HEAD` pinned to an OID at the start of the pass; every ancestry test, distance,
    /// and the `sync` count use this pin, so one fetch reads one consistent local state.
    pub head_oid: Option<String>,
    /// The winning base entry pinned to an OID — the paint guard keys on it, so a base
    /// moving mid-fetch never paints a stale verdict.
    pub base_oid: Option<String>,
    /// The branch's forge names: the checked-out branch's own name, its recorded upstream,
    /// and the `origin` branch names at the pushed frontier — the branch the work was
    /// pushed to, whatever its local name (`specs/forge-host.md` Resolution).
    pub names: Vec<String>,
    /// `HEAD` is detached — no branch, no PR story.
    pub detached: bool,
}

/// Derive the pinned `HEAD`, the pinned base, and the branch's forge names
/// (`specs/forge-host.md` Resolution).
pub fn pr_local(repo: &Path, base_flag: Option<&str>) -> Result<PrLocalState, GitFail> {
    let Some(branch) = git_tristate(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])? else {
        return Ok(PrLocalState { detached: true, ..PrLocalState::default() });
    };
    let head_oid = git_tristate(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?;
    let resolution = resolve_base(repo, base_flag)?;
    let bases = resolution.oids();
    let mut names = vec![branch.clone()];
    let push_name = |name: String, names: &mut Vec<String>| {
        if !names.contains(&name) {
            names.push(name);
        }
    };
    if let Some(upstream) = recorded_upstream(repo, &branch, &resolution.recorded, &bases)? {
        push_name(upstream, &mut names);
    }
    if let Some(head) = &head_oid
        && !bases.is_empty()
    {
        for name in frontier_names(repo, head, &bases)? {
            push_name(name, &mut names);
        }
    }
    // A frontier of many refs stays bounded, so the per-name forge queries do.
    names.truncate(8);
    Ok(PrLocalState { head_oid, base_oid: bases.into_iter().next(), names, detached: false })
}

/// The winning base: the source's bare name and its pinned commit
/// (`specs/review-model.md` Base branch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBase {
    pub name: String,
    pub oid: String,
}

/// The chain outcome the header paints: the winner and the first recorded choice the
/// chain skipped because it no longer resolves. The skip rides beside the winner, not
/// inside it, so it survives a chain where nothing resolves at all — a dormant pick
/// never reads as never-chosen (`specs/tui.md`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseStatus {
    pub winner: Option<ResolvedBase>,
    pub skipped: Option<String>,
}

/// One pass over the base chain. `candidates` keeps every source that resolved, in
/// precedence order and deduped by OID — the PR frontier walk needs all of them, not just
/// the winner (`pr_local`). `recorded` keeps every source name the chain considered —
/// every candidate's name and every dormant one's, since a pick that fails to resolve
/// still shields its name from the PR name lookup (`specs/forge-host.md` Resolution).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseResolution {
    pub status: BaseStatus,
    candidates: Vec<(String, String)>,
    recorded: Vec<String>,
}

impl BaseResolution {
    fn oids(&self) -> Vec<String> {
        self.candidates.iter().map(|(_, oid)| oid.clone()).collect()
    }
}

/// Resolve the base chain: the `--base` flag, then the repo pick, then the branch
/// `origin/HEAD` names (`specs/review-model.md` Base branch). A source that resolves to no
/// ref is skipped, never an error; a skipped flag or pick that would have outranked the
/// winner is recorded for the header (`specs/tui.md`).
pub fn resolve_base(repo: &Path, base_flag: Option<&str>) -> Result<BaseResolution, GitFail> {
    let mut candidates: Vec<(String, String)> = Vec::new();
    let mut recorded: Vec<String> = Vec::new();
    let mut skipped: Option<String> = None;
    let push = |name: String, oid: String, c: &mut Vec<(String, String)>| {
        if !c.iter().any(|(_, o)| *o == oid) {
            c.push((name, oid));
        }
    };
    let record = |name: String, r: &mut Vec<String>| {
        if !name.is_empty() && !r.contains(&name) {
            r.push(name);
        }
    };
    if let Some(flag) = base_flag.filter(|b| !b.is_empty()) {
        // The flag is the power-user escape hatch: any rev works verbatim (a SHA, a tag,
        // `upstream/main`), and a branch-name spelling falls back to prefix-stripped
        // resolution. The header and the name shield use the bare spelling either way.
        let entry = strip_base_prefix(flag);
        record(entry.clone(), &mut recorded);
        let probe = format!("{flag}^{{commit}}");
        if let Some(oid) = git_tristate(repo, &["rev-parse", "--verify", "--quiet", &probe])? {
            push(entry, oid, &mut candidates);
        } else {
            match resolve_base_entry(repo, &entry)? {
                Some(oid) => push(entry, oid, &mut candidates),
                None => skipped = Some(entry),
            }
        }
    }
    if let Some(pick) = read_base_pick(repo)? {
        record(pick.clone(), &mut recorded);
        match resolve_base_entry(repo, &pick)? {
            Some(oid) => push(pick, oid, &mut candidates),
            // Only a failure that outranks the eventual winner reads as skipped.
            None if candidates.is_empty() => skipped = skipped.or(Some(pick)),
            None => {}
        }
    }
    if let Some(name) = default_branch_name(repo)? {
        record(name.clone(), &mut recorded);
        if let Some(oid) = resolve_base_entry(repo, &name)? {
            push(name, oid, &mut candidates);
        }
    }
    let winner =
        candidates.first().map(|(name, oid)| ResolvedBase { name: name.clone(), oid: oid.clone() });
    Ok(BaseResolution { status: BaseStatus { winner, skipped }, candidates, recorded })
}

/// The branch name `origin/HEAD` points at (`specs/review-model.md` Base branch). Some
/// clones carry `origin/HEAD` as a plain ref instead of a symref — then the name is the
/// origin tip whose commit matches it.
pub fn default_branch_name(repo: &Path) -> Result<Option<String>, GitFail> {
    let target = git_tristate(repo, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?;
    if let Some(name) =
        target.and_then(|t| t.strip_prefix("refs/remotes/origin/").map(str::to_string))
    {
        // `fetch --prune` can delete the target and leave the symref dangling: a name
        // that resolves to nothing is no default, or the picker could never mark the
        // clearing row and the name shield would carry a phantom (`specs/input.md`).
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

/// Strip the ref prefixes a `--base` branch name may carry (`specs/review-model.md`).
pub(crate) fn strip_base_prefix(entry: &str) -> String {
    ["refs/remotes/origin/", "refs/heads/", "origin/"]
        .iter()
        .find_map(|p| entry.strip_prefix(p))
        .unwrap_or(entry)
        .to_string()
}

/// Every branch name for the base picker: `refs/heads` and `refs/remotes/origin` merged by
/// bare name, newest commit first, `origin/HEAD` and the checked-out branch excluded —
/// except the `default` branch, which stays listed so its row can clear a pick even while
/// checked out (`specs/input.md` Base picker). The caller passes the default it already
/// resolved, so one picker open runs the resolution once.
pub fn list_branches(repo: &Path, default: Option<&str>) -> Result<Vec<String>, GitFail> {
    let checked_out = git_tristate(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?
        .filter(|name| default != Some(name));
    let out = git_strict(
        repo,
        &[
            "for-each-ref",
            "refs/heads",
            "refs/remotes/origin",
            "--sort=-committerdate",
            "--format=%(refname)",
        ],
    )?;
    let mut names: Vec<String> = Vec::new();
    for line in out.lines() {
        let name =
            line.strip_prefix("refs/remotes/origin/").or_else(|| line.strip_prefix("refs/heads/"));
        let Some(name) = name else { continue };
        if name == "HEAD" || checked_out.as_deref() == Some(name) {
            continue;
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// The `origin` remote-tracking tips as `(OID, bare name)`, `origin/HEAD` excluded — one
/// listing per pass serves the frontier names and the published-at-all short-circuit.
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

/// The `origin` branch names at the pushed frontier: the names of the tips at the boundary
/// of the unpushed range — or at `head` itself when nothing is unpushed. A tip on base
/// history carries no work of this branch and contributes no name. Bounded at 32 boundary
/// commits, so a merge-heavy frontier stays cheap.
fn frontier_names(repo: &Path, head: &str, bases: &[String]) -> Result<Vec<String>, GitFail> {
    let tips = origin_tips(repo)?;
    if tips.is_empty() {
        // Nothing is published at all; skip the history walk, which `--not
        // --remotes=origin` would otherwise run unbounded.
        return Ok(Vec::new());
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
    let mut names = Vec::new();
    for oid in oids {
        // The caller keeps at most 8 names, so stop paying git calls past that.
        if names.len() >= 8 {
            break;
        }
        if !beyond_all_bases(repo, &oid, bases)? {
            continue;
        }
        for (tip, name) in &tips {
            if tip == &oid && !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    Ok(names)
}

/// Whether `commit` is an ancestor of (or equal to) `of`.
fn is_ancestor(repo: &Path, commit: &str, of: &str) -> Result<bool, GitFail> {
    Ok(git_tristate(repo, &["merge-base", "--is-ancestor", commit, of])?.is_some())
}

/// Whether the pinned `HEAD` contains `commit` — the merged/closed admission guard: a
/// reused branch name never resurrects a PR whose commits this branch does not hold
/// (`specs/forge-host.md` Resolution). A commit absent from the object database is not
/// contained; an unfetched head proves nothing.
pub fn contains_commit(repo: &Path, head: &str, commit: &str) -> Result<bool, GitFail> {
    if git_tristate(repo, &["cat-file", "-e", commit])?.is_none() {
        return Ok(false);
    }
    is_ancestor(repo, commit, head)
}

/// Whether `oid` lies beyond every resolved base — an ancestor of none of them. Decides which
/// frontier tips carry provable work.
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
/// back, read errors do not. The origin identity rides along for the fork lookup —
/// on a fork clone the fork's own PRs live there (`specs/forge-host.md`). A usable `upstream`
/// already fixes the target, so an `origin` read that fails then costs only that fetch's
/// association source, not the whole read.
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

/// Classify one rewritten primary fetch URL. A missing remote is a clean state; every other
/// `remote get-url` failure is transient. The command applies `url.*.insteadOf` rewrites.
fn remote_identity(
    repo: &Path,
    remote: &str,
    hosts: &ForgeHosts<'_>,
) -> Result<RepositoryIdentity, GitFail> {
    let args = ["remote", "get-url", "--", remote];
    let out = run_git(repo, &args)?;
    if out.status.success() {
        let url = std::str::from_utf8(&out.stdout)
            .map_err(|_| GitFail(format!("git remote get-url {remote}: invalid UTF-8")))?;
        return Ok(classify_remote(url.trim(), hosts));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.to_lowercase().contains("no such remote") {
        return Ok(RepositoryIdentity::Missing);
    }
    Err(GitFail(format!("git {args:?}: {}", stderr.trim())))
}

/// One base branch name pinned to an OID: `refs/remotes/origin/<name>`, then
/// `refs/heads/<name>` (`specs/review-model.md` Base branch).
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
/// name, or `None` when unset, not under a remote, or naming a resolved base — the record
/// `git switch -c work origin/main` auto-writes is tracking, not publication. A base
/// resolved without a configured name — through `origin/HEAD` or a verbatim `--base` rev —
/// is recognized by its tip OID in `base_oids` instead. `for-each-ref` exits 0 with an empty
/// field when unset, so absence never reads as failure (`rev-parse @{u}` exits 128 for
/// both). `%(push)` is deliberately not consulted: with any remote present git *computes*
/// a destination even with nothing recorded, which would shadow a real record.
fn recorded_upstream(
    repo: &Path,
    branch: &str,
    base_names: &[String],
    base_oids: &[String],
) -> Result<Option<String>, GitFail> {
    let out = git_strict(
        repo,
        &["for-each-ref", &format!("refs/heads/{branch}"), "--format=%(upstream)"],
    )?;
    let dest = out.lines().next().unwrap_or("").trim();
    let Some(rest) = dest.strip_prefix("refs/remotes/") else { return Ok(None) };
    let Some((_, name)) = rest.split_once('/') else { return Ok(None) };
    if name.is_empty() || base_names.iter().any(|entry| entry == name) {
        return Ok(None);
    }
    // A pruned upstream ref no longer resolves; the record still carries the name
    // (`specs/forge-host.md` Resolution — a stale local record costs recall, never
    // correctness).
    let probe = format!("{dest}^{{commit}}");
    if let Some(tip) = git_tristate(repo, &["rev-parse", "--verify", "--quiet", &probe])?
        && base_oids.contains(&tip)
    {
        return Ok(None);
    }
    Ok(Some(name.to_string()))
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

/// The merge-base commit of the resolved base OID and `HEAD`
/// (`specs/review-model.md` Base branch).
pub fn merge_base(repo: &Path, base_oid: &str) -> Option<String> {
    git_line(repo, &["merge-base", base_oid, "HEAD"])
}

/// The content of `path` at `rev` (`git show <rev>:<path>`). Empty when the path does
/// not exist at that rev — an added file against its old side, say.
pub fn file_content(repo: &Path, rev: &str, path: &str) -> String {
    git_lenient(repo, &["show", &format!("{rev}:{path}")])
}

// --- base pick (branch scope) --------------------------------------------------
//
// See `specs/review-model.md` Base branch. The pick is one branch name per repository:
// a blob under one private ref, and refs are shared across a repository's worktrees, so
// every pane reads the same pick.

const BASE_PICK_REF: &str = "refs/reviewr/base-pick";

/// The recorded pick's branch name, or `None` when no pick is recorded. One git call, so a
/// concurrent clear from another pane can never split the read the way an exists-then-read
/// pair would; a failed read is no pick, matching the chain's skip-never-error contract
/// (`specs/review-model.md`).
pub fn read_base_pick(repo: &Path) -> Result<Option<String>, GitFail> {
    let out = run_git(repo, &["cat-file", "blob", BASE_PICK_REF])?;
    if !out.status.success() {
        return Ok(None);
    }
    let name = String::from_utf8_lossy(&out.stdout);
    let name = name.trim();
    Ok(branch_name_shaped(name).then(|| name.to_string()))
}

/// Whether a recorded pick is shaped like the branch name reviewr writes there. The ref is
/// shared repository state any tool can write, and a pick that resolves to nothing still
/// paints its name in the header, so a value git could never have produced is no pick at
/// all — the chain's skip-never-error contract (`specs/review-model.md`), and the rule git
/// itself applies to a refname.
fn branch_name_shaped(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains(['~', '^', ':', '?', '*', '[', '\\'])
        && value.bytes().all(|byte| byte > b' ' && byte != 0x7f)
}

/// Record `name` as the repository's pick. The ref write lands before the pick applies
/// (`specs/review-model.md`), so a crash between the two loses nothing.
pub fn write_base_pick(repo: &Path, name: &str) -> Result<(), GitFail> {
    let blob = git_stdin(repo, &["hash-object", "-w", "--stdin"], name)?;
    git_strict(repo, &["update-ref", BASE_PICK_REF, blob.trim()])?;
    Ok(())
}

/// Drop the recorded pick — choosing the default branch clears it
/// (`specs/review-model.md`).
pub fn clear_base_pick(repo: &Path) -> Result<(), GitFail> {
    git_strict(repo, &["update-ref", "-d", BASE_PICK_REF])?;
    Ok(())
}

/// Run git with `input` piped to stdin, any non-zero exit a failure.
fn git_stdin(repo: &Path, args: &[&str], input: &str) -> Result<String, GitFail> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("LC_ALL", "C")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GitFail(format!("git {args:?}: {e}")))?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(input.as_bytes())
        .map_err(|e| GitFail(format!("git {args:?}: {e}")))?;
    let out = child.wait_with_output().map_err(|e| GitFail(format!("git {args:?}: {e}")))?;
    if !out.status.success() {
        return Err(GitFail(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

/// The changed files for `scope`, sorted by path. `branch_base` is the resolved base OID
/// for the `branch` scope ([`resolve_base`]'s winner); with none the scope lists nothing.
/// `last-turn` is resolved separately by [`changed_against_tree`], so it lists nothing here.
pub fn changed_files(
    repo: &Path,
    scope: Scope,
    branch_base: Option<&str>,
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
        Scope::Branch => match branch_base.and_then(|b| merge_base(repo, b)) {
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
        ChangeKind, Forge, ForgeHosts, RepoTarget, RepositoryIdentity, classify_remote,
        parse_name_status, parse_numstat,
    };

    const NONE: ForgeHosts<'_> = ForgeHosts { github: None, gitlab: None, azure_devops: None };

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
        // A real worktree resolves to its root. Compare against std canonicalization, an oracle
        // independent of `worktree_of` (both git and std resolve the temp dir's symlinks).
        let repo = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        let canonical = std::fs::canonicalize(repo.path()).unwrap();
        assert_eq!(worktree_of(repo.path()), Worktree::Root(canonical));
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
        // The organization is case-insensitive on Azure DevOps and the legacy host derives
        // it lowercased, so every casing and clone form is one target.
        assert_eq!(
            classify_remote("https://dev.azure.com/Extruct/project/_git/repo", &NONE),
            repo("dev.azure.com", "extruct", "project", "repo")
        );
        assert_eq!(
            classify_remote("Org@vs-ssh.visualstudio.com:v3/Extruct/project/repo", &NONE),
            repo("extruct.visualstudio.com", "extruct", "project", "repo")
        );
        // On a self-hosted server the first segment is the collection identity, so a
        // literal `DefaultCollection` collection survives canonicalization.
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
        // A self-hosted virtual directory is not supported: its extra path segment leaves a
        // four-part path, which is malformed, not a silently misread target.
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
