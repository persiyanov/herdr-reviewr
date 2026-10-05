//! The read-only forge kernel: fetch input, per-forge dispatch, the shared snapshot, and GitHub.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::proc::RunError;

/// What the `PR` tab shows: the resolved snapshot, or a degraded state with its own remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrView {
    /// Work is pending but has not crossed the loading-indicator delay.
    Pending,
    /// Work crossed the loading-indicator delay without producing a snapshot.
    Loading,
    /// The PR resolved from the branch's published heads or its pin.
    Pr(Box<PrSnapshot>),
    /// No PR resolves from the current branch's heads.
    NoPr,
    /// `HEAD` is detached, so there is no branch identity to query.
    Detached,
    /// No PR resolved, but `HEAD` still contains the painted one's head, so it stays. Never stored.
    Held,
    /// The resolved forge's CLI is not on `PATH`.
    NoCli(crate::git::Forge),
    /// The forge CLI is installed but misses the extension its reads require
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
    /// The remedy for a retryable failure, which keeps the visible snapshot; `None` otherwise.
    pub fn retry_remedy(&self, refresh: crate::keymap::Key) -> Option<String> {
        let refresh = refresh.label();
        match self {
            Self::NoCli(forge) => Some(format!(
                "{} CLI not found. Install `{}`, then press {refresh}.",
                forge.display_name(),
                forge.cli()
            )),
            // A forge with no extension still gets a remedy.
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

/// The login command the unauthenticated remedy advertises.
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

/// The extension-install command the missing-extension remedy advertises.
fn extension_hint(forge: crate::git::Forge) -> Option<&'static str> {
    match forge {
        crate::git::Forge::AzureDevOps => Some("`az extension add --name azure-devops`"),
        crate::git::Forge::GitHub | crate::git::Forge::GitLab => None,
    }
}

/// One pull request's state, read fresh from the forge each poll.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PrSnapshot {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The PR description as the forge returns it, empty when none.
    pub body: String,
    pub state: PrState,
    pub is_draft: bool,
    /// The PR's head branch name, which may differ from the local one.
    pub head_ref: String,
    /// Whether the head lives in a fork, marked so a same-named fork PR shows.
    pub head_is_fork: bool,
    /// The PR's head commit — the hold gate's anchor, never rendered
    pub head_oid: String,
    pub base_ref: String,
    pub merge: Merge,
    pub sync: Sync,
    pub checks: Vec<Check>,
    pub comments: Vec<Comment>,
    /// Reviews, conversation comments, or threads had more rows than the 100-row fetch.
    pub comments_truncated: bool,
    /// Checks had more rows than the 100-row fetch.
    pub checks_truncated: bool,
}

/// The PR lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// The PR's actionable merge blocker; anything a reviewer can't act on folds into `Clean`.
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
    /// `path`, `path:line`, or `path:start-end` for a finding, the kind word otherwise.
    pub anchor: String,
    /// Path, line range, and side for a `finding`. None for a review or comment.
    pub place: Option<FindingPlace>,
    pub body: String,
    /// The finding's diff hunk as GitHub returns it; `None` for a review or comment.
    pub snippet: Option<String>,
    /// The post time as GitHub's ISO-8601 string (`…Z`), the newest-first sort key.
    pub created_at: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    /// Replies after the root, oldest first. Empty for a single card.
    pub replies: Vec<Reply>,
    /// The caller's unpublished GitLab draft note id. `None` when published.
    pub draft_id: Option<u64>,
}

/// One reply on a thread. The root lives on [`Comment`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub author: String,
    pub author_is_bot: bool,
    pub body: String,
    pub created_at: String,
    /// The caller's unpublished GitLab draft note id. `None` when published.
    pub draft_id: Option<u64>,
}

/// What a comment is anchored to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    Review,
    Comment,
    Finding,
}

/// Where a finding sits: path, inclusive line range, and which file side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindingPlace {
    pub path: String,
    pub range: Option<(u32, u32)>,
    pub side: Option<crate::model::Side>,
}

impl FindingPlace {
    pub fn from_lines(
        path: &str,
        start: Option<u64>,
        end: Option<u64>,
        side: Option<crate::model::Side>,
    ) -> Self {
        let range = match (start, end) {
            (Some(a), Some(b)) => {
                let (a, b) = (a as u32, b as u32);
                Some(if a <= b { (a, b) } else { (b, a) })
            }
            (Some(n), None) | (None, Some(n)) => Some((n as u32, n as u32)),
            (None, None) => None,
        };
        Self { path: path.to_string(), range, side }
    }

    /// Test helper: `path`, `path:line`, or `path:start-end`.
    pub fn from_anchor(anchor: &str, side: Option<crate::model::Side>) -> Self {
        let Some((path, rest)) = anchor.rsplit_once(':') else {
            return Self { path: anchor.to_string(), range: None, side };
        };
        if let Some((a, b)) = rest.split_once('-')
            && let (Ok(s), Ok(e)) = (a.parse::<u32>(), b.parse::<u32>())
        {
            let (lo, hi) = if s <= e { (s, e) } else { (e, s) };
            return Self { path: path.to_string(), range: Some((lo, hi)), side };
        }
        if let Ok(n) = rest.parse::<u32>() {
            return Self { path: path.to_string(), range: Some((n, n)), side };
        }
        Self { path: anchor.to_string(), range: None, side }
    }

    pub fn anchor(&self) -> String {
        finding_anchor(
            &self.path,
            self.range.map(|(s, _)| u64::from(s)),
            self.range.map(|(_, e)| u64::from(e)),
        )
    }
}

/// New/right wins; old/left only when there is no new-side signal.
pub(crate) fn finding_side(on_new: bool, on_old: bool) -> Option<crate::model::Side> {
    if on_new {
        Some(crate::model::Side::New)
    } else if on_old {
        Some(crate::model::Side::Old)
    } else {
        None
    }
}

impl PrSnapshot {
    /// Any failure fails, else any running runs, else success; `None` without checks.
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

    /// Checks that ran and passed. A skipped check counts toward neither side.
    pub fn passed_checks(&self) -> usize {
        self.checks.iter().filter(|c| c.status == CheckStatus::Success).count()
    }
}

