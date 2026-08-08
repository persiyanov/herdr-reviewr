//! The shared forge kernel: fetch-input derivation, per-forge dispatch, and the GitHub read.
//!
//! See `specs/forge-host.md`. A fetch first derives [`PrFetchInput`] from local Git and one
//! validated config snapshot, then routes to the resolved forge's provider: GitHub reads
//! inline here through explicitly hosted `gh` GraphQL calls, GitLab and Azure DevOps through
//! their own modules (`crate::gitlab`, `crate::azure_devops`). The normalized [`PrSnapshot`],
//! the [`PrView`] failure states with their remedies, and the association helpers every
//! provider shares all live here. Nothing ever writes to a forge. The `PR` tab renders what
//! this module produces; degradation is in-band as [`PrView`].

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// What the `PR` tab shows: the resolved snapshot, or a degraded state with its own remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrView {
    /// Work is pending but has not crossed the loading-indicator delay.
    Pending,
    /// Work crossed the loading-indicator delay without producing a snapshot.
    Loading,
    /// An open (or merged/closed) PR resolved from the current branch's forge names.
    Pr(Box<PrSnapshot>),
    /// No PR resolves from the current branch's names.
    NoPr,
    /// `HEAD` is detached, so there is no branch identity to query.
    Detached,
    /// No PR resolved, but the pinned `HEAD` still contains the painted PR's head commit —
    /// the story stays on screen (`specs/forge-host.md` Refresh). Never stored: the app
    /// keeps its current snapshot when this arrives.
    Held,
    /// The resolved forge's CLI is not on `PATH`.
    NoCli(crate::git::Forge),
    /// The forge CLI is installed but misses the extension its reads require
    /// (`specs/forge-providers.md` — Azure DevOps).
    NoExtension(crate::git::Forge),
    /// The forge CLI is installed but not authenticated for this canonical host.
    NotAuthed(crate::git::Forge, String),
    /// Neither `upstream` nor `origin` names a recognized forge repository.
    NeedsForgeRemote,
    /// The fallback `origin` names a hosted forge outside the supported forge hosts.
    UnsupportedHost(String),
    /// The fallback `origin` names a supported host but not a valid repository path.
    MalformedOrigin(String),
    /// A local Git read failed before the forge fetch could start.
    GitError(String),
    /// Any other forge-CLI failure (rate limit, offline, …); the app freezes the last good view.
    Error(crate::git::Forge, String),
}

impl PrView {
    /// A same-input failure that can be retried without discarding the visible snapshot.
    /// Both snapshot preservation and the empty-state renderer consume this projection so a
    /// newly added retryable failure cannot diverge between those surfaces. `refresh` is the
    /// active `refresh` binding's hint key, so the advertised retry key follows a rebind.
    pub fn retry_remedy(&self, refresh: crate::keymap::Key) -> Option<String> {
        match self {
            Self::NoCli(forge) => Some(format!(
                "{} CLI not found. Install `{}`, then press {refresh}.",
                forge.display_name(),
                forge.cli()
            )),
            // Total over every forge: one without an extension concept still renders a
            // retryable message, never a missing remedy the render would trip over.
            Self::NoExtension(forge) => Some(match extension_hint(*forge) {
                Some(hint) => format!(
                    "{} CLI extension missing. Run {hint}, then press {refresh}.",
                    forge.display_name()
                ),
                None => format!(
                    "{} CLI extension missing. Press {refresh} to retry.",
                    forge.display_name()
                ),
            }),
            Self::NotAuthed(forge, host) => Some(format!(
                "Not signed in to {host}. Run {}, then press {refresh}.",
                login_hint(*forge, host)
            )),
            Self::GitError(message) => {
                Some(format!("Git read failed: {message}. Press {refresh} to retry."))
            }
            Self::Error(forge, message) => Some(format!(
                "{} unavailable: {message}. Press {refresh} to retry.",
                forge.display_name()
            )),
            _ => None,
        }
    }
}

/// The backticked login command the unauthenticated remedy advertises
/// (`specs/forge-providers.md`). Azure DevOps signs in per account, not per host.
fn login_hint(forge: crate::git::Forge, host: &str) -> String {
    match forge {
        crate::git::Forge::GitHub | crate::git::Forge::GitLab => {
            format!("`{} auth login --hostname {host}`", forge.cli())
        }
        crate::git::Forge::AzureDevOps => {
            "`az login` (or `az devops login` with a PAT)".to_string()
        }
    }
}

/// The backticked extension-install command the missing-extension remedy advertises
/// (`specs/forge-providers.md`). Only Azure DevOps' CLI carries a required extension.
fn extension_hint(forge: crate::git::Forge) -> Option<&'static str> {
    match forge {
        crate::git::Forge::AzureDevOps => Some("`az extension add --name azure-devops`"),
        crate::git::Forge::GitHub | crate::git::Forge::GitLab => None,
    }
}

/// One pull request's state, read fresh from the forge each poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnapshot {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The PR description as the forge returns it, empty when none (`specs/forge-host.md`).
    pub body: String,
    pub state: PrState,
    pub is_draft: bool,
    /// The PR's head branch name — the candidate that resolved, which may differ from the
    /// worktree's local branch name (`specs/forge-host.md`).
    pub head_ref: String,
    /// The head branch lives in another repository — a fork PR; shown as a marker so a
    /// same-named fork PR is visible.
    pub head_is_fork: bool,
    /// The PR's head commit — the hold gate's anchor, never rendered
    /// (`specs/forge-host.md` Refresh).
    pub head_oid: String,
    pub base_ref: String,
    pub merge: Merge,
    pub sync: Sync,
    pub checks: Vec<Check>,
    pub comments: Vec<Comment>,
    /// A capped surface (reviews/comments/threads/checks) had more rows than the 100-row fetch
    /// returned — the lists shown are a prefix, not the whole set. Drives a "more on the forge" marker.
    pub truncated: bool,
}

/// The PR lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// Whether the PR has a merge blocker worth surfacing, folded from each forge's merge-status
/// fields. Only the actionable blockers are modelled; states carrying nothing a reviewer acts
/// on — GitHub's `behind` / `unstable` / still-`checking`, for example — fold into `Clean`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    Clean,
    Conflicting,
    Blocked,
}

/// The local branch's position relative to the PR head (`head_oid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    InSync,
    /// Local `HEAD` is ahead of the PR head by N commits — the PR lags your local tree.
    Unpushed(u32),
    /// The PR head is ahead of local `HEAD` by N commits.
    Behind(u32),
    /// The PR head object is not available locally, so its relation to `HEAD` is unknowable.
    Unknown,
}

/// One CI check, the latest run for its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
}

/// A check's outcome, normalised across check runs and commit statuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Success,
    Failure,
    Running,
    Pending,
    Skipped,
}

/// One incoming comment: a PR-level review, a plain comment, or an inline finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub kind: CommentKind,
    pub author: String,
    pub author_is_bot: bool,
    /// `path:line` for a finding, the literal `review`/`comment` for the unanchored kinds.
    pub anchor: String,
    pub body: String,
    /// The finding's diff hunk as GitHub returns it; `None` for a review or comment.
    pub snippet: Option<String>,
    /// The post time as GitHub's ISO-8601 string (`…Z`), the newest-first sort key.
    pub created_at: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub reply_count: u32,
}

/// What a comment is anchored to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    Review,
    Comment,
    Finding,
}

impl PrSnapshot {
    /// The overall check rollup: any failure fails, else any still-running is running, else success.
    /// `None` when the PR has no checks.
    #[must_use]
    pub fn checks_rollup(&self) -> Option<CheckStatus> {
        if self.checks.is_empty() {
            return None;
        }
        if self.checks.iter().any(|c| c.status == CheckStatus::Failure) {
            return Some(CheckStatus::Failure);
        }
        if self
            .checks
            .iter()
            .any(|c| matches!(c.status, CheckStatus::Running | CheckStatus::Pending))
        {
            return Some(CheckStatus::Running);
        }
        Some(CheckStatus::Success)
    }

    /// How many checks have failed — the count behind the `✗ N failing` rollup label.
    #[must_use]
    pub fn failing_checks(&self) -> usize {
        self.checks.iter().filter(|c| c.status == CheckStatus::Failure).count()
    }
}

