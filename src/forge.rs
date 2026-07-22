//! Read-only forge access: the pull request's identity, state, checks, and comments.
//!
//! See `specs/forge-host.md`. A fetch first derives [`PrFetchInput`] from local Git and one
//! validated config snapshot, then reads its canonical target through explicitly hosted CLI
//! calls — the GitHub queries live in [`github`]. It never posts, resolves, re-runs, merges,
//! or otherwise writes to the forge. The `PR` tab renders the [`PrSnapshot`] this module
//! produces; degradation is in-band as [`PrView`].

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

mod github;
mod gitlab;

/// What the `PR` tab shows: the resolved snapshot, or a degraded state with its own remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrView {
    /// Work is pending but has not crossed the loading-indicator delay.
    Pending,
    /// Work crossed the loading-indicator delay without producing a snapshot.
    Loading,
    /// An open (or merged/closed) PR resolved through the worktree's publication points.
    Pr(Box<PrSnapshot>),
    /// No PR contains the worktree's published work.
    NoPr,
    /// `HEAD` is detached, so there is no branch identity to query.
    Detached,
    /// Two or more open PRs contain the published work and no tiebreak decides; the
    /// count, so the user knows to pick on the forge.
    Ambiguous(usize),
    /// The target forge's CLI (`gh`/`glab`) is not on `PATH`.
    NoCli(crate::git::Forge),
    /// The forge CLI is installed but not authenticated for this canonical host.
    NotAuthed(crate::git::Forge, String),
    /// Neither `upstream` nor `origin` names a supported hosted Git repository.
    NeedsForgeRemote,
    /// The fallback `origin` names a hosted forge outside the supported hosts.
    UnsupportedHost(String),
    /// The fallback `origin` names a supported host but not an owner/repository path.
    MalformedOrigin(String),
    /// A local Git read failed before the forge fetch could start.
    GitError(String),
    /// Any other forge-CLI failure (rate limit, offline, …); the app freezes the last good view.
    Error(String),
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
                forge.name(),
                forge.cli()
            )),
            Self::NotAuthed(forge, host) => Some(format!(
                "Not signed in to {host}. Run `{} auth login --hostname {host}`, then press {refresh}.",
                forge.cli()
            )),
            Self::GitError(message) => {
                Some(format!("Git read failed: {message}. Press {refresh} to retry."))
            }
            Self::Error(message) => {
                Some(format!("Forge unavailable: {message}. Press {refresh} to retry."))
            }
            _ => None,
        }
    }
}

/// One pull request's state, read fresh from the forge each poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnapshot {
    /// The forge this snapshot was read from — names the remote surface in UI chrome.
    pub forge: crate::git::Forge,
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The PR description as GitHub returns it, empty when none (`specs/forge-host.md`).
    pub body: String,
    pub state: PrState,
    pub is_draft: bool,
    /// The PR's head branch name — the candidate that resolved, which may differ from the
    /// worktree's local branch name (`specs/forge-host.md`).
    pub head_ref: String,
    /// The head branch lives in another repository (GitHub's `isCrossRepository`); shown
    /// as a marker so a same-named fork PR is visible.
    pub head_is_fork: bool,
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

/// Whether the PR has a merge blocker worth surfacing, folded from GitHub's `mergeable` and
/// `mergeStateStatus`. Only the actionable blockers are modelled; GitHub's `behind` / `unstable`
/// / still-`checking` states carry nothing a reviewer acts on, so they fold into `Clean`.
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

/// One forge-CLI subprocess failure, before backend-specific stderr classification.
enum CliFailure {
    /// The binary is not on `PATH`.
    Missing,
    /// A spawn or wait error, or a cancellation — never classified further.
    Other(String),
    /// A non-zero exit; the backend classifies the stderr wording.
    Stderr(String),
}