/// Run explicitly targeted `gh` arguments in `repo` and return stdout or a classified failure.
fn gh(repo: &Path, host: &str, args: &[&str], cancelled: &AtomicBool) -> Result<String, GhError> {
    let mut cmd = crate::proc::command("gh");
    cmd.current_dir(repo).args(args);
    run_provider(
        cmd,
        cancelled,
        GhError::NoGh,
        |stderr| classify_failure(stderr, host),
        GhError::Other,
    )
}

/// A failed `gh`'s state by stderr wording; it has no stable exit codes.
fn classify_failure(stderr: &str, host: &str) -> GhError {
    let s = stderr.to_lowercase();
    if s.contains("not logged")
        || s.contains("authentication")
        || s.contains("gh auth login")
        // Never a bare `401`, which an OID or path can contain.
        || reports_status(&s, 401)
        || s.contains("bad credentials")
    {
        GhError::NotAuthed(host.to_owned())
    } else if s.contains("could not resolve to a ") {
        GhError::NotFound(stderr.trim().to_string())
    } else {
        GhError::Other(stderr.trim().to_string())
    }
}

/// Run one provider CLI read, mapping each failure into the provider's error type.
pub(crate) fn run_provider<E>(
    cmd: Command,
    cancelled: &AtomicBool,
    not_found: E,
    classify: impl FnOnce(&str) -> E,
    other: impl Fn(String) -> E,
) -> Result<String, E> {
    // A fetch has no deadline of its own: the coordinator cancels one it superseded.
    match crate::proc::run_tree(cmd, || cancelled.load(std::sync::atomic::Ordering::Acquire)) {
        Ok(stdout) => Ok(stdout),
        Err(RunError::NotFound) => Err(not_found),
        Err(RunError::Failed { stderr }) => Err(classify(&stderr)),
        Err(RunError::Io(error)) => Err(other(error)),
        Err(RunError::Stopped) => Err(other("request cancelled".into())),
    }
}

/// Join a reader thread; a panic becomes a retryable error instead of killing the worker.
pub(crate) fn join_read<T, E>(
    handle: std::thread::ScopedJoinHandle<'_, Result<T, E>>,
    on_panic: impl FnOnce() -> E,
) -> Result<T, E> {
    handle.join().unwrap_or_else(|_| Err(on_panic()))
}

/// The newest `SURFACE_CAP` of oldest-first `rows`.
pub(crate) fn newest_capped<T>(mut rows: Vec<T>) -> Vec<T> {
    let keep = rows.len().min(SURFACE_CAP);
    rows.split_off(rows.len() - keep)
}

/// Each surface reads at most this many rows, never paged to exhaustion
pub(crate) const SURFACE_CAP: usize = 100;

/// Whether stderr reports HTTP `code` as a status, never as digits inside an OID or path.
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
        // `404 not found` and `http 404` lead a line; a URL or an OID never does.
        let line = line.strip_prefix("http ").unwrap_or(line);
        line.strip_prefix(&code).is_some_and(|rest| rest.starts_with(' ') || rest.is_empty())
    })
}

/// A classified `gh` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug, PartialEq, Eq)]
enum GhError {
    NoGh,
    NotAuthed(String),
    /// GraphQL could not resolve the addressed pull request or repository.
    NotFound(String),
    LocalGit(String),
    Other(String),
}

impl From<GhError> for PrView {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NoGh => PrView::NoCli(crate::git::Forge::GitHub),
            GhError::NotAuthed(host) => PrView::NotAuthed(crate::git::Forge::GitHub, host),
            GhError::LocalGit(message) => PrView::GitError(message),
            GhError::NotFound(m) | GhError::Other(m) => PrView::Error(crate::git::Forge::GitHub, m),
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
    let local = match crate::git::pr_local(repo, base, &config.forge_hosts()) {
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
    if input.local.branch.is_none() {
        // A detached HEAD (e.g. after `gh pr merge --delete-branch`) has no branch story.
        return Ok(PrView::Detached);
    }
    // Exhaustive, so a new forge must be routed here to build.
    match repository.forge() {
        crate::git::Forge::GitLab => {
            return Ok(crate::gitlab::fetch(repo, input, repository, cancelled));
        }
        crate::git::Forge::AzureDevOps => {
            return Ok(crate::azure_devops::fetch(repo, input, repository, cancelled));
        }
        crate::git::Forge::GitHub => {}
    }
    // A `gh pr checkout` pin outranks the lookup, unless the forge no longer resolves it.
    if let Some(pin) = input.local.pin_on(crate::git::Forge::GitHub)
        && let Some(view) = pin_outcome(read_pr(repo, input, &pin.repo, pin.number, cancelled))?
    {
        return Ok(view);
    }
    let Some((number, detail_repo)) = lookup_pick(repo, input, repository, cancelled)? else {
        return Ok(PrView::NoPr);
    };
    Ok(read_pr(repo, input, detail_repo, number, cancelled)?.unwrap_or(PrView::NoPr))
}

/// A pinned read's view, or `None` to fall back to the lookup when the pin is stale.
fn pin_outcome(read: Result<Option<PrView>, GhError>) -> Result<Option<PrView>, GhError> {
    match read {
        Err(GhError::NotFound(_)) => Ok(None),
        read => read,
    }
}

/// Read one pull request's full snapshot. `None` when the forge reports no such PR.
fn read_pr(
    repo: &Path,
    input: &PrFetchInput,
    detail_repo: &crate::git::RepoTarget,
    number: u64,
    cancelled: &AtomicBool,
) -> Result<Option<PrView>, GhError> {
    let target = FetchTarget {
        repo,
        host: detail_repo.host(),
        owner: detail_repo.owner(),
        name: detail_repo.name(),
        cancelled,
    };
    let mut detail = pr_detail(&target, number)?;
    complete_review_thread_comments(&target, &mut detail)?;
    let node = &detail["data"]["repository"]["pullRequest"];
    if node.is_null() {
        return Ok(None);
    }
    // Against the pinned HEAD, so a mid-fetch checkout never mixes two branches.
    let pr_head = node["headRefOid"].as_str().unwrap_or_default();
    let sync = local_sync(repo, input.local.head_oid.as_deref(), pr_head)
        .map_err(|error| GhError::LocalGit(error.0))?;
    Ok(Some(PrView::Pr(Box::new(build_snapshot(node, sync)))))
}

/// The branch's PR by head lookup; a fork clone asks both, and upstream's pick wins.
fn lookup_pick<'a>(
    repo: &Path,
    input: &'a PrFetchInput,
    repository: &'a crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> Result<Option<(u64, &'a crate::git::RepoTarget)>, GhError> {
    let names = input.local.head_names();
    if names.is_empty() {
        return Ok(None);
    }
    let head = input.local.head_oid.as_deref();
    let heads = &input.local.heads;
    let pick_in = |queried: &'a crate::git::RepoTarget| -> Result<Option<(u64, _)>, GhError> {
        let target = FetchTarget {
            repo,
            host: queried.host(),
            owner: queried.owner(),
            name: queried.name(),
            cancelled,
        };
        let assoc = branch_lookup(&target, queried, heads, &names)?;
        Ok(resolve_pick(repo, &assoc, head)
            .map_err(|error| GhError::LocalGit(error.0))?
            .map(|number| (number, queried)))
    };
    if let Some(pick) = pick_in(repository)? {
        return Ok(Some(pick));
    }
    match fork_repository(input.origin_repository.as_ref(), repository) {
        Some(fork) => pick_in(fork),
        None => Ok(None),
    }
}