/// How one forge-CLI invocation failed, before any forge-specific classification.
#[derive(Debug)]
enum CliError {
    /// The CLI binary is not on `PATH`.
    NotFound,
    /// The CLI ran and exited non-zero; `stderr` carries its diagnostic.
    Failed { stderr: String },
    /// Spawning or waiting failed at the OS level.
    Io(String),
    /// The coordinator superseded this fetch mid-flight.
    Cancelled,
}

/// Run one prepared forge-CLI command to completion and return its stdout.
fn run_cli(cmd: &mut Command, cancelled: &AtomicBool) -> Result<String, CliError> {
    let child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::NotFound);
        }
        Err(error) => return Err(CliError::Io(error.to_string())),
    };

    // Drain both pipes while polling so a large response cannot fill a pipe and block the
    // child before it exits. A superseded config/fetch kills the process; the coordinator
    // keeps ownership until this worker reports completion, preserving one real fetch in flight.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let status = loop {
        if cancelled.load(Ordering::Acquire) {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(CliError::Io(error.to_string()));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if cancelled.load(Ordering::Acquire) {
        return Err(CliError::Cancelled);
    }
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    Err(CliError::Failed { stderr: String::from_utf8_lossy(&stderr).into_owned() })
}

/// Run explicitly targeted `gh` arguments in `repo` and return stdout or a classified failure.
fn gh(repo: &Path, host: &str, args: &[&str], cancelled: &AtomicBool) -> Result<String, GhError> {
    let mut cmd = Command::new("gh");
    cmd.current_dir(repo).args(args);
    run_provider(
        &mut cmd,
        cancelled,
        GhError::NoGh,
        |stderr| classify_failure(stderr, host),
        GhError::Other,
    )
}

/// Map a failed `gh`'s stderr to a degraded state by its wording — `gh` has no stable exit
/// codes for these. An unrecognised failure is `Other` → a transient `Error` view.
fn classify_failure(stderr: &str, host: &str) -> GhError {
    let s = stderr.to_lowercase();
    if s.contains("not logged")
        || s.contains("authentication")
        || s.contains("gh auth login")
        // The status marker, never a bare `401`: a commit OID or repository path carrying
        // those digits must not read as an expired token.
        || reports_status(&s, 401)
        || s.contains("bad credentials")
    {
        GhError::NotAuthed(host.to_owned())
    } else {
        GhError::Other(stderr.trim().to_string())
    }
}

/// Run one provider CLI read and map its failure shapes into the provider's error type:
/// a missing binary, a classified stderr, and the IO/cancellation tail every provider
/// folds into its retryable variant.
pub(crate) fn run_provider<E>(
    cmd: &mut Command,
    cancelled: &AtomicBool,
    not_found: E,
    classify: impl FnOnce(&str) -> E,
    other: impl Fn(String) -> E,
) -> Result<String, E> {
    match run_cli(cmd, cancelled) {
        Ok(stdout) => Ok(stdout),
        Err(CliError::NotFound) => Err(not_found),
        Err(CliError::Failed { stderr }) => Err(classify(&stderr)),
        Err(CliError::Io(error)) => Err(other(error)),
        Err(CliError::Cancelled) => Err(other("request cancelled".to_string())),
    }
}

/// Join a provider reader thread, degrading a panic into the caller's retryable error.
/// Propagating the panic would kill the fetch worker, and the tab would wait on a
/// completion never sent.
pub(crate) fn join_read<T, E>(
    handle: std::thread::ScopedJoinHandle<'_, Result<T, E>>,
    on_panic: impl FnOnce() -> E,
) -> Result<T, E> {
    handle.join().unwrap_or_else(|_| Err(on_panic()))
}

/// The newest `SURFACE_CAP` of `rows`, which arrive oldest-first — the shared tail cut
/// behind every surface's cap (`specs/forge-host.md`).
pub(crate) fn newest_capped<T>(mut rows: Vec<T>) -> Vec<T> {
    let keep = rows.len().min(SURFACE_CAP);
    rows.split_off(rows.len() - keep)
}

/// Each surface reads at most this many rows, never paged to exhaustion
/// (`specs/forge-host.md`).
pub(crate) const SURFACE_CAP: usize = 100;

/// Whether the CLI reported HTTP `code` somewhere that means a status: the `(http <code>)`
/// marker both `glab` and `gh` append to a failed request, or the leading token of an error
/// line. A commit OID or a repository path that merely contains those digits never qualifies,
/// so a transport failure stays a retryable error instead of reading as absence.
pub(crate) fn reports_status(lowercased_stderr: &str, code: u16) -> bool {
    let code = code.to_string();
    let marker = lowercased_stderr
        .split_once("(http ")
        .and_then(|(_, rest)| rest.split(')').next())
        .map(str::trim);
    if marker == Some(code.as_str()) {
        return true;
    }
    lowercased_stderr.lines().any(|line| {
        let line = line.trim().trim_start_matches("glab: ").trim_start_matches("gh: ");
        let line = line.trim_start_matches("{\"message\":\"").trim_start_matches('"');
        // `404 not found` and `http 404` both lead a status line. A URL cannot: `https://` has
        // no space, and an OID's digits never start the line.
        let line = line.strip_prefix("http ").unwrap_or(line);
        line.strip_prefix(&code).is_some_and(|rest| rest.starts_with(' ') || rest.is_empty())
    })
}

/// A classified `gh` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug, PartialEq, Eq)]
enum GhError {
    NoGh,
    NotAuthed(String),
    LocalGit(String),
    Other(String),
}

impl From<GhError> for PrView {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NoGh => PrView::NoCli(crate::git::Forge::GitHub),
            GhError::NotAuthed(host) => PrView::NotAuthed(crate::git::Forge::GitHub, host),
            GhError::LocalGit(message) => PrView::GitError(message),
            GhError::Other(m) => PrView::Error(crate::git::Forge::GitHub, m),
        }
    }
}

/// The derived local state that determines one PR fetch.
pub use crate::git::PrFetchInput;

/// A local Git failure before a GitHub fetch starts.
#[derive(Debug, PartialEq, Eq)]
pub enum PrInputError {
    /// The repository target could not be proven, so no existing snapshot is attributable.
    TargetRead(String),
    /// Branch state failed after this repository target was proven.
    BranchState { target: crate::git::RepoTarget, message: String },
}

/// Derive one complete fetch input from local Git and one validated config snapshot.
pub fn fetch_input(
    repo: &Path,
    base: Option<&str>,
    config: &crate::config::PluginConfig,
) -> Result<PrFetchInput, PrInputError> {
    fetch_input_inner(repo, base, config, false)
}

/// Re-derive a completed fetch's input, confirming its repository again after the branch reads.
pub(crate) fn verify_input(
    repo: &Path,
    base: Option<&str>,
    config: &crate::config::PluginConfig,
) -> Result<PrFetchInput, PrInputError> {
    fetch_input_inner(repo, base, config, true)
}

fn fetch_input_inner(
    repo: &Path,
    base: Option<&str>,
    config: &crate::config::PluginConfig,
    verify_repository: bool,
) -> Result<PrFetchInput, PrInputError> {
    let (repository, origin_repository) =
        crate::git::remote_identities(repo, &config.forge_hosts())
            .map_err(|error| PrInputError::TargetRead(error.0))?;
    let crate::git::RepositoryIdentity::Repository(target) = &repository else {
        return Ok(PrFetchInput {
            repository,
            origin_repository: None,
            local: crate::git::PrLocalState::default(),
        });
    };
    let local = match crate::git::pr_local(repo, base) {
        Ok(local) => local,
        Err(error) => {
            let (current, _) = crate::git::remote_identities(repo, &config.forge_hosts())
                .map_err(|read_error| PrInputError::TargetRead(read_error.0))?;
            if current != repository {
                return Err(PrInputError::TargetRead(
                    "repository changed while reading branch state".to_string(),
                ));
            }
            return Err(PrInputError::BranchState { target: target.clone(), message: error.0 });
        }
    };
    let (repository, origin_repository) = if verify_repository {
        crate::git::remote_identities(repo, &config.forge_hosts())
            .map_err(|error| PrInputError::TargetRead(error.0))?
    } else {
        (repository, origin_repository)
    };
    Ok(PrFetchInput { repository, origin_repository, local })
}