/// Run `program` with `args` in `repo` and return stdout or an unclassified failure.
fn run_cli(
    program: &str,
    repo: &Path,
    args: &[&str],
    cancelled: &AtomicBool,
) -> Result<String, CliFailure> {
    let child = Command::new(program)
        .current_dir(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliFailure::Missing);
        }
        Err(error) => return Err(CliFailure::Other(error.to_string())),
    };

    // Drain both pipes while polling so a large API response cannot fill a pipe and block
    // the child before it exits. A superseded config/fetch kills the process; the coordinator
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
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(CliFailure::Other(error.to_string()));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if cancelled.load(Ordering::Acquire) {
        return Err(CliFailure::Other("request cancelled".to_string()));
    }
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    Err(CliFailure::Stderr(String::from_utf8_lossy(&stderr).into_owned()))
}

/// A classified forge-CLI failure, mapped to a [`PrView`] degraded state.
#[derive(Debug, PartialEq, Eq)]
enum CliError {
    Missing(crate::git::Forge),
    NotAuthed(crate::git::Forge, String),
    LocalGit(String),
    Other(String),
}

impl From<CliError> for PrView {
    fn from(e: CliError) -> Self {
        match e {
            CliError::Missing(forge) => PrView::NoCli(forge),
            CliError::NotAuthed(forge, host) => PrView::NotAuthed(forge, host),
            CliError::LocalGit(message) => PrView::GitError(message),
            CliError::Other(m) => PrView::Error(m),
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
    let (repository, origin_repository) = crate::git::remote_identities(repo, config.forge_hosts())
        .map_err(|error| PrInputError::TargetRead(error.0))?;
    let crate::git::RepositoryIdentity::Repository(target) = &repository else {
        return Ok(PrFetchInput {
            repository,
            origin_repository: None,
            local: crate::git::PrLocalState::default(),
        });
    };
    let local = match crate::git::pr_local(repo, base, config.base_branches()) {
        Ok(local) => local,
        Err(error) => {
            let (current, _) = crate::git::remote_identities(repo, config.forge_hosts())
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
        crate::git::remote_identities(repo, config.forge_hosts())
            .map_err(|error| PrInputError::TargetRead(error.0))?
    } else {
        (repository, origin_repository)
    };
    Ok(PrFetchInput { repository, origin_repository, local })
}

/// Read the forge for one already-derived input. Degradation stays in-band for the PR tab.
#[must_use]
pub fn fetch(repo: &Path, input: &PrFetchInput) -> PrView {
    fetch_cancellable(repo, input, &AtomicBool::new(false))
}

/// Read the forge with a cancellation signal owned by the event-loop coordinator.
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
) -> Result<PrView, CliError> {
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
    if input.local.points.is_empty()
        && input.local.absorbed.is_empty()
        && nominated_head(&input.local).is_none()
    {
        // No published work beyond the base, no parked published tip, and no
        // exact-identity HEAD — nothing can prove a PR, so nothing is fetched
        // (`specs/forge-host.md`).
        return Ok(PrView::NoPr);
    }
    let forge = repository.forge();
    let target = FetchTarget {
        repo,
        host: repository.host(),
        owner: repository.owner(),
        name: repository.name(),
        cancelled,
    };
    let source = select_source(input.origin_repository.as_ref(), repository);
    let head = nominated_head(&input.local);
    let assoc = match forge {
        crate::git::Forge::GitHub => github::associate_points(
            &target,
            source,
            &input.local.points,
            &input.local.absorbed,
            head,
        )?,
        crate::git::Forge::GitLab => gitlab::associate_points(
            &target,
            source,
            &input.local.points,
            &input.local.absorbed,
            head,
        )?,
    };
    let number = match pick_open(&assoc.open, input) {
        Pick::One(n) => n,
        Pick::Ambiguous(count) => {
            return Ok(PrView::Ambiguous(count));
        }
        Pick::None => match pick_merged(&assoc.merged).or_else(|| pick_closed(&assoc.closed)) {
            Some(n) => n,
            None => {
                return Ok(PrView::NoPr);
            }
        },
    };
    let detail = match forge {
        crate::git::Forge::GitHub => github::pr_detail(&target, number)?,
        crate::git::Forge::GitLab => gitlab::pr_detail(&target, number)?,
    };
    let Some((node, pr_head)) = detail else {
        return Ok(PrView::NoPr);
    };
    // Sync compares the fetch's pinned HEAD to the PR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's PR with another branch's count.
    let sync = match input.local.head_oid.as_deref() {
        Some(pin) if !pr_head.is_empty() => derive_sync(
            crate::git::ahead_behind_oids(repo, pin, &pr_head)
                .map_err(|error| CliError::LocalGit(error.0))?,
        ),
        _ => Sync::Unknown,
    };
    Ok(PrView::Pr(Box::new(match forge {
        crate::git::Forge::GitHub => github::build_snapshot(&node, sync),
        crate::git::Forge::GitLab => gitlab::build_snapshot(&node, sync),
    })))
}

/// The local branch's position relative to the PR head, from `git`'s ahead/behind counts. A
/// diverged branch (both nonzero) leads with the unpushed count — the headline case. `None`
/// (the PR head isn't local yet) stays explicitly unknown rather than guessing.
fn derive_sync(ahead_behind: Option<(u32, u32)>) -> Sync {
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

/// One PR from the association query, reduced to the pick-relevant fields.
#[derive(Debug)]
struct AssocPr {
    number: u64,
    head_oid: String,
    head_ref: String,
    merged_at: String,
    created_at: String,
}

/// The association result, split by lifecycle: open and merged from the commit
/// association, closed-unmerged from the exact-identity name lookup.
#[derive(Debug, Default)]
struct Association {
    open: Vec<AssocPr>,
    merged: Vec<AssocPr>,
    closed: Vec<AssocPr>,
}

/// The repository the association query runs against: the origin repository, where the
/// published commits live — the fork case resolves through it (`specs/forge-host.md`). An
/// origin on another host cannot prove anything on the target's forge, so the target
/// stands in.
fn select_source<'a>(
    origin: Option<&'a crate::git::RepoTarget>,
    target: &'a crate::git::RepoTarget,
) -> &'a crate::git::RepoTarget {
    origin.filter(|origin| origin.host() == target.host()).unwrap_or(target)
}

/// The pinned `HEAD` as one more exact-identity nomination, unless a point already
/// carries the same OID. A nominating `HEAD` sits outside base history, so it can never
/// be an absorbed candidate (`specs/forge-host.md`).
fn nominated_head(local: &crate::git::PrLocalState) -> Option<&str> {
    local
        .head_oid
        .as_deref()
        .filter(|head| local.head_nominates && !local.points.iter().any(|point| point.oid == *head))
}

/// Push `pr` unless its number is already in `bucket` — a PR's identity is its number.
fn push_unique(bucket: &mut Vec<AssocPr>, pr: AssocPr) {
    if !bucket.iter().any(|have| have.number == pr.number) {
        bucket.push(pr);
    }
}

/// The winner among the open PRs (`specs/forge-host.md` "Resolution").
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    One(u64),
    Ambiguous(usize),
    None,
}