/// Where a pull request's head lives, as the forge reports it.
pub(crate) enum HeadRepo<'a> {
    /// In the queried repository, by the forge's same-repo flag, which survives renames.
    Queried,
    /// In another repository, by every local spelling of it; none for a deleted fork.
    Other(Vec<&'a crate::git::RepoTarget>),
}

/// Whether a PR's head repository and name are one of the branch's published heads.
pub(crate) fn admits(
    heads: &[crate::git::Head],
    queried: &crate::git::RepoTarget,
    head_ref: &str,
    head_repo: &HeadRepo<'_>,
) -> bool {
    heads.iter().any(|head| {
        head.name == head_ref
            && match (head.repo.is(queried), head_repo) {
                (true, HeadRepo::Queried) => true,
                (false, HeadRepo::Other(repos)) => repos.iter().any(|repo| repo.is(&head.repo)),
                _ => false,
            }
    })
}

/// The local sync against the PR's head; `Unknown` when either side is unpinned.
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

/// The branch's position against the PR head; a diverged one leads with its unpushed count.
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
    /// Read only by Azure DevOps, whose lookup does not filter by branch.
    pub(crate) head_ref: String,
    pub(crate) created_at: String,
    /// The history sort key: the merge or close time. Empty for an open PR.
    pub(crate) closed_at: String,
    /// The full node when the lookup returned the whole pull request, as Azure DevOps does.
    pub(crate) raw: Option<Value>,
}

/// The branch's pull requests: open, and finished ones behind the ancestry guard.
#[derive(Debug, Default)]
pub struct Association {
    pub open: Vec<AssocPr>,
    pub history: Vec<AssocPr>,
}

/// The GitHub lookup: aliased `pullRequests(headRefName:)` blocks, values as variables.
fn branch_lookup(
    target: &FetchTarget<'_>,
    queried: &crate::git::RepoTarget,
    heads: &[crate::git::Head],
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
    Ok(parse_branch_lookup(&v, names.len(), queried, heads))
}

/// Per name, an open block `o{i}` and a finished block `h{i}`, so history never buries an open PR.
fn build_branch_query(names: usize) -> String {
    use std::fmt::Write;
    let mut q = String::from("query($o:String!,$n:String!");
    for i in 0..names {
        let _ = write!(q, ",$b{i}:String!");
    }
    q.push_str("){repository(owner:$o,name:$n){");
    let fields = "first:20, orderBy:{field:CREATED_AT, direction:DESC}){nodes{\
                  number state headRefOid headRefName createdAt closedAt \
                  isCrossRepository headRepository{nameWithOwner}}} ";
    for i in 0..names {
        let _ = write!(q, "o{i}:pullRequests(headRefName:$b{i}, states:[OPEN], {fields}");
        let _ = write!(q, "h{i}:pullRequests(headRefName:$b{i}, states:[MERGED,CLOSED], {fields}");
    }
    q.push_str("}}");
    q
}