/// Read GitHub for one already-derived input. Degradation stays in-band for the PR tab.
#[must_use]
pub fn fetch(repo: &Path, input: &PrFetchInput) -> PrView {
    fetch_cancellable(repo, input, &AtomicBool::new(false))
}

/// Read GitHub with a cancellation signal owned by the event-loop coordinator.
#[must_use]
pub(crate) fn fetch_cancellable(
    repo: &Path,
    input: &PrFetchInput,
    cancelled: &AtomicBool,
) -> PrView {
    match fetch_inner(repo, input, cancelled) {
        Ok(view) => view,
        Err(error) => error.into(),
    }
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    cancelled: &AtomicBool,
) -> Result<PrView, GhError> {
    let repository = match &input.repository {
        crate::git::RepositoryIdentity::Repository(target) => target,
        crate::git::RepositoryIdentity::Missing | crate::git::RepositoryIdentity::Hostless => {
            return Ok(PrView::NeedsForgeRemote);
        }
        crate::git::RepositoryIdentity::Unsupported(host) => {
            return Ok(PrView::UnsupportedHost(host.clone()));
        }
        crate::git::RepositoryIdentity::Malformed(host) => {
            return Ok(PrView::MalformedOrigin(host.clone()));
        }
    };
    if input.local.detached {
        // A detached HEAD (e.g. after `gh pr merge --delete-branch`) has no pin.
        return Ok(PrView::Detached);
    }
    // Exhaustive per-forge dispatch: a new forge must be routed here before it builds
    // (`specs/forge-providers.md`). Each provider owns its whole read and degrades in-band.
    match repository.forge() {
        crate::git::Forge::GitLab => {
            return Ok(crate::gitlab::fetch(repo, input, repository, cancelled));
        }
        crate::git::Forge::AzureDevOps => {
            return Ok(crate::azure_devops::fetch(repo, input, repository, cancelled));
        }
        crate::git::Forge::GitHub => {}
    }
    let target = FetchTarget {
        repo,
        host: repository.host(),
        owner: repository.owner(),
        name: repository.name(),
        cancelled,
    };
    // A fork clone: `origin` is the fork, the target is upstream. Both repositories are
    // asked, and upstream's pick outranks the fork's own (`specs/forge-host.md`).
    let fork = fork_repository(input.origin_repository.as_ref(), repository);
    let head = input.local.head_oid.as_deref();
    let assoc =
        branch_lookup(&target, fork.map(crate::git::RepoTarget::owner), &input.local.names)?;
    let mut pick = resolve_pick(repo, &assoc, head)
        .map_err(|error| GhError::LocalGit(error.0))?
        .map(|number| (number, repository));
    if pick.is_none()
        && let Some(fork_repo) = fork
    {
        let fork_target = FetchTarget {
            repo,
            host: fork_repo.host(),
            owner: fork_repo.owner(),
            name: fork_repo.name(),
            cancelled,
        };
        let fork_assoc = branch_lookup(&fork_target, None, &input.local.names)?;
        pick = resolve_pick(repo, &fork_assoc, head)
            .map_err(|error| GhError::LocalGit(error.0))?
            .map(|number| (number, fork_repo));
    }
    let Some((number, detail_repo)) = pick else {
        return Ok(PrView::NoPr);
    };
    let target = FetchTarget {
        repo,
        host: detail_repo.host(),
        owner: detail_repo.owner(),
        name: detail_repo.name(),
        cancelled,
    };
    let detail = pr_detail(&target, number)?;
    let node = &detail["data"]["repository"]["pullRequest"];
    if node.is_null() {
        return Ok(PrView::NoPr);
    }
    // Sync compares the fetch's pinned HEAD to the PR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's PR with another branch's count.
    let pr_head = node["headRefOid"].as_str().unwrap_or_default();
    let sync = local_sync(repo, input.local.head_oid.as_deref(), pr_head)
        .map_err(|error| GhError::LocalGit(error.0))?;
    Ok(PrView::Pr(Box::new(build_snapshot(node, sync))))
}

/// The local sync against the PR's reported head: `Unknown` when either side is unpinned,
/// otherwise the ahead/behind derivation. Shared by all three providers so the unpinned
/// handling cannot drift.
pub(crate) fn local_sync(
    repo: &Path,
    pin: Option<&str>,
    pr_head: &str,
) -> Result<Sync, crate::git::GitFail> {
    match pin {
        Some(pin) if !pr_head.is_empty() => {
            Ok(derive_sync(crate::git::ahead_behind_oids(repo, pin, pr_head)?))
        }
        _ => Ok(Sync::Unknown),
    }
}

/// The local branch's position relative to the PR head, from `git`'s ahead/behind counts. A
/// diverged branch (both nonzero) leads with the unpushed count — the headline case. `None`
/// (the PR head isn't local yet) stays explicitly unknown rather than guessing.
pub(crate) fn derive_sync(ahead_behind: Option<(u32, u32)>) -> Sync {
    match ahead_behind {
        None => Sync::Unknown,
        Some((0, 0)) => Sync::InSync,
        Some((0, behind)) => Sync::Behind(behind),
        Some((ahead, _)) => Sync::Unpushed(ahead),
    }
}

struct FetchTarget<'a> {
    repo: &'a Path,
    host: &'a str,
    owner: &'a str,
    name: &'a str,
    cancelled: &'a AtomicBool,
}

/// One PR from the branch lookup, reduced to the pick-relevant fields.
#[derive(Debug)]
pub struct AssocPr {
    pub(crate) number: u64,
    pub(crate) head_oid: String,
    /// Consulted only by providers whose lookup is not branch-filtered server-side
    /// (Azure DevOps); GitHub and GitLab filter in the query itself.
    pub(crate) head_ref: String,
    pub(crate) created_at: String,
    /// The history sort key: the merge or close time. Empty for an open PR.
    pub(crate) closed_at: String,
    /// The lookup's full payload node, when it already is the complete pull request —
    /// Azure DevOps lists full nodes, so its picks need no detail read. `None` when the
    /// lookup returns reduced fields, as GitHub's and GitLab's do.
    pub(crate) raw: Option<Value>,
}

/// The branch's pull requests, split by lifecycle: open, and finished (merged or closed)
/// history candidates behind the ancestry guard.
#[derive(Debug, Default)]
pub struct Association {
    pub open: Vec<AssocPr>,
    pub history: Vec<AssocPr>,
}

/// The GitHub branch lookup: one aliased `pullRequests(headRefName:)` block per name,
/// every lifecycle state, newest first (`specs/forge-providers.md`). `fork_head_owner`
/// is the head filter: `None` keeps only same-repository heads; `Some(owner)` keeps only
/// heads living in that owner's fork. Values ride as variables, never in the query text.
fn branch_lookup(
    target: &FetchTarget<'_>,
    fork_head_owner: Option<&str>,
    names: &[String],
) -> Result<Association, GhError> {
    let q = build_branch_query(names.len());
    let mut vars = vec![
        ("o".to_string(), target.owner.to_string()),
        ("n".to_string(), target.name.to_string()),
    ];
    for (i, name) in names.iter().enumerate() {
        vars.push((format!("b{i}"), name.clone()));
    }
    let v = graphql(target.repo, target.host, &q, &vars, target.cancelled)?;
    Ok(parse_branch_lookup(&v, names.len(), fork_head_owner))
}