/// Pick the open PR: a lone PR wins; several disambiguate by a head equal to the pinned
/// `HEAD`, then a head equal to a publication point, then the head named by the recorded
/// upstream — each only when exactly one matches. Failing all three, the count surfaces.
fn pick_open(open: &[AssocPr], input: &PrFetchInput) -> Pick {
    match open {
        [] => Pick::None,
        [only] => Pick::One(only.number),
        many => {
            let unique = |test: &dyn Fn(&AssocPr) -> bool| -> Option<u64> {
                let mut hits = many.iter().filter(|pr| test(pr));
                match (hits.next(), hits.next()) {
                    (Some(pr), None) => Some(pr.number),
                    _ => None,
                }
            };
            if let Some(pin) = input.local.head_oid.as_deref()
                && let Some(number) = unique(&|pr| pr.head_oid == pin)
            {
                return Pick::One(number);
            }
            if let Some(number) =
                unique(&|pr| input.local.points.iter().any(|point| point.oid == pr.head_oid))
            {
                return Pick::One(number);
            }
            if let Some(upstream) = input.local.upstream.as_deref()
                && let Some(number) = unique(&|pr| pr.head_ref == upstream)
            {
                return Pick::One(number);
            }
            Pick::Ambiguous(many.len())
        }
    }
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

/// The newest-merged PR containing a publication point.
fn pick_merged(merged: &[AssocPr]) -> Option<u64> {
    newest_by(merged, |pr| &pr.merged_at)
}

/// The newest closed-unmerged PR whose head is exactly a publication point.
fn pick_closed(closed: &[AssocPr]) -> Option<u64> {
    newest_by(closed, |pr| &pr.created_at)
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
            || keep_newest.get(&c.author) == Some(&c.created_at)
    });
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

    fn point(oid: &str, names: &[&str]) -> crate::git::PublicationPoint {
        crate::git::PublicationPoint {
            oid: oid.to_string(),
            names: names.iter().map(|n| (*n).to_string()).collect(),
        }
    }

    fn input(
        head: &str,
        points: Vec<crate::git::PublicationPoint>,
        up: Option<&str>,
    ) -> PrFetchInput {
        PrFetchInput {
            repository: crate::git::RepositoryIdentity::Missing,
            origin_repository: None,
            local: crate::git::PrLocalState {
                head_oid: Some(head.to_string()),
                base_oid: Some("base".to_string()),
                points,
                absorbed: Vec::new(),
                head_nominates: false,
                upstream: up.map(str::to_string),
                detached: false,
            },
        }
    }

    fn assoc(number: u64, head_oid: &str, head_ref: &str) -> AssocPr {
        AssocPr {
            number,
            head_oid: head_oid.to_string(),
            head_ref: head_ref.to_string(),
            merged_at: String::new(),
            created_at: String::new(),
        }
    }

    #[test]
    fn fetch_gates_resolve_without_touching_the_forge() {
        // Each early gate returns before any `gh` spawn: identity failures, a detached
        // HEAD, and a worktree with no publication points (`specs/forge-host.md`).
        let gated = |input: &PrFetchInput| fetch(Path::new("."), input);
        let mut missing = input("head", vec![], None);
        missing.repository = crate::git::RepositoryIdentity::Missing;
        assert_eq!(gated(&missing), PrView::NeedsForgeRemote);

        let mut unsupported = input("head", vec![], None);
        unsupported.repository =
            crate::git::RepositoryIdentity::Unsupported("bitbucket.org".into());
        assert_eq!(gated(&unsupported), PrView::UnsupportedHost("bitbucket.org".into()));

        let repo = crate::git::RepositoryIdentity::Repository(
            crate::git::RepoTarget::new(crate::git::Forge::GitHub, "github.com", "owner", "repo")
                .unwrap(),
        );
        let mut detached = input("head", vec![], None);
        detached.repository = repo.clone();
        detached.local.detached = true;
        assert_eq!(gated(&detached), PrView::Detached);

        let mut zero_work = input("head", vec![], None);
        zero_work.repository = repo;
        assert_eq!(gated(&zero_work), PrView::NoPr);
    }

    #[test]
    fn the_head_nominates_only_when_flagged_and_not_already_a_point() {
        // The gate and the alias both route through this one filter, so a regression
        // that drops the nomination re-tightens the NoPr gate and fails here.
        let mut local = input("tip", vec![], None).local;
        assert_eq!(nominated_head(&local), None, "an unflagged HEAD never nominates");
        local.head_nominates = true;
        assert_eq!(nominated_head(&local), Some("tip"), "a flagged HEAD nominates alone");
        local.points = vec![point("tip", &["feat"])];
        assert_eq!(nominated_head(&local), None, "a point already carries the OID");
    }

    #[test]
    fn pick_open_prefers_head_then_point_then_upstream_else_surfaces_the_count() {
        let one = [assoc(1, "aaa", "feat")];
        assert_eq!(pick_open(&one, &input("zzz", vec![], None)), Pick::One(1));
        assert_eq!(pick_open(&[], &input("zzz", vec![], None)), Pick::None);

        let two = [assoc(1, "aaa", "feat"), assoc(2, "bbb", "cont")];
        assert_eq!(pick_open(&two, &input("bbb", vec![], None)), Pick::One(2));
        assert_eq!(pick_open(&two, &input("zzz", vec![point("aaa", &[])], None)), Pick::One(1));
        assert_eq!(pick_open(&two, &input("zzz", vec![], Some("cont"))), Pick::One(2));
        assert_eq!(pick_open(&two, &input("zzz", vec![], None)), Pick::Ambiguous(2));
        // A tiebreak matching several PRs decides nothing.
        let dup = [assoc(1, "aaa", "feat"), assoc(2, "aaa", "feat")];
        assert_eq!(pick_open(&dup, &input("aaa", vec![], Some("feat"))), Pick::Ambiguous(2));
        // Tiers outrank, not merely win in isolation: with the pinned HEAD on one PR and
        // both lower tiers pointing at the other, the HEAD tier decides. Same one rung
        // down — a point identity beats an upstream name, per "names never prove identity".
        let crossed = [assoc(1, "aaa", "feat"), assoc(2, "bbb", "cont")];
        assert_eq!(
            pick_open(&crossed, &input("aaa", vec![point("bbb", &[])], Some("cont"))),
            Pick::One(1)
        );
        assert_eq!(
            pick_open(&crossed, &input("zzz", vec![point("aaa", &[])], Some("cont"))),
            Pick::One(1)
        );
    }

    #[test]
    fn source_selection_prefers_a_same_host_origin_else_the_target() {
        let github = |host: &str, owner: &str, name: &str| {
            crate::git::RepoTarget::new(crate::git::Forge::GitHub, host, owner, name).unwrap()
        };
        let target = github("github.com", "acme", "widgets");
        let fork = github("github.com", "contributor", "widgets");
        let foreign = github("ghe.corp.test", "me", "widgets");
        assert_eq!(select_source(Some(&fork), &target), &fork);
        assert_eq!(select_source(Some(&foreign), &target), &target);
        assert_eq!(select_source(None, &target), &target);
    }

    #[test]
    fn merged_pick_takes_the_newest_merge_and_closed_pick_the_newest_created() {
        let merged = [
            AssocPr { merged_at: "2026-06-01T00:00:00Z".into(), ..assoc(1, "a", "x") },
            AssocPr { merged_at: "2026-06-03T00:00:00Z".into(), ..assoc(2, "b", "y") },
            AssocPr { merged_at: "2026-06-03T00:00:00Z".into(), ..assoc(3, "c", "z") }, // tie → earlier
        ];
        assert_eq!(pick_merged(&merged), Some(2));
        assert_eq!(pick_merged(&[]), None);
        let closed = [
            AssocPr { created_at: "2026-05-01T00:00:00Z".into(), ..assoc(4, "d", "x") },
            AssocPr { created_at: "2026-05-09T00:00:00Z".into(), ..assoc(5, "e", "y") },
        ];
        assert_eq!(pick_closed(&closed), Some(5));
    }

    #[test]
    fn rollup_fails_on_any_failure_else_running_else_success() {
        let snap = |statuses: &[CheckStatus]| PrSnapshot {
            forge: crate::git::Forge::GitHub,
            number: 1,
            title: String::new(),
            url: String::new(),
            body: String::new(),
            state: PrState::Open,
            is_draft: false,
            head_ref: String::new(),
            head_is_fork: false,
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

    #[test]
    fn sync_leads_with_unpushed_and_tolerates_a_missing_head() {
        assert_eq!(derive_sync(None), Sync::Unknown);
        assert_eq!(derive_sync(Some((0, 0))), Sync::InSync);
        assert_eq!(derive_sync(Some((2, 0))), Sync::Unpushed(2));
        assert_eq!(derive_sync(Some((0, 3))), Sync::Behind(3));
        assert_eq!(derive_sync(Some((2, 3))), Sync::Unpushed(2)); // diverged → unpushed leads
    }

    #[test]
    fn local_git_failures_degrade_to_the_git_error_view() {
        assert_eq!(
            PrView::from(CliError::LocalGit("rev-list failed".into())),
            PrView::GitError("rev-list failed".into())
        );
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
}