/// The lookup's admitted nodes by lifecycle, deduplicated across aliases.
fn parse_branch_lookup(
    v: &Value,
    aliases: usize,
    queried: &crate::git::RepoTarget,
    heads: &[crate::git::Head],
) -> Association {
    let mut assoc = Association::default();
    let keys = (0..aliases).flat_map(|i| [format!("o{i}"), format!("h{i}")]);
    for key in keys {
        let nodes = &v["data"]["repository"][key.as_str()]["nodes"];
        for node in nodes.as_array().into_iter().flatten() {
            let head_ref = node["headRefName"].as_str().unwrap_or_default();
            let cross = node["isCrossRepository"].as_bool() == Some(true);
            // A deleted fork nulls `headRepository`, so its head names no repository.
            let reported = node["headRepository"]["nameWithOwner"]
                .as_str()
                .and_then(|full| full.split_once('/'))
                .and_then(|(owner, name)| {
                    crate::git::RepoTarget::with_path(
                        crate::git::Forge::GitHub,
                        queried.host(),
                        &[owner, name],
                    )
                });
            let head_repo =
                if cross { HeadRepo::Other(reported.iter().collect()) } else { HeadRepo::Queried };
            if !admits(heads, queried, head_ref, &head_repo) {
                continue;
            }
            let state = node["state"].as_str().unwrap_or_default();
            let Some(number) = node["number"].as_u64() else { continue };
            let pr = AssocPr {
                number,
                head_oid: node["headRefOid"].as_str().unwrap_or_default().to_string(),
                head_ref: head_ref.to_string(),
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

/// The fork this clone works from: `origin`, when it is another repository on the target's host.
pub(crate) fn fork_repository<'a>(
    origin: Option<&'a crate::git::RepoTarget>,
    target: &crate::git::RepoTarget,
) -> Option<&'a crate::git::RepoTarget> {
    origin.filter(|origin| origin.host() == target.host() && !origin.is(target))
}

/// Push `pr` unless its number is already in `bucket` — a PR's identity is its number.
pub(crate) fn push_unique(bucket: &mut Vec<AssocPr>, pr: AssocPr) {
    if !bucket.iter().any(|have| have.number == pr.number) {
        bucket.push(pr);
    }
}

/// The newest open PR, else the newest finished one whose head `HEAD` contains.
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
    // Each candidate costs git spawns; ten is ample for a real branch.
    history.truncate(10);
    for pr in history {
        if !pr.head_oid.is_empty() && crate::git::contains_commit(repo, head, &pr.head_oid)? {
            return Ok(Some(pr.number));
        }
    }
    Ok(None)
}

/// The PR with the newest `key` timestamp; a tie keeps the earlier entry.
fn newest_by(prs: &[AssocPr], key: impl Fn(&AssocPr) -> &str) -> Option<u64> {
    let mut best: Option<&AssocPr> = None;
    for pr in prs {
        if best.is_none_or(|b| key(pr) > key(b)) {
            best = Some(pr);
        }
    }
    best.map(|pr| pr.number)
}

/// One PR's whole state in one GraphQL call, each list its newest 100 rows.
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
         reviews(last:100){{pageInfo{{hasPreviousPage}} nodes{{author{{login}} body submittedAt state databaseId}}}} \
         comments(last:100){{pageInfo{{hasPreviousPage}} nodes{{author{{login}} body createdAt}}}} \
         reviewThreads(last:100){{pageInfo{{hasPreviousPage}} nodes{{id isResolved isOutdated path \
         startLine line originalStartLine originalLine diffSide \
         comments(first:100){{pageInfo{{hasNextPage endCursor}} nodes{{author{{login}} body createdAt diffHunk state databaseId}}}}}}}}}}}}}}"
    )
}

/// Run a GraphQL `query`, every variable a raw `-f` string: `-F` would make branch `123` an Int.
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

/// Page out every shown thread's comments, or fail so the last good view stays.
fn complete_review_thread_comments(
    target: &FetchTarget<'_>,
    detail: &mut Value,
) -> Result<(), GhError> {
    let Some(threads) =
        detail["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"].as_array_mut()
    else {
        return Ok(());
    };
    for thread in threads {
        while let Some((id, after)) = next_thread_page(thread)? {
            let page = thread_comments_page(target, &id, &after)?;
            append_thread_comment_page(thread, &page)?;
        }
    }
    Ok(())
}

/// The thread's next comments page; `Err` when GitHub claims one without a cursor.
fn next_thread_page(thread: &Value) -> Result<Option<(String, String)>, GhError> {
    if thread["comments"]["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
        return Ok(None);
    }
    let id = thread["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GhError::Other("review thread missing id".into()))?;
    let after = thread["comments"]["pageInfo"]["endCursor"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GhError::Other("incomplete thread comments page".into()))?;
    Ok(Some((id.to_string(), after.to_string())))
}

fn append_thread_comment_page(thread: &mut Value, page: &Value) -> Result<(), GhError> {
    let comments = &page["data"]["node"]["comments"];
    if comments["pageInfo"].is_null() {
        return Err(GhError::Other("thread comments page missing".into()));
    }
    let more = comments["nodes"].as_array().cloned().unwrap_or_default();
    thread["comments"]["nodes"]
        .as_array_mut()
        .ok_or_else(|| GhError::Other("thread comments missing".into()))?
        .extend(more);
    thread["comments"]["pageInfo"] = comments["pageInfo"].clone();
    Ok(())
}

fn thread_comments_page(target: &FetchTarget<'_>, id: &str, after: &str) -> Result<Value, GhError> {
    let q = "query($id:ID!,$after:String!){node(id:$id){... on PullRequestReviewThread{\
             comments(first:100, after:$after){pageInfo{hasNextPage endCursor} \
             nodes{author{login} body createdAt diffHunk state databaseId}}}}}";
    graphql(
        target.repo,
        target.host,
        q,
        &[("id".into(), id.to_string()), ("after".into(), after.to_string())],
        target.cancelled,
    )
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
    // Each surface asks for only its own paging flag, so OR-ing both reads the right one.
    let more = |conn: &Value| {
        conn["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false)
            || conn["pageInfo"]["hasPreviousPage"].as_bool().unwrap_or(false)
    };
    let comments_truncated =
        more(&node["reviews"]) || more(&node["comments"]) || more(&node["reviewThreads"]);
    let checks_truncated = more(contexts);
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
        comments_truncated,
        checks_truncated,
    }
}

fn parse_state(s: &str) -> PrState {
    match s {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    }
}

/// GitHub's merge blocker: a conflict or a `blocked` gate, else clean.
fn derive_merge(mergeable: Option<&str>, state: Option<&str>) -> Merge {
    match (mergeable, state) {
        (Some("CONFLICTING"), _) | (_, Some("DIRTY")) => Merge::Conflicting,
        (_, Some("BLOCKED")) => Merge::Blocked,
        _ => Merge::Clean,
    }
}

/// Insert or replace a check by name, so a re-run replaces its earlier entry.
pub(crate) fn upsert_latest(checks: &mut Vec<Check>, check: Check) {
    if let Some(slot) = checks.iter_mut().find(|c| c.name == check.name) {
        *slot = check;
    } else {
        checks.push(check);
    }
}

/// Keep each bot's latest PR-level post, then order newest first, drafts leading.
pub(crate) fn finish_comments(out: &mut Vec<Comment>) {
    dedup_bot_prose(out);
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out.sort_by_key(|c| !carries_draft(c));
}

fn carries_draft(c: &Comment) -> bool {
    c.draft_id.is_some() || c.replies.iter().any(|r| r.draft_id.is_some())
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

/// A check run's or a commit status's [`CheckStatus`].
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
            // Timed out, cancelled, action required, or missing: something needs attention.
            _ => CheckStatus::Failure,
        },
        Some("IN_PROGRESS") => CheckStatus::Running,
        _ => CheckStatus::Pending,
    }
}