/// The branch-lookup query text: per name, an open block (`o{i}`) apart from the finished
/// block (`h{i}`), each newest-created-first and capped at 20. Open PRs get their own page
/// so `resolve_pick`'s open-before-history precedence never loses an older still-open PR
/// behind a deep finished history on a reused name.
fn build_branch_query(names: usize) -> String {
    use std::fmt::Write;
    let mut q = String::from("query($o:String!,$n:String!");
    for i in 0..names {
        let _ = write!(q, ",$b{i}:String!");
    }
    q.push_str("){repository(owner:$o,name:$n){");
    let fields = "first:20, orderBy:{field:CREATED_AT, direction:DESC}){nodes{\
                  number state headRefOid headRefName createdAt closedAt \
                  isCrossRepository headRepositoryOwner{login}}} ";
    for i in 0..names {
        let _ = write!(q, "o{i}:pullRequests(headRefName:$b{i}, states:[OPEN], {fields}");
        let _ = write!(q, "h{i}:pullRequests(headRefName:$b{i}, states:[MERGED,CLOSED], {fields}");
    }
    q.push_str("}}");
    q
}

/// Split the branch lookup by lifecycle. A node is this branch's only when its head lives
/// in the queried repository — or, under a fork filter, in that fork — so a stranger's
/// same-named fork branch never attaches (`specs/forge-host.md` Resolution). Duplicates
/// across name aliases collapse.
fn parse_branch_lookup(v: &Value, aliases: usize, fork_head_owner: Option<&str>) -> Association {
    let mut assoc = Association::default();
    let keys = (0..aliases).flat_map(|i| [format!("o{i}"), format!("h{i}")]);
    for key in keys {
        let nodes = &v["data"]["repository"][key.as_str()]["nodes"];
        for node in nodes.as_array().into_iter().flatten() {
            let cross = node["isCrossRepository"].as_bool() == Some(true);
            let head_owner = node["headRepositoryOwner"]["login"].as_str().unwrap_or_default();
            let state = node["state"].as_str().unwrap_or_default();
            let admitted = match fork_head_owner {
                None => !cross,
                // A deleted fork nulls the head owner; its merged PR still admits, and
                // the history ancestry guard keeps strangers out (`specs/forge-host.md`).
                Some(owner) => {
                    cross
                        && (head_owner.eq_ignore_ascii_case(owner)
                            || (head_owner.is_empty() && state != "OPEN"))
                }
            };
            if !admitted {
                continue;
            }
            let Some(number) = node["number"].as_u64() else { continue };
            let pr = AssocPr {
                number,
                head_oid: node["headRefOid"].as_str().unwrap_or_default().to_string(),
                head_ref: node["headRefName"].as_str().unwrap_or_default().to_string(),
                created_at: node["createdAt"].as_str().unwrap_or_default().to_string(),
                closed_at: node["closedAt"].as_str().unwrap_or_default().to_string(),
                // A lookup node is a reduced row, never the full pull request.
                raw: None,
            };
            match state {
                "OPEN" => push_unique(&mut assoc.open, pr),
                "MERGED" | "CLOSED" => push_unique(&mut assoc.history, pr),
                _ => {}
            }
        }
    }
    assoc
}

/// A finished-history row for integration tests: only the fields the pick consults.
pub fn assoc_history(number: u64, head_oid: &str, closed_at: &str) -> AssocPr {
    AssocPr {
        number,
        head_oid: head_oid.to_string(),
        head_ref: String::new(),
        created_at: String::new(),
        closed_at: closed_at.to_string(),
        raw: None,
    }
}

/// The fork this clone works from, when `origin` is a same-host repository other than the
/// target — the dual-query trigger (`specs/forge-host.md` Resolution). One definition, so
/// the providers cannot drift on what counts as a fork.
pub(crate) fn fork_repository<'a>(
    origin: Option<&'a crate::git::RepoTarget>,
    target: &crate::git::RepoTarget,
) -> Option<&'a crate::git::RepoTarget> {
    origin.filter(|origin| origin.host() == target.host() && *origin != target)
}

/// Push `pr` unless its number is already in `bucket` — a PR's identity is its number.
pub(crate) fn push_unique(bucket: &mut Vec<AssocPr>, pr: AssocPr) {
    if !bucket.iter().any(|have| have.number == pr.number) {
        bucket.push(pr);
    }
}

/// Resolve the branch's PR: the newest open one wins; with none, the newest finished one
/// whose head commit the pinned `HEAD` contains — the reused-name guard; with neither,
/// nothing (`specs/forge-host.md` Resolution). The one enforcement site of that precedence
/// for every provider.
pub fn resolve_pick(
    repo: &Path,
    assoc: &Association,
    head: Option<&str>,
) -> Result<Option<u64>, crate::git::GitFail> {
    if let Some(number) = newest_by(&assoc.open, |pr| &pr.created_at) {
        return Ok(Some(number));
    }
    let Some(head) = head else { return Ok(None) };
    let mut history: Vec<&AssocPr> = assoc.history.iter().collect();
    history.sort_by(|a, b| b.closed_at.cmp(&a.closed_at));
    // Each candidate costs git subprocesses; a churny shared name must not turn one
    // fetch into a hundred spawns. Ten newest is ample for any real branch.
    history.truncate(10);
    for pr in history {
        if !pr.head_oid.is_empty() && crate::git::contains_commit(repo, head, &pr.head_oid)? {
            return Ok(Some(pr.number));
        }
    }
    Ok(None)
}

/// The PR with the newest `key` timestamp. ISO-8601 `…Z` strings compare lexically; a
/// strict `>` keeps the earlier entry on a tie, so the pick is deterministic.
fn newest_by(prs: &[AssocPr], key: impl Fn(&AssocPr) -> &str) -> Option<u64> {
    let mut best: Option<&AssocPr> = None;
    for pr in prs {
        if best.is_none_or(|b| key(pr) > key(b)) {
            best = Some(pr);
        }
    }
    best.map(|pr| pr.number)
}

/// All of one PR's state in a single direct GraphQL call — identity, mergeability, checks,
/// reviews, plain comments, and review threads. Each list surface reads its newest 100 rows
/// (`last:100`, flagged by `hasPreviousPage`) — ample for any real PR in a review pane —
/// and flags a fuller surface so the UI can mark it, rather than paging to exhaustion
/// (`specs/forge-host.md`). Checks keep `first:100`/`hasNextPage`.
fn pr_detail(target: &FetchTarget<'_>, number: u64) -> Result<Value, GhError> {
    let q = build_detail_query(number);
    let vars = vec![
        ("o".to_string(), target.owner.to_string()),
        ("n".to_string(), target.name.to_string()),
    ];
    graphql(target.repo, target.host, &q, &vars, target.cancelled)
}

/// Project one PR directly, including fork identity and capped check/comment surfaces.
fn build_detail_query(number: u64) -> String {
    format!(
        "query($o:String!,$n:String!){{repository(owner:$o,name:$n){{\
         pullRequest(number:{number}){{\
         number title url body isDraft state mergeable mergeStateStatus baseRefName headRefName \
         headRefOid isCrossRepository \
         commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:100){{pageInfo{{hasNextPage}} nodes{{__typename \
         ... on CheckRun{{name status conclusion}} ... on StatusContext{{context state}}}}}}}}}}}}}} \
         reviews(last:100){{pageInfo{{hasPreviousPage}} nodes{{author{{login}} body submittedAt}}}} \
         comments(last:100){{pageInfo{{hasPreviousPage}} nodes{{author{{login}} body createdAt}}}} \
         reviewThreads(last:100){{pageInfo{{hasPreviousPage}} nodes{{isResolved isOutdated path line \
         comments(first:1){{totalCount nodes{{author{{login}} body createdAt diffHunk}}}}}}}}}}}}}}"
    )
}

/// Run a GraphQL `query` with `vars` and parse the response. Every variable is passed with
/// `-f` (raw string) — `-F` type-coerces, so a branch literally named `123` would arrive
/// as an Int and fail its `String!` declaration.
fn graphql(
    repo: &Path,
    host: &str,
    query: &str,
    vars: &[(String, String)],
    cancelled: &AtomicBool,
) -> Result<Value, GhError> {
    let args = graphql_args(host, query, vars);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = gh(repo, host, &arg_refs, cancelled)?;
    serde_json::from_str(&out).map_err(|e| GhError::Other(e.to_string()))
}