/// Reviews, comments, and review threads as one newest-first list.
fn merge_comments(reviews: &Value, issues: &Value, threads: &Value) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();

    // Submitted reviews with a non-empty body (the PR-level `review` cards).
    for r in reviews.as_array().into_iter().flatten() {
        let body = r["body"].as_str().unwrap_or("").trim().to_string();
        if body.is_empty() {
            continue;
        }
        let mut review =
            prose_comment(CommentKind::Review, &r["author"], body, r["submittedAt"].as_str());
        review.draft_id = pending_id(r);
        out.push(review);
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
        let nodes = t["comments"]["nodes"].as_array().map_or(&[][..], Vec::as_slice);
        let Some(root_i) =
            nodes.iter().position(|n| !n["body"].as_str().unwrap_or("").trim().is_empty())
        else {
            continue;
        };
        let root = &nodes[root_i];
        let login = root["author"]["login"].as_str().unwrap_or("").to_string();
        let path = t["path"].as_str().unwrap_or("");
        let diff_side = t["diffSide"].as_str();
        let (start, end) = thread_range(
            t["startLine"].as_u64(),
            t["line"].as_u64(),
            t["originalStartLine"].as_u64(),
            t["originalLine"].as_u64(),
            diff_side,
        );
        let place = FindingPlace::from_lines(
            path,
            start,
            end,
            finding_side(diff_side == Some("RIGHT"), diff_side == Some("LEFT")),
        );
        out.push(Comment {
            kind: CommentKind::Finding,
            author_is_bot: is_bot(&login),
            author: login,
            anchor: place.anchor(),
            place: Some(place),
            body: root["body"].as_str().unwrap_or("").trim().to_string(),
            snippet: root["diffHunk"].as_str().filter(|h| !h.is_empty()).map(str::to_string),
            created_at: root["createdAt"].as_str().unwrap_or("").to_string(),
            is_resolved: t["isResolved"].as_bool().unwrap_or(false),
            is_outdated: t["isOutdated"].as_bool().unwrap_or(false),
            replies: replies_from_nodes(&nodes[root_i..]),
            draft_id: pending_id(root),
        });
    }

    finish_comments(&mut out);
    out
}

/// The viewer's unsubmitted review or comment: GitHub shows a `PENDING` one only to its author.
fn pending_id(node: &Value) -> Option<u64> {
    (node["state"].as_str() == Some("PENDING")).then(|| node["databaseId"].as_u64()).flatten()
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

pub(crate) fn finding_anchor(path: &str, start: Option<u64>, end: Option<u64>) -> String {
    match (start, end) {
        (Some(a), Some(b)) if a != b => {
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            format!("{path}:{lo}-{hi}")
        }
        (Some(n), _) | (_, Some(n)) => format!("{path}:{n}"),
        (None, None) => path.to_string(),
    }
}

/// Read-pane caption for a finding range.
pub(crate) fn finding_range_caption(start: u32, end: u32, sign: Option<char>) -> String {
    let (start, end) = if start <= end { (start, end) } else { (end, start) };
    let n = |n: u32| match sign {
        Some(sign) => format!("{sign}{n}"),
        None => n.to_string(),
    };
    if start == end {
        format!("Comment on line {}", n(start))
    } else {
        format!("Comment on lines {} to {}", n(start), n(end))
    }
}

/// A thread's new-side range, else its original one; a LEFT thread always takes the original.
fn thread_range(
    start_line: Option<u64>,
    line: Option<u64>,
    original_start: Option<u64>,
    original_line: Option<u64>,
    diff_side: Option<&str>,
) -> (Option<u64>, Option<u64>) {
    if diff_side == Some("LEFT") && (original_start.is_some() || original_line.is_some()) {
        return (original_start.or(original_line), original_line.or(original_start));
    }
    if start_line.is_some() || line.is_some() {
        return (start_line.or(line), line.or(start_line));
    }
    (original_start.or(original_line), original_line.or(original_start))
}

/// One PR-level prose row, with the defaults every non-`finding` comment shares.
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
        place: None,
        body,
        snippet: None,
        created_at,
        is_resolved: false,
        is_outdated: false,
        replies: Vec::new(),
        draft_id: None,
    }
}

/// Replies are every comment node after the root, skipping empty bodies the way roots do.
fn replies_from_nodes(nodes: &[Value]) -> Vec<Reply> {
    nodes
        .iter()
        .skip(1)
        .filter_map(|n| {
            let body = n["body"].as_str().unwrap_or("").trim();
            if body.is_empty() {
                return None;
            }
            let login = n["author"]["login"].as_str().unwrap_or("").to_string();
            Some(Reply {
                author_is_bot: is_bot(&login),
                author: login,
                body: body.to_string(),
                created_at: n["createdAt"].as_str().unwrap_or("").to_string(),
                draft_id: pending_id(n),
            })
        })
        .collect()
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
            // An undated review is a standing verdict, never lost to a newer post.
            || (c.kind == CommentKind::Review && c.created_at.is_empty())
            || keep_newest.get(&c.author) == Some(&c.created_at)
    });
}

/// Percent-encode one URL path or query value, `/` included.
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

/// Whether a name looks like a bot: `[bot]` or `-bot`, so `Talbot` stays human.
pub(crate) fn is_named_bot(name: &str) -> bool {
    is_bot(name) || name.to_ascii_lowercase().ends_with("-bot")
}

/// An RFC 3339 timestamp as Unix seconds; `None` when malformed, so no age is wrong.
pub(crate) fn parse_iso(s: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(s, &Rfc3339).ok().map(time::OffsetDateTime::unix_timestamp)
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
        let s = build_snapshot(&base, Sync::InSync);
        assert!(!s.comments_truncated && !s.checks_truncated, "all pages complete");
        // The description parses when present and stays empty when GitHub returns null.
        assert_eq!(build_snapshot(&base, Sync::InSync).body, "");
        let mut with_body = base.clone();
        with_body["body"] = serde_json::json!("## Summary\nfixes things");
        assert_eq!(build_snapshot(&with_body, Sync::InSync).body, "## Summary\nfixes things");

        // Comments and threads read `last:100`, so their "more exist" flag pages backward.
        let mut comments_more = base.clone();
        comments_more["comments"]["pageInfo"]["hasPreviousPage"] = serde_json::json!(true);
        assert!(build_snapshot(&comments_more, Sync::InSync).comments_truncated);
        assert!(!build_snapshot(&comments_more, Sync::InSync).checks_truncated);

        let mut threads_more = base.clone();
        threads_more["reviewThreads"]["pageInfo"]["hasPreviousPage"] = serde_json::json!(true);
        assert!(build_snapshot(&threads_more, Sync::InSync).comments_truncated);

        let mut checks_more = base.clone();
        checks_more["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["pageInfo"]
            ["hasNextPage"] = serde_json::json!(true);
        assert!(build_snapshot(&checks_more, Sync::InSync).checks_truncated);
        assert!(!build_snapshot(&checks_more, Sync::InSync).comments_truncated);

        // `reviews` pages backward, so `hasPreviousPage` is its flag.
        let mut reviews_more = base.clone();
        reviews_more["reviews"]["pageInfo"]["hasPreviousPage"] = serde_json::json!(true);
        assert!(build_snapshot(&reviews_more, Sync::InSync).comments_truncated);
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
            comments_truncated: false,
            checks_truncated: false,
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
                branch: names.first().map(|n| (*n).to_string()),
                heads: names
                    .iter()
                    .map(|n| crate::git::Head {
                        repo: gh("acme", "widgets"),
                        name: (*n).to_string(),
                    })
                    .collect(),
                pin: None,
            },
        }
    }

    fn gh(owner: &str, name: &str) -> crate::git::RepoTarget {
        crate::git::RepoTarget::new("github.com", owner, name).unwrap()
    }

    fn head(repo: crate::git::RepoTarget, name: &str) -> crate::git::Head {
        crate::git::Head { repo, name: name.to_string() }
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
        // Identity failures and a detached HEAD return before any `gh` spawn.
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
        detached.local.branch = None;
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
    fn parse_branch_lookup_splits_lifecycles_and_collapses_aliases() {
        let node = |number: u64, state: &str| {
            serde_json::json!({"number": number, "state": state, "headRefOid": "abc",
                "headRefName": "feat", "createdAt": "2026-07-01T00:00:00Z",
                "closedAt": null, "isCrossRepository": false,
                "headRepository": {"nameWithOwner": "acme/widgets"}})
        };
        let v = serde_json::json!({"data": {"repository": {
            // The open PR arrives through its own block, out of the finished page's cap.
            "o0": {"nodes": [node(7, "OPEN")]},
            "h0": {"nodes": [node(8, "MERGED"), node(9, "CLOSED")]},
            // A duplicate across name aliases lands once.
            "o1": {"nodes": [node(7, "OPEN")]},
            "h1": {"nodes": []}
        }}});
        let heads = [head(gh("acme", "widgets"), "feat")];
        let a = parse_branch_lookup(&v, 2, &gh("acme", "widgets"), &heads);
        assert_eq!(a.open.iter().map(|p| p.number).collect::<Vec<_>>(), [7]);
        assert_eq!(a.history.iter().map(|p| p.number).collect::<Vec<_>>(), [8, 9]);
    }

    #[test]
    fn the_branch_query_lists_open_prs_apart_from_the_capped_finished_page() {
        // An open and a finished block per name, each row naming its head repository.
        let q = build_branch_query(2);
        for i in 0..2 {
            assert!(q.contains(&format!("o{i}:pullRequests(headRefName:$b{i}, states:[OPEN]")));
            assert!(
                q.contains(&format!("h{i}:pullRequests(headRefName:$b{i}, states:[MERGED,CLOSED]"))
            );
        }
        assert!(q.contains("isCrossRepository headRepository{nameWithOwner}"));
    }

    #[test]
    fn a_pin_answers_unless_the_forge_no_longer_resolves_it() {
        let found = Ok(Some(PrView::NoPr));
        assert_eq!(pin_outcome(found).unwrap(), Some(PrView::NoPr), "a read pin is the answer");
        assert_eq!(pin_outcome(Ok(None)).unwrap(), None, "a null node falls back");
        let missing = "gh: Could not resolve to a PullRequest with the number of 999999.";
        assert!(matches!(classify_failure(missing, "github.com"), GhError::NotFound(_)));
        assert_eq!(pin_outcome(Err(classify_failure(missing, "github.com"))).unwrap(), None);
        // Anything else is a real failure: it surfaces instead of hiding behind the lookup.
        assert!(pin_outcome(Err(GhError::Other("gh: HTTP 502".into()))).is_err());
        assert!(pin_outcome(Err(GhError::NotAuthed("github.com".into()))).is_err());
    }

    #[test]
    fn each_provider_reads_only_its_own_forges_pin() {
        let mut local = input("head", &["feat"]).local;
        assert!(local.pin_on(crate::git::Forge::GitHub).is_none());
        local.pin = Some(crate::git::PrPin { repo: gh("acme", "widgets"), number: 108 });
        assert_eq!(local.pin_on(crate::git::Forge::GitHub).map(|p| p.number), Some(108));
        assert!(local.pin_on(crate::git::Forge::GitLab).is_none());
    }

    #[test]
    fn a_pull_request_attaches_only_when_its_head_is_one_of_the_branchs() {
        let upstream = gh("acme", "widgets");
        let fork = gh("contributor", "widgets-fork");
        // (head_ref, isCrossRepository, headRepository) as GitHub reports a node.
        let node = |head_ref: &str, cross: bool, head_repo: Option<&str>| {
            serde_json::json!({"number": 1, "state": "MERGED", "headRefOid": "abc",
                "headRefName": head_ref, "createdAt": "", "closedAt": "",
                "isCrossRepository": cross,
                "headRepository": head_repo.map(|n| serde_json::json!({"nameWithOwner": n}))})
        };
        let admitted = |queried: &crate::git::RepoTarget,
                        heads: &[crate::git::Head],
                        node: serde_json::Value| {
            let v = serde_json::json!({"data": {"repository": {
                "o0": {"nodes": []}, "h0": {"nodes": [node]}}}});
            !parse_branch_lookup(&v, 1, queried, heads).history.is_empty()
        };
        let on_main = [head(upstream.clone(), "main")];
        let checkout = [head(fork.clone(), "fix-typo"), head(upstream.clone(), "fix-typo")];
        let cases: &[(
            &str,
            &crate::git::RepoTarget,
            &[crate::git::Head],
            serde_json::Value,
            bool,
        )] = &[
            // The hole: a stranger's fork PR from their `main` never attaches to upstream `main`.
            (
                "stranger fork main on main",
                &upstream,
                &on_main,
                node("main", true, Some("stranger/widgets")),
                false,
            ),
            (
                "own same-repo main",
                &upstream,
                &on_main,
                node("main", false, Some("acme/widgets")),
                true,
            ),
            // #105: the fork `gh pr checkout` recorded attaches, by repo and name.
            (
                "checked-out fork PR",
                &upstream,
                &checkout,
                node("fix-typo", true, Some("Contributor/Widgets-Fork")),
                true,
            ),
            (
                "same name, another fork",
                &upstream,
                &checkout,
                node("fix-typo", true, Some("stranger/widgets")),
                false,
            ),
            // A renamed target: the same-repo flag decides, never the reported name.
            (
                "renamed target",
                &upstream,
                &on_main,
                node("main", false, Some("acme/renamed")),
                true,
            ),
            // A deleted fork nulls headRepository: it matches nothing.
            ("deleted fork", &upstream, &checkout, node("fix-typo", true, None), false),
            // A same-named repository on another host is another repository.
            (
                "fork on another host",
                &upstream,
                &[head(
                    crate::git::RepoTarget::new("ghe.corp.test", "contributor", "widgets-fork")
                        .unwrap(),
                    "fix-typo",
                )],
                node("fix-typo", true, Some("contributor/widgets-fork")),
                false,
            ),
            // Querying the fork: an upstream head is cross-repository there.
            (
                "upstream head, fork queried",
                &fork,
                &[head(upstream.clone(), "fix")],
                node("fix", false, Some("contributor/widgets-fork")),
                false,
            ),
            // A fork clone's branch that only tracks upstream main has no upstream head.
            (
                "fork clone, upstream's own fix",
                &upstream,
                &[head(fork.clone(), "fix")],
                node("fix", false, Some("acme/widgets")),
                false,
            ),
            (
                "fork clone, its own fix",
                &upstream,
                &[head(fork.clone(), "fix")],
                node("fix", true, Some("contributor/widgets-fork")),
                true,
            ),
            // The fork's own PRs, queried in the fork: same-repo there.
            (
                "fork's internal PR",
                &fork,
                &[head(fork.clone(), "fix")],
                node("fix", false, Some("contributor/widgets-fork")),
                true,
            ),
        ];
        for (label, queried, heads, node, expected) in cases {
            assert_eq!(admitted(queried, heads, node.clone()), *expected, "{label}");
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
        // The open path never touches git; `tests/pr_candidates.rs` covers history.
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
    fn pending_review_comments_are_drafts_and_lead() {
        let reviews = serde_json::json!([
            {"author": {"login": "me"}, "state": "PENDING", "body": "Summary draft.",
             "submittedAt": null, "databaseId": 77},
            {"author": {"login": "ann"}, "state": "COMMENTED", "body": "Done.",
             "submittedAt": "2026-06-27T09:00:00Z", "databaseId": 70}
        ]);
        let threads = serde_json::json!([
            {"isResolved": false, "isOutdated": false, "path": "a.rs", "line": 4,
             "diffSide": "RIGHT", "comments": {"nodes": [
                {"author": {"login": "me"}, "body": "Pending finding.", "state": "PENDING",
                 "databaseId": 345, "createdAt": "2026-06-27T08:00:00Z"}
             ]}},
            {"isResolved": false, "isOutdated": false, "path": "b.rs", "line": 9,
             "diffSide": "RIGHT", "comments": {"nodes": [
                {"author": {"login": "ann"}, "body": "Published.", "state": "SUBMITTED",
                 "databaseId": 300, "createdAt": "2026-06-27T12:00:00Z"},
                {"author": {"login": "me"}, "body": "Pending reply.", "state": "PENDING",
                 "databaseId": 346, "createdAt": "2026-06-27T12:30:00Z"}
             ]}}
        ]);
        let out = merge_comments(&reviews, &serde_json::json!([]), &threads);
        assert_eq!(out.len(), 4);
        // The three rows with a draft lead, newest-first among themselves.
        assert_eq!(out[0].body, "Published.");
        assert_eq!(out[0].draft_id, None);
        assert_eq!(out[0].replies[0].draft_id, Some(346));
        assert_eq!(out[1].body, "Pending finding.");
        assert_eq!((out[1].draft_id, out[1].anchor.as_str()), (Some(345), "a.rs:4"));
        assert_eq!(out[2].body, "Summary draft.");
        assert_eq!((out[2].draft_id, out[2].kind), (Some(77), CommentKind::Review));
        assert_eq!(out[3].body, "Done.");
        assert_eq!(out[3].draft_id, None, "a submitted review is no draft");
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
             "comments": {"nodes": [
                {"author": {"login": "claude[bot]"}, "body": "SSRF", "createdAt": "2026-06-27T11:00:00Z"},
                {"author": {"login": "persijano"}, "body": "Addressed in abc", "createdAt": "2026-06-27T11:30:00Z"}
             ]}}
        ]);
        let cs = merge_comments(&reviews, &issues, &threads);
        assert_eq!(cs.len(), 3);
        // The full order, so an unstable comparator fails.
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
        assert_eq!(f.replies.len(), 1);
        assert_eq!(f.replies[0].author, "persijano");
        assert_eq!(f.replies[0].body, "Addressed in abc");
    }

    #[test]
    fn a_short_thread_page_does_not_land() {
        let incomplete = serde_json::json!({
            "id": "T1",
            "comments": {"pageInfo": {"hasNextPage": true, "endCursor": ""}, "nodes": [{"body": "root"}]}
        });
        assert!(next_thread_page(&incomplete).is_err(), "empty cursor is a failed page");
        let missing_id = serde_json::json!({
            "comments": {"pageInfo": {"hasNextPage": true, "endCursor": "c1"}, "nodes": [{"body": "root"}]}
        });
        assert!(next_thread_page(&missing_id).is_err(), "missing id is a failed page");
        let done = serde_json::json!({
            "id": "T1",
            "comments": {"pageInfo": {"hasNextPage": false, "endCursor": "c1"}, "nodes": [{"body": "root"}]}
        });
        assert_eq!(next_thread_page(&done).unwrap(), None);
        let more = serde_json::json!({
            "id": "T1",
            "comments": {"pageInfo": {"hasNextPage": true, "endCursor": "c1"}, "nodes": [{"body": "root"}]}
        });
        assert_eq!(next_thread_page(&more).unwrap(), Some(("T1".into(), "c1".into())));
        let null_page = serde_json::json!({"data": {"node": {}}});
        let mut thread = more;
        assert!(append_thread_comment_page(&mut thread, &null_page).is_err());
    }

    #[test]
    fn an_empty_leading_github_note_is_not_the_root() {
        let threads = serde_json::json!([{
            "isResolved": false, "isOutdated": false, "path": "a.rs", "line": 1,
            "comments": {"nodes": [
                {"author": {"login": "bot"}, "body": "  ", "createdAt": "2026-06-27T11:00:00Z"},
                {"author": {"login": "bot"}, "body": "the finding", "createdAt": "2026-06-27T11:01:00Z"},
                {"author": {"login": "ann"}, "body": "Addressed", "createdAt": "2026-06-27T11:02:00Z"}
            ]}
        }]);
        let cs = merge_comments(&serde_json::json!([]), &serde_json::json!([]), &threads);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].body, "the finding");
        assert_eq!(cs[0].replies.len(), 1);
        assert_eq!(cs[0].replies[0].body, "Addressed");
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
        // A bot's prose folds to one; its findings never do.
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
        assert_eq!(cs.iter().find(|c| c.body == "finding one").unwrap().anchor, "a.py:10");
    }

    #[test]
    fn a_finding_anchor_is_the_thread_range() {
        assert_eq!(finding_anchor("a.rs", Some(10), Some(12)), "a.rs:10-12");
        assert_eq!(finding_anchor("a.rs", Some(10), Some(10)), "a.rs:10");
        assert_eq!(finding_anchor("a.rs", None, Some(7)), "a.rs:7");
        assert_eq!(finding_anchor("a.rs", None, None), "a.rs");
        let p = FindingPlace::from_anchor("a.rs:10-12", Some(crate::model::Side::New));
        assert_eq!(p.path, "a.rs");
        assert_eq!(p.range, Some((10, 12)));
        let p = FindingPlace::from_anchor("a.rs:10", Some(crate::model::Side::New));
        assert_eq!(p.range, Some((10, 10)));
        let p = FindingPlace::from_anchor("a.rs", None);
        assert_eq!(p.range, None);
        assert_eq!(finding_range_caption(10, 12, Some('+')), "Comment on lines +10 to +12");
        assert_eq!(finding_range_caption(22, 23, Some('-')), "Comment on lines -22 to -23");
        assert_eq!(finding_range_caption(7, 7, None), "Comment on line 7");

        let threads = serde_json::json!([
            {"isResolved": false, "isOutdated": false, "path": "a.py",
             "startLine": 10, "line": 12, "originalStartLine": 9, "originalLine": 11,
             "diffSide": "RIGHT",
             "comments": {"totalCount": 1, "nodes": [{"author": {"login": "ann"}, "body": "r", "createdAt": "2026-06-27T10:00:00Z"}]}},
            {"isResolved": false, "isOutdated": true, "path": "gone.py",
             "startLine": null, "line": null, "originalStartLine": 3, "originalLine": 5,
             "diffSide": "LEFT",
             "comments": {"totalCount": 1, "nodes": [{"author": {"login": "ann"}, "body": "old", "createdAt": "2026-06-27T11:00:00Z"}]}}
        ]);
        let cs = merge_comments(&serde_json::json!([]), &serde_json::json!([]), &threads);
        assert_eq!(cs.iter().find(|c| c.body == "r").unwrap().anchor, "a.py:10-12");
        assert_eq!(
            cs.iter().find(|c| c.body == "r").unwrap().place.as_ref().unwrap().side,
            Some(crate::model::Side::New)
        );
        assert_eq!(cs.iter().find(|c| c.body == "old").unwrap().anchor, "gone.py:3-5");
        assert_eq!(
            cs.iter().find(|c| c.body == "old").unwrap().place.as_ref().unwrap().side,
            Some(crate::model::Side::Old)
        );
    }

    #[test]
    fn an_undated_bot_review_survives_beside_the_bots_dated_prose() {
        // Undated reviews (approvals, votes) survive newest-wins dedup.
        let row = |kind, anchor: &str, body: &str, created_at: &str| Comment {
            kind,
            author: "claude[bot]".to_string(),
            author_is_bot: true,
            anchor: anchor.to_string(),
            place: None,
            body: body.to_string(),
            snippet: None,
            created_at: created_at.to_string(),
            is_resolved: false,
            is_outdated: false,
            replies: Vec::new(),
            draft_id: None,
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
    fn parse_iso_reads_every_forge_timestamp_as_epoch_seconds() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2000-02-29T00:00:00Z"), Some(951_782_400)); // a leap-day boundary
        assert_eq!(parse_iso("2000-02-29T02:00:00+02:00"), Some(951_782_400), "a zone offset");
        assert_eq!(parse_iso("1970-01-01T00:00:00.1234567Z"), Some(0), "Azure's 7-digit fraction");
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