fn graphql_args(host: &str, query: &str, vars: &[(String, String)]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "api".to_string(),
        "graphql".to_string(),
        "--hostname".to_string(),
        host.to_owned(),
        "-f".to_string(),
        format!("query={query}"),
    ];
    for (key, value) in vars {
        args.push("-f".to_string());
        args.push(format!("{key}={value}"));
    }
    args
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// Assemble the snapshot from the `gh pr view` JSON, the computed `sync`, and the merged comments.
fn build_snapshot(node: &Value, sync: Sync) -> PrSnapshot {
    let contexts = &node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
    let rollup = &contexts["nodes"];
    // A surface whose page reports more in the direction it pages is a prefix, not the whole set.
    // Each query asks only for its own flag — `hasPreviousPage` for the `last:` lists,
    // `hasNextPage` for checks — so OR-ing both reads whichever applies; the absent one is false.
    let more = |conn: &Value| {
        conn["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false)
            || conn["pageInfo"]["hasPreviousPage"].as_bool().unwrap_or(false)
    };
    let truncated = more(contexts)
        || more(&node["reviews"])
        || more(&node["comments"])
        || more(&node["reviewThreads"]);
    PrSnapshot {
        number: node["number"].as_u64().unwrap_or_default(),
        title: node["title"].as_str().unwrap_or_default().to_string(),
        url: node["url"].as_str().unwrap_or_default().to_string(),
        body: node["body"].as_str().unwrap_or_default().to_string(),
        state: parse_state(node["state"].as_str().unwrap_or("OPEN")),
        is_draft: node["isDraft"].as_bool().unwrap_or(false),
        head_ref: node["headRefName"].as_str().unwrap_or_default().to_string(),
        head_is_fork: node["isCrossRepository"].as_bool().unwrap_or(false),
        head_oid: node["headRefOid"].as_str().unwrap_or_default().to_string(),
        base_ref: node["baseRefName"].as_str().unwrap_or_default().to_string(),
        merge: derive_merge(node["mergeable"].as_str(), node["mergeStateStatus"].as_str()),
        sync,
        checks: normalize_checks(rollup),
        comments: merge_comments(
            &node["reviews"]["nodes"],
            &node["comments"]["nodes"],
            &node["reviewThreads"]["nodes"],
        ),
        truncated,
    }
}

fn parse_state(s: &str) -> PrState {
    match s {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    }
}

/// Fold GitHub's `mergeable` and `mergeStateStatus` into a [`Merge`]. Only the actionable
/// blockers are surfaced: conflicts and a `blocked` required gate. Everything else — `clean`,
/// `behind`, `unstable`, and still-`unknown` (computing) — folds into `Clean` (shows nothing).
fn derive_merge(mergeable: Option<&str>, state: Option<&str>) -> Merge {
    match (mergeable, state) {
        (Some("CONFLICTING"), _) | (_, Some("DIRTY")) => Merge::Conflicting,
        (_, Some("BLOCKED")) => Merge::Blocked,
        _ => Merge::Clean,
    }
}

/// Insert or replace by name — the latest run for a check name wins, so a re-run
/// replaces its earlier entry. Shared by every provider's checks assembly.
pub(crate) fn upsert_latest(checks: &mut Vec<Check>, check: Check) {
    if let Some(slot) = checks.iter_mut().find(|c| c.name == check.name) {
        *slot = check;
    } else {
        checks.push(check);
    }
}

/// The shared comment finish: collapse each bot's PR-level posts to its latest, then order
/// newest first — ISO-8601 `…Z` strings sort lexically in chronological order
/// (`specs/forge-host.md`).
pub(crate) fn finish_comments(out: &mut Vec<Comment>) {
    dedup_bot_prose(out);
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
}

/// The latest run per check name, normalised from check runs and commit statuses.
fn normalize_checks(rollup: &Value) -> Vec<Check> {
    let mut out: Vec<Check> = Vec::new();
    for node in rollup.as_array().into_iter().flatten() {
        let name =
            node["name"].as_str().or_else(|| node["context"].as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let status = check_status(node);
        upsert_latest(&mut out, Check { name, status });
    }
    out
}

/// Normalise one check node — a check run (`status`/`conclusion`) or a commit status (`state`)
/// — to a [`CheckStatus`].
fn check_status(node: &Value) -> CheckStatus {
    // Check runs carry `status`/`conclusion`; commit statuses carry `state`.
    if let Some(state) = node["state"].as_str() {
        return match state {
            "SUCCESS" => CheckStatus::Success,
            "FAILURE" | "ERROR" => CheckStatus::Failure,
            _ => CheckStatus::Pending,
        };
    }
    match node["status"].as_str() {
        Some("COMPLETED") => match node["conclusion"].as_str() {
            Some("SUCCESS") => CheckStatus::Success,
            Some("SKIPPED" | "NEUTRAL") => CheckStatus::Skipped,
            // FAILURE / TIMED_OUT / CANCELLED / ACTION_REQUIRED / a missing conclusion all read
            // as a failed check — something needs attention.
            _ => CheckStatus::Failure,
        },
        Some("IN_PROGRESS") => CheckStatus::Running,
        _ => CheckStatus::Pending,
    }
}

/// Merge the three comment surfaces (GraphQL `reviews`, `comments`, and `reviewThreads` node
/// arrays) into one newest-first list, keeping only a bot's latest PR-level post and each human's.
fn merge_comments(reviews: &Value, issues: &Value, threads: &Value) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();

    // Submitted reviews with a non-empty body (the PR-level `review` cards).
    for r in reviews.as_array().into_iter().flatten() {
        let body = r["body"].as_str().unwrap_or("").trim().to_string();
        if body.is_empty() {
            continue;
        }
        out.push(prose_comment(CommentKind::Review, &r["author"], body, r["submittedAt"].as_str()));
    }

    // Plain conversation comments (the `comment` cards).
    for c in issues.as_array().into_iter().flatten() {
        let body = c["body"].as_str().unwrap_or("").trim().to_string();
        if body.is_empty() {
            continue;
        }
        out.push(prose_comment(CommentKind::Comment, &c["author"], body, c["createdAt"].as_str()));
    }

    // Inline review threads (the `finding` cards), with resolved/outdated and replies.
    for t in threads.as_array().into_iter().flatten() {
        let root = &t["comments"]["nodes"][0];
        let login = root["author"]["login"].as_str().unwrap_or("").to_string();
        let path = t["path"].as_str().unwrap_or("");
        let anchor = match t["line"].as_u64() {
            Some(line) => format!("{path}:{line}"),
            None => path.to_string(),
        };
        out.push(Comment {
            kind: CommentKind::Finding,
            author_is_bot: is_bot(&login),
            author: login,
            anchor,
            body: root["body"].as_str().unwrap_or("").trim().to_string(),
            snippet: root["diffHunk"].as_str().filter(|h| !h.is_empty()).map(str::to_string),
            created_at: root["createdAt"].as_str().unwrap_or("").to_string(),
            is_resolved: t["isResolved"].as_bool().unwrap_or(false),
            is_outdated: t["isOutdated"].as_bool().unwrap_or(false),
            reply_count: t["comments"]["totalCount"].as_u64().unwrap_or(1).saturating_sub(1) as u32,
        });
    }

    finish_comments(&mut out);
    out
}

fn prose_comment(
    kind: CommentKind,
    user: &Value,
    body: String,
    created_at: Option<&str>,
) -> Comment {
    let login = user["login"].as_str().unwrap_or("").to_string();
    let bot = is_bot(&login);
    prose_row(kind, login, bot, body, created_at.unwrap_or("").to_string())
}

/// One PR-level prose row with the defaults every non-`finding` comment shares. Both
/// providers build their `review`/`comment` rows through this one shape.
pub(crate) fn prose_row(
    kind: CommentKind,
    author: String,
    author_is_bot: bool,
    body: String,
    created_at: String,
) -> Comment {
    let anchor = match kind {
        CommentKind::Review => "review",
        _ => "comment",
    };
    Comment {
        kind,
        author_is_bot,
        author,
        anchor: anchor.to_string(),
        body,
        snippet: None,
        created_at,
        is_resolved: false,
        is_outdated: false,
        reply_count: 0,
    }
}

/// Keep only the latest PR-level (`review`/`comment`) post per bot author; humans keep all.
fn dedup_bot_prose(out: &mut Vec<Comment>) {
    let mut keep_newest: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for c in out.iter() {
        if c.author_is_bot && c.kind != CommentKind::Finding {
            let e = keep_newest.entry(c.author.clone()).or_default();
            if c.created_at > *e {
                e.clone_from(&c.created_at);
            }
        }
    }
    out.retain(|c| {
        !(c.author_is_bot && c.kind != CommentKind::Finding)
            // An undated review is a standing verdict, not repeated prose — GitLab's approvals
            // surface carries no timestamp — so it never loses newest-wins to a dated post.
            || (c.kind == CommentKind::Review && c.created_at.is_empty())
            || keep_newest.get(&c.author) == Some(&c.created_at)
    });
}

/// Percent-encode one URL path or query value. A GitLab project path's `/` separators encode
/// to `%2F`, which is how its API addresses a project by path; an Azure DevOps browse link
/// re-encodes the space a decoded project name carries.
pub(crate) fn urlencode(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Whether a GitHub login is an app/bot (`…[bot]`).
fn is_bot(login: &str) -> bool {
    login.ends_with("[bot]")
}

/// The shared name-only bot heuristics for forges that carry no bot flag: the `[bot]`
/// suffix, or a `-bot` suffix. The hyphen is load-bearing: it admits `gitlab-bot` while a
/// human `Talbot` stays human.
pub(crate) fn is_named_bot(name: &str) -> bool {
    is_bot(name) || name.to_ascii_lowercase().ends_with("-bot")
}

/// A relative age label (`5m`, `2h`, `3d`, `2w`) from an ISO-8601 `…Z` timestamp, against `now`.
/// `now` is injected so the formatting is testable; the UI passes `SystemTime::now()`.
#[must_use]
pub fn relative_age(created_at: &str, now: SystemTime) -> String {
    let Some(then) = parse_iso(created_at) else {
        return String::new();
    };
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let secs = (now - then).max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 604_800 => format!("{}d", s / 86_400),
        s => format!("{}w", s / 604_800),
    }
}

/// Parse a fixed `YYYY-MM-DDTHH:MM:SSZ` timestamp to a Unix epoch second. `None` on any
/// deviation, so a malformed value yields an empty age rather than a wrong one.
// The civil-from-days algorithm reads naturally with the conventional short field names.
#[allow(clippy::many_single_char_names)]
fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, se) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // Days from the civil date (Howard Hinnant's algorithm), then to seconds.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let year_of_era = y - era * 400;
    let day_of_year = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_surfaces_only_conflicts_and_blocked() {
        assert_eq!(derive_merge(Some("CONFLICTING"), Some("DIRTY")), Merge::Conflicting);
        assert_eq!(derive_merge(Some("MERGEABLE"), Some("BLOCKED")), Merge::Blocked);
        // Everything non-actionable folds into Clean: clean, behind, unstable, still-computing.
        assert_eq!(derive_merge(Some("MERGEABLE"), Some("CLEAN")), Merge::Clean);
        assert_eq!(derive_merge(Some("MERGEABLE"), Some("BEHIND")), Merge::Clean);
        assert_eq!(derive_merge(Some("MERGEABLE"), Some("UNSTABLE")), Merge::Clean);
        assert_eq!(derive_merge(Some("UNKNOWN"), Some("UNKNOWN")), Merge::Clean);
        // DIRTY means conflicts even while mergeability is still UNKNOWN or the field is missing.
        assert_eq!(derive_merge(Some("UNKNOWN"), Some("DIRTY")), Merge::Conflicting);
        assert_eq!(derive_merge(None, Some("DIRTY")), Merge::Conflicting);
        assert_eq!(derive_merge(None, None), Merge::Clean);
    }

    #[test]
    fn parse_state_maps_the_three_github_lifecycles() {
        assert_eq!(parse_state("MERGED"), PrState::Merged);
        assert_eq!(parse_state("CLOSED"), PrState::Closed);
        assert_eq!(parse_state("OPEN"), PrState::Open);
        assert_eq!(parse_state("anything-else"), PrState::Open); // default is the live case
    }

    #[test]
    fn truncated_flips_when_any_capped_surface_has_a_next_page() {
        let base = serde_json::json!({
            "number": 1, "title": "t", "url": "u", "state": "OPEN", "isDraft": false,
            "baseRefName": "main", "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "commits": {"nodes": [{"commit": {"statusCheckRollup":
                {"contexts": {"pageInfo": {"hasNextPage": false}, "nodes": []}}}}]},
            "reviews": {"pageInfo": {"hasNextPage": false}, "nodes": []},
            "comments": {"pageInfo": {"hasNextPage": false}, "nodes": []},
            "reviewThreads": {"pageInfo": {"hasNextPage": false}, "nodes": []}
        });
        assert!(
            !build_snapshot(&base, Sync::InSync).truncated,
            "all pages complete → not truncated"
        );
        // The description parses when present and stays empty when GitHub returns null.
        assert_eq!(build_snapshot(&base, Sync::InSync).body, "");
        let mut with_body = base.clone();
        with_body["body"] = serde_json::json!("## Summary\nfixes things");
        assert_eq!(build_snapshot(&with_body, Sync::InSync).body, "## Summary\nfixes things");

        // Comments and threads read `last:100`, so their "more exist" flag pages backward.
        let mut comments_more = base.clone();
        comments_more["comments"]["pageInfo"]["hasPreviousPage"] = serde_json::json!(true);
        assert!(build_snapshot(&comments_more, Sync::InSync).truncated);

        let mut threads_more = base.clone();
        threads_more["reviewThreads"]["pageInfo"]["hasPreviousPage"] = serde_json::json!(true);
        assert!(build_snapshot(&threads_more, Sync::InSync).truncated);

        let mut checks_more = base.clone();
        checks_more["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["pageInfo"]
            ["hasNextPage"] = serde_json::json!(true);
        assert!(build_snapshot(&checks_more, Sync::InSync).truncated);

        // `reviews` pages backward (last:100), so its "more exist" flag is `hasPreviousPage` —
        // checking `hasNextPage` here (the old bug) would leave this surface never marked.
        let mut reviews_more = base.clone();
        reviews_more["reviews"]["pageInfo"]["hasPreviousPage"] = serde_json::json!(true);
        assert!(build_snapshot(&reviews_more, Sync::InSync).truncated);
    }

    #[test]
    fn checks_take_the_latest_run_per_name() {
        let rollup = serde_json::json!([
            {"__typename": "CheckRun", "name": "tests", "status": "COMPLETED", "conclusion": "FAILURE"},
            {"__typename": "CheckRun", "name": "tests", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "name": "build", "status": "IN_PROGRESS"},
            {"__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "SKIPPED"},
            {"__typename": "CheckRun", "name": "codeql", "status": "COMPLETED", "conclusion": "NEUTRAL"},
            {"__typename": "StatusContext", "context": "deploy", "state": "PENDING"}
        ]);
        let checks = normalize_checks(&rollup);
        assert_eq!(checks.len(), 5);
        let tests = checks.iter().find(|c| c.name == "tests").unwrap();
        assert_eq!(tests.status, CheckStatus::Success); // the re-run won
        assert_eq!(checks.iter().find(|c| c.name == "build").unwrap().status, CheckStatus::Running);
        // SKIPPED and NEUTRAL both fold to Skipped — neither fails nor blocks the rollup.
        assert_eq!(checks.iter().find(|c| c.name == "lint").unwrap().status, CheckStatus::Skipped);
        assert_eq!(
            checks.iter().find(|c| c.name == "codeql").unwrap().status,
            CheckStatus::Skipped
        );
        assert_eq!(
            checks.iter().find(|c| c.name == "deploy").unwrap().status,
            CheckStatus::Pending
        );
    }

    #[test]
    fn rollup_fails_on_any_failure_else_running_else_success() {
        let snap = |statuses: &[CheckStatus]| PrSnapshot {
            number: 1,
            title: String::new(),
            url: String::new(),
            body: String::new(),
            state: PrState::Open,
            is_draft: false,
            head_ref: String::new(),
            head_is_fork: false,
            head_oid: String::new(),
            base_ref: String::new(),
            merge: Merge::Clean,
            sync: Sync::InSync,
            checks: statuses.iter().map(|&s| Check { name: "c".into(), status: s }).collect(),
            comments: Vec::new(),
            truncated: false,
        };
        assert_eq!(snap(&[]).checks_rollup(), None);
        assert_eq!(
            snap(&[CheckStatus::Success, CheckStatus::Success]).checks_rollup(),
            Some(CheckStatus::Success)
        );
        assert_eq!(
            snap(&[CheckStatus::Success, CheckStatus::Running]).checks_rollup(),
            Some(CheckStatus::Running)
        );
        assert_eq!(
            snap(&[CheckStatus::Running, CheckStatus::Failure]).checks_rollup(),
            Some(CheckStatus::Failure)
        );
    }

    fn input(head: &str, names: &[&str]) -> PrFetchInput {
        PrFetchInput {
            repository: crate::git::RepositoryIdentity::Missing,
            origin_repository: None,
            local: crate::git::PrLocalState {
                head_oid: Some(head.to_string()),
                base_oid: Some("base".to_string()),
                names: names.iter().map(|n| (*n).to_string()).collect(),
                detached: false,
            },
        }
    }

    fn assoc(number: u64, head_oid: &str, head_ref: &str) -> AssocPr {
        AssocPr {
            number,
            head_oid: head_oid.to_string(),
            head_ref: head_ref.to_string(),
            created_at: String::new(),
            closed_at: String::new(),
            raw: None,
        }
    }

    #[test]
    fn fetch_gates_resolve_without_touching_the_forge() {
        // Each early gate returns before any `gh` spawn: identity failures and a
        // detached HEAD (`specs/forge-host.md`).
        let gated = |input: &PrFetchInput| fetch(Path::new("."), input);
        let mut missing = input("head", &["feat"]);
        missing.repository = crate::git::RepositoryIdentity::Missing;
        assert_eq!(gated(&missing), PrView::NeedsForgeRemote);

        let mut unsupported = input("head", &["feat"]);
        unsupported.repository =
            crate::git::RepositoryIdentity::Unsupported("bitbucket.org".into());
        assert_eq!(gated(&unsupported), PrView::UnsupportedHost("bitbucket.org".into()));

        let repo = crate::git::RepositoryIdentity::Repository(
            crate::git::RepoTarget::new("github.com", "owner", "repo").unwrap(),
        );
        let mut detached = input("head", &["feat"]);
        detached.repository = repo;
        detached.local.detached = true;
        assert_eq!(gated(&detached), PrView::Detached);
    }

    #[test]
    fn fork_repository_admits_only_a_same_host_other_repository() {
        let target = crate::git::RepoTarget::new("github.com", "acme", "widgets").unwrap();
        let fork = crate::git::RepoTarget::new("github.com", "contributor", "widgets").unwrap();
        let foreign = crate::git::RepoTarget::new("ghe.corp.test", "me", "widgets").unwrap();
        assert_eq!(fork_repository(Some(&fork), &target), Some(&fork));
        assert_eq!(fork_repository(Some(&target.clone()), &target), None, "same repo, no fork");
        assert_eq!(fork_repository(Some(&foreign), &target), None, "another host proves nothing");
        assert_eq!(fork_repository(None, &target), None);
    }

    #[test]
    fn parse_branch_lookup_splits_lifecycles_and_filters_stranger_forks() {
        let node = |number: u64, state: &str, cross: bool, owner: &str| {
            serde_json::json!({"number": number, "state": state, "headRefOid": "abc",
                "headRefName": "feat", "createdAt": "2026-07-01T00:00:00Z",
                "closedAt": null, "isCrossRepository": cross,
                "headRepositoryOwner": {"login": owner}})
        };
        let v = serde_json::json!({"data": {"repository": {
            // The open PR arrives only through its own state-filtered block — on a
            // churny branch name the finished page's cap must never hide it.
            "o0": {"nodes": [
                node(7, "OPEN", false, "acme"),
                // A stranger's same-named fork branch never attaches.
                node(10, "OPEN", true, "stranger")
            ]},
            "h0": {"nodes": [
                node(8, "MERGED", false, "acme"),
                node(9, "CLOSED", false, "acme"),
                // A merged fork PR whose fork was deleted: GitHub nulls the head owner.
                node(11, "MERGED", true, "")
            ]},
            // A duplicate across name aliases lands once.
            "o1": {"nodes": [node(7, "OPEN", false, "acme")]},
            "h1": {"nodes": []}
        }}});
        let a = parse_branch_lookup(&v, 2, None);
        assert_eq!(a.open.iter().map(|p| p.number).collect::<Vec<_>>(), [7]);
        assert_eq!(a.history.iter().map(|p| p.number).collect::<Vec<_>>(), [8, 9]);
        // Under a fork filter, only the named fork's heads count: same-repository heads
        // are upstream's own branches, not this clone's. The deleted-fork merged PR
        // still admits — the history ancestry guard keeps strangers out downstream.
        let a = parse_branch_lookup(&v, 2, Some("stranger"));
        assert_eq!(a.open.iter().map(|p| p.number).collect::<Vec<_>>(), [10]);
        assert_eq!(a.history.iter().map(|p| p.number).collect::<Vec<_>>(), [11]);
    }

    #[test]
    fn the_branch_query_lists_open_prs_apart_from_the_capped_finished_page() {
        // One open block and one finished block per name: `resolve_pick` promises any
        // open PR outranks history, and it can only honor that for rows it receives —
        // a mixed-state page 20 deep could bury an older still-open PR.
        let q = build_branch_query(2);
        for i in 0..2 {
            assert!(q.contains(&format!("o{i}:pullRequests(headRefName:$b{i}, states:[OPEN]")));
            assert!(
                q.contains(&format!("h{i}:pullRequests(headRefName:$b{i}, states:[MERGED,CLOSED]"))
            );
        }
    }

    #[test]
    fn resolve_pick_takes_the_newest_open_before_any_history() {
        let open = |n: u64, created: &str| AssocPr {
            created_at: created.to_string(),
            ..assoc(n, "h", "b")
        };
        let hist =
            |n: u64, closed: &str| AssocPr { closed_at: closed.to_string(), ..assoc(n, "h", "b") };
        // The open path never touches git, so a dummy repo path is safe here; the
        // history path's ancestry guard is exercised in `tests/pr_candidates.rs`.
        let all = Association {
            open: vec![open(1, "2026-06-01T00:00:00Z"), open(2, "2026-06-03T00:00:00Z")],
            history: vec![hist(9, "2026-07-01T00:00:00Z")],
        };
        let pick = resolve_pick(Path::new("."), &all, Some("head")).unwrap();
        assert_eq!(pick, Some(2), "the newest open wins over any history");
        // A creation-time tie keeps the earlier entry, so the pick is deterministic.
        let tie = Association {
            open: vec![open(3, "2026-06-03T00:00:00Z"), open(4, "2026-06-03T00:00:00Z")],
            history: Vec::new(),
        };
        assert_eq!(resolve_pick(Path::new("."), &tie, None).unwrap(), Some(3));
        // With no open PR and no pinned HEAD, history proves nothing.
        let history_only =
            Association { open: Vec::new(), history: vec![hist(9, "2026-07-01T00:00:00Z")] };
        assert_eq!(resolve_pick(Path::new("."), &history_only, None).unwrap(), None);
    }

    #[test]
    fn snapshot_carries_the_head_ref_and_fork_marker() {
        let node = serde_json::json!({
            "number": 5, "title": "t", "url": "u", "state": "OPEN", "isDraft": false,
            "headRefName": "persiyanov/feature", "isCrossRepository": true, "baseRefName": "main",
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "commits": {"nodes": []}, "reviews": {"nodes": []},
            "comments": {"nodes": []}, "reviewThreads": {"nodes": []}
        });
        let s = build_snapshot(&node, Sync::InSync);
        assert_eq!(s.head_ref, "persiyanov/feature");
        assert!(s.head_is_fork);
        // Absent fields default rather than fail — a mid-rollout API response degrades soft.
        let bare = serde_json::json!({"number": 5});
        let s = build_snapshot(&bare, Sync::InSync);
        assert_eq!(s.head_ref, "");
        assert!(!s.head_is_fork);
    }

    #[test]
    fn comments_merge_three_surfaces_newest_first() {
        let reviews = serde_json::json!([
            {"author": {"login": "codex[bot]"}, "state": "COMMENTED", "body": "Codex review.", "submittedAt": "2026-06-27T10:00:00Z"}
        ]);
        let issues = serde_json::json!([
            {"author": {"login": "persijano"}, "body": "watch the 404s", "createdAt": "2026-06-27T12:00:00Z"}
        ]);
        let threads = serde_json::json!([
            {"isResolved": false, "isOutdated": true, "path": "a.py", "line": null,
             "comments": {"totalCount": 2, "nodes": [{"author": {"login": "claude[bot]"}, "body": "SSRF", "createdAt": "2026-06-27T11:00:00Z"}]}}
        ]);
        let cs = merge_comments(&reviews, &issues, &threads);
        assert_eq!(cs.len(), 3);
        // Newest first across all three surfaces — pin the full order so a reversed or
        // unstable comparator fails rather than passing on the endpoints alone.
        assert_eq!(
            cs.iter().map(|c| c.created_at.as_str()).collect::<Vec<_>>(),
            ["2026-06-27T12:00:00Z", "2026-06-27T11:00:00Z", "2026-06-27T10:00:00Z"]
        );
        assert_eq!(cs[0].author, "persijano");
        assert_eq!(cs[0].kind, CommentKind::Comment);
        assert!(!cs[0].author_is_bot);
        assert_eq!(cs[1].kind, CommentKind::Finding);
        assert_eq!(cs[2].kind, CommentKind::Review);
        // The finding carries its thread state, an unanchored line, and one reply.
        let f = cs.iter().find(|c| c.kind == CommentKind::Finding).unwrap();
        assert_eq!(f.anchor, "a.py");
        assert!(f.is_outdated);
        assert_eq!(f.reply_count, 1);
    }

    #[test]
    fn a_bots_prose_collapses_to_its_latest_a_humans_is_kept() {
        let reviews = serde_json::json!([
            {"author": {"login": "claude[bot]"}, "body": "old review", "submittedAt": "2026-06-27T09:00:00Z"},
            {"author": {"login": "claude[bot]"}, "body": "new review", "submittedAt": "2026-06-27T10:00:00Z"},
            {"author": {"login": "persijano"}, "body": "note one", "submittedAt": "2026-06-27T09:30:00Z"},
            {"author": {"login": "persijano"}, "body": "note two", "submittedAt": "2026-06-27T09:45:00Z"}
        ]);
        let cs = merge_comments(&reviews, &serde_json::json!([]), &serde_json::json!([]));
        let claude: Vec<_> = cs.iter().filter(|c| c.author == "claude[bot]").collect();
        assert_eq!(claude.len(), 1); // only the latest bot review
        assert_eq!(claude[0].body, "new review");
        assert_eq!(cs.iter().filter(|c| c.author == "persijano").count(), 2); // both human notes
    }

    #[test]
    fn a_bots_findings_are_each_kept_even_as_its_prose_collapses() {
        // Inline findings anchor to distinct lines, so — unlike a bot's PR-level prose — they
        // are never collapsed: two findings from the same bot both survive, the prose folds to one.
        let reviews = serde_json::json!([
            {"author": {"login": "claude[bot]"}, "body": "old prose", "submittedAt": "2026-06-27T09:00:00Z"},
            {"author": {"login": "claude[bot]"}, "body": "new prose", "submittedAt": "2026-06-27T09:30:00Z"}
        ]);
        let threads = serde_json::json!([
            {"isResolved": false, "isOutdated": false, "path": "a.py", "line": 10,
             "comments": {"totalCount": 1, "nodes": [{"author": {"login": "claude[bot]"}, "body": "finding one", "createdAt": "2026-06-27T10:00:00Z"}]}},
            {"isResolved": false, "isOutdated": false, "path": "b.py", "line": 20,
             "comments": {"totalCount": 1, "nodes": [{"author": {"login": "claude[bot]"}, "body": "finding two", "createdAt": "2026-06-27T11:00:00Z"}]}}
        ]);
        let cs = merge_comments(&reviews, &serde_json::json!([]), &threads);
        assert_eq!(cs.iter().filter(|c| c.kind == CommentKind::Finding).count(), 2);
        assert_eq!(cs.iter().filter(|c| c.kind == CommentKind::Review).count(), 1); // prose collapsed
    }

    #[test]
    fn an_undated_bot_review_survives_beside_the_bots_dated_prose() {
        // GitLab approvals and Azure DevOps votes arrive as reviews with no timestamp: a
        // standing verdict, not repeated prose, so newest-wins dedup never drops one.
        let row = |kind, anchor: &str, body: &str, created_at: &str| Comment {
            kind,
            author: "claude[bot]".to_string(),
            author_is_bot: true,
            anchor: anchor.to_string(),
            body: body.to_string(),
            snippet: None,
            created_at: created_at.to_string(),
            is_resolved: false,
            is_outdated: false,
            reply_count: 0,
        };
        let mut out = vec![
            row(CommentKind::Review, "review", "approved", ""),
            row(CommentKind::Comment, "comment", "old prose", "2026-06-27T09:00:00Z"),
            row(CommentKind::Comment, "comment", "new prose", "2026-06-27T10:00:00Z"),
        ];
        dedup_bot_prose(&mut out);
        let bodies: Vec<_> = out.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, ["approved", "new prose"]);
    }

    #[test]
    fn relative_age_buckets_by_magnitude() {
        // now = 2026-06-27T12:00:00Z
        let now = UNIX_EPOCH
            + std::time::Duration::from_secs(parse_iso("2026-06-27T12:00:00Z").unwrap() as u64);
        assert_eq!(relative_age("2026-06-27T11:55:00Z", now), "5m");
        assert_eq!(relative_age("2026-06-27T10:00:00Z", now), "2h");
        assert_eq!(relative_age("2026-06-24T12:00:00Z", now), "3d");
        assert_eq!(relative_age("2026-06-13T12:00:00Z", now), "2w");
        assert_eq!(relative_age("garbage", now), "");
    }

    #[test]
    fn parse_iso_anchors_the_epoch_and_the_feb_year_branch() {
        // The epoch anchors the civil-from-days math; a Jan/Feb date exercises the `mo <= 2`
        // year-adjust branch that the June fixtures above never hit.
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2000-02-29T00:00:00Z"), Some(951_782_400)); // a leap-day boundary
        assert_eq!(parse_iso("not-a-date"), None);
    }

    #[test]
    fn sync_leads_with_unpushed_and_tolerates_a_missing_head() {
        assert_eq!(derive_sync(None), Sync::Unknown);
        assert_eq!(derive_sync(Some((0, 0))), Sync::InSync);
        assert_eq!(derive_sync(Some((2, 0))), Sync::Unpushed(2));
        assert_eq!(derive_sync(Some((0, 3))), Sync::Behind(3));
        assert_eq!(derive_sync(Some((2, 3))), Sync::Unpushed(2)); // diverged → unpushed leads
    }

    #[test]
    fn gh_failure_classifies_by_stderr_wording() {
        assert_eq!(
            classify_failure("gh auth login required", "github.example.com"),
            GhError::NotAuthed("github.example.com".to_string())
        );
        assert_eq!(
            classify_failure("You are not logged into any GitHub hosts", "github.com"),
            GhError::NotAuthed("github.com".to_string())
        );
        assert_eq!(
            classify_failure("HTTP 500 something", "github.com"),
            GhError::Other("HTTP 500 something".into())
        );
        assert_eq!(
            PrView::from(GhError::LocalGit("rev-list failed".into())),
            PrView::GitError("rev-list failed".into())
        );
    }

    #[test]
    fn graphql_arguments_always_pin_the_canonical_host() {
        let args = graphql_args(
            "github.example.com",
            "query($o:String!){viewer{login}}",
            &[("o".to_string(), "owner".to_string())],
        );
        assert_eq!(&args[..4], ["api", "graphql", "--hostname", "github.example.com"]);
        assert!(args.windows(2).any(|pair| pair == ["-f", "o=owner"]));
    }
}
