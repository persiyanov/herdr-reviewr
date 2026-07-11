//! Forge-neutral core: the PR tab's snapshot model, degraded-state view, and per-forge dispatch.
//!
//! See `specs/forge-host.md`. A fetch first derives [`PrFetchInput`] from local Git and one
//! validated config snapshot, then dispatches on the origin's classified [`crate::git::Forge`]
//! to a backend (`github`, and later `gitlab`/`bitbucket`) that reads the canonical target and
//! returns the finished [`PrView`]. A backend never posts, resolves, re-runs, merges, or
//! otherwise writes to the forge. The `PR` tab renders the [`PrSnapshot`] a backend produces;
//! degradation is in-band as [`PrView`].

mod bitbucket;
mod github;
mod gitlab;
mod proc;

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

/// What the `PR` tab shows: the resolved snapshot, or a degraded state with its own remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrView {
    /// Work is pending but has not crossed the loading-indicator delay.
    Pending,
    /// Work crossed the loading-indicator delay without producing a snapshot.
    Loading,
    /// An open (or merged/closed) PR resolved for one of the worktree's candidate branches.
    Pr(Box<PrSnapshot>),
    /// No candidate branch has a PR; the queried candidate names, so the empty state can
    /// say what was looked for. Empty on a detached `HEAD` (nothing was queried).
    NoPr(Vec<String>),
    /// Two or more open PRs back the winning candidate branch and not exactly one matches
    /// the pinned `HEAD`; the count, so the user knows to pick on the forge.
    Ambiguous(usize),
    /// The forge's CLI tool is not on `PATH` ("gh", "glab").
    NoCli(&'static str),
    /// The tool is present but not authenticated for this canonical host.
    NotAuthed { forge: crate::git::Forge, host: String },
    /// Bitbucket only: no token in `BITBUCKET_TOKEN` or git-credential for this host.
    NoToken(String),
    /// Origin is missing or has no hosted Git URL.
    NeedsSupportedOrigin,
    /// Origin names a hosted forge outside the supported hosts.
    UnsupportedHost(String),
    /// Origin names a supported host but not an owner/repository path.
    MalformedOrigin(String),
    /// Any other backend failure (rate limit, offline, …); the app freezes the last good view.
    Error(String),
}

impl PrView {
    /// A same-input failure that can be retried without discarding the visible snapshot.
    /// Both snapshot preservation and the empty-state renderer consume this projection so a
    /// newly added retryable failure cannot diverge between those surfaces.
    pub fn retry_remedy(&self) -> Option<String> {
        match self {
            Self::NoCli(tool) => Some(format!("{tool} not found — install `{tool}`, then press r")),
            Self::NotAuthed { forge: crate::git::Forge::GitHub, host } => {
                Some(format!("not signed in — run `gh auth login --hostname {host}`, then press r"))
            }
            Self::NotAuthed { forge: crate::git::Forge::GitLab, host } => Some(format!(
                "not signed in — run `glab auth login --hostname {host}`, then press r"
            )),
            Self::NotAuthed { forge: crate::git::Forge::Bitbucket, host } => {
                Some(format!("not signed in — check BITBUCKET_TOKEN for {host}, then press r"))
            }
            Self::NoToken(host) => Some(format!(
                "no token for {host} — set BITBUCKET_TOKEN or add it to git credentials, then press r"
            )),
            Self::Error(message) => {
                Some(format!("forge unavailable — {message}; press r to retry now"))
            }
            _ => None,
        }
    }
}

/// One pull request's state, read fresh from the forge each poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnapshot {
    pub number: u64,
    pub title: String,
    pub url: String,
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
    /// returned — the lists shown are a prefix, not the whole set. Drives a "more on GitHub" marker.
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

/// A classified backend failure, mapped to a [`PrView`] degraded state by the core.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ForgeError {
    /// The forge's CLI tool is not on `PATH` ("gh", "glab", "curl").
    NoCli(&'static str),
    /// The tool is present but not authenticated for this host.
    NotAuthed {
        forge: crate::git::Forge,
        host: String,
    },
    /// Bitbucket only: no token in `BITBUCKET_TOKEN` or git-credential for this host.
    NoToken(String),
    Other(String),
}

impl From<ForgeError> for PrView {
    fn from(e: ForgeError) -> Self {
        match e {
            ForgeError::NoCli(tool) => PrView::NoCli(tool),
            ForgeError::NotAuthed { forge, host } => PrView::NotAuthed { forge, host },
            ForgeError::NoToken(host) => PrView::NoToken(host),
            ForgeError::Other(m) => PrView::Error(m),
        }
    }
}

/// Everything a backend needs for one fetch. Candidates are already derived and capped.
pub(crate) struct FetchTarget<'a> {
    pub repo: &'a Path,
    pub host: &'a str,
    pub owner: &'a str,
    pub name: &'a str,
    pub cancelled: &'a AtomicBool,
}

/// Backend contract: resolve + read one PR/MR for the input, or a typed error. The core
/// handles Missing/Unsupported/Malformed origins and empty candidates before dispatch.
/// Returns the snapshot-or-empty `PrView` variants only (`Pr` / `NoPr` / `Ambiguous`).
pub(crate) fn backend_fetch(
    forge: crate::git::Forge,
    target: &FetchTarget<'_>,
    input: &PrFetchInput,
) -> Result<PrView, ForgeError> {
    match forge {
        crate::git::Forge::GitHub => github::fetch(target, input),
        crate::git::Forge::GitLab => gitlab::fetch(target, input),
        crate::git::Forge::Bitbucket => bitbucket::fetch(target, input),
    }
}

/// Every local and configuration value that identifies one PR fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFetchInput {
    pub origin: crate::git::OriginIdentity,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub candidates: Vec<String>,
    pub base: Option<String>,
    pub base_branches: Vec<String>,
}

/// Derive one complete fetch input without contacting the forge.
pub fn fetch_input(
    repo: &Path,
    base: Option<&str>,
    config: &crate::config::PluginConfig,
) -> Result<PrFetchInput, String> {
    let hosts = crate::git::ForgeHosts {
        github: config.github_host(),
        gitlab: config.gitlab_host(),
        bitbucket: config.bitbucket_host(),
    };
    let local = crate::git::pr_local(repo, base, config.base_branches(), &hosts)
        .map_err(|error| error.0)?;
    Ok(PrFetchInput {
        origin: local.origin,
        branch: local.branch,
        head_oid: local.head_oid,
        candidates: local.candidates,
        base: base.map(str::to_owned),
        base_branches: config.base_branches().to_vec(),
    })
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
    fetch_inner(repo, input, cancelled)
}

fn fetch_inner(repo: &Path, input: &PrFetchInput, cancelled: &AtomicBool) -> PrView {
    let t = match &input.origin {
        crate::git::OriginIdentity::Repository(target) => target,
        crate::git::OriginIdentity::Missing | crate::git::OriginIdentity::Hostless => {
            return PrView::NeedsSupportedOrigin;
        }
        crate::git::OriginIdentity::Unsupported(host) => {
            return PrView::UnsupportedHost(host.clone());
        }
        crate::git::OriginIdentity::Malformed(host) => {
            return PrView::MalformedOrigin(host.clone());
        }
    };
    if input.candidates.is_empty() {
        // A detached HEAD (e.g. after `gh pr merge --delete-branch`) has no branch identity
        // to publish, so nothing was derived. Show the empty state rather than querying
        // `headRefName:""`, which GitHub treats as unfiltered and would mis-resolve to an
        // unrelated PR.
        return PrView::NoPr(Vec::new());
    }
    let target = FetchTarget { repo, host: &t.host, owner: &t.owner, name: &t.name, cancelled };
    match backend_fetch(t.forge, &target, input) {
        Ok(view) => view,
        Err(e) => e.into(),
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

/// The winner among the candidates' open PRs (`specs/forge-host.md` "Resolution").
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Pick {
    One(u64),
    Ambiguous(usize),
    None,
}

/// Pick the open PR: the earliest candidate in derivation order holding any wins — the
/// recorded upstream outranks an inferred branch, which outranks the bare local name. On
/// one name backing several open PRs, exactly one head at the pinned `HEAD` wins; else the
/// ambiguity count is surfaced rather than a silent guess.
pub(crate) fn select_open(per_candidate: &[Vec<(u64, String)>], pinned_head: Option<&str>) -> Pick {
    for prs in per_candidate {
        match prs.as_slice() {
            [] => {}
            [(number, _)] => return Pick::One(*number),
            many => {
                if let Some(pin) = pinned_head {
                    let mut hits = many.iter().filter(|(_, oid)| oid == pin);
                    if let (Some((number, _)), None) = (hits.next(), hits.next()) {
                        return Pick::One(*number);
                    }
                }
                return Pick::Ambiguous(many.len());
            }
        }
    }
    Pick::None
}

/// The historical fallback: the newest-created merged/closed PR across all candidates.
/// ISO-8601 `…Z` strings compare lexically; a strict `>` keeps the earlier candidate on a
/// timestamp tie, so the pick is deterministic.
pub(crate) fn select_historical(per_candidate: &[Vec<(u64, String)>]) -> Option<u64> {
    let mut best: Option<(u64, &str)> = None;
    for prs in per_candidate {
        for (number, created) in prs {
            if best.is_none_or(|(_, b)| created.as_str() > b) {
                best = Some((*number, created));
            }
        }
    }
    best.map(|(number, _)| number)
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

/// Percent-encode for a URL path segment or query value: unreserved chars pass, all else %XX.
/// Shared by every backend that builds REST paths by hand (`gitlab`, `bitbucket`).
pub(crate) fn enc(s: &str) -> String {
    s.bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![b as char]
            }
            _ => format!("%{b:02X}").chars().collect(),
        })
        .collect()
}

/// Parse a fixed `YYYY-MM-DDTHH:MM:SSZ` timestamp to a Unix epoch second. `None` on any
/// deviation, so a malformed value yields an empty age rather than a wrong one.
// The civil-from-days algorithm reads naturally with the conventional short field names.
#[allow(clippy::many_single_char_names)]
pub(crate) fn parse_iso(s: &str) -> Option<i64> {
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
    fn rollup_fails_on_any_failure_else_running_else_success() {
        let snap = |statuses: &[CheckStatus]| PrSnapshot {
            number: 1,
            title: String::new(),
            url: String::new(),
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
    fn select_open_takes_the_earliest_candidate_with_any_open_pr() {
        let per = vec![
            vec![],
            vec![(12, "aaa".to_string())],
            vec![(99, "bbb".to_string())], // a later candidate never preempts an earlier one
        ];
        assert_eq!(select_open(&per, Some("zzz")), Pick::One(12));
        assert_eq!(select_open(&[vec![], vec![]], Some("zzz")), Pick::None);
        assert_eq!(select_open(&[], None), Pick::None);
    }

    #[test]
    fn select_open_disambiguates_one_name_by_the_pinned_head_else_surfaces_the_count() {
        let two = vec![vec![(1, "aaa".to_string()), (2, "bbb".to_string())]];
        assert_eq!(select_open(&two, Some("bbb")), Pick::One(2));
        // No pinned HEAD, no exact match, or several exact matches: ambiguous, count shown.
        assert_eq!(select_open(&two, None), Pick::Ambiguous(2));
        assert_eq!(select_open(&two, Some("zzz")), Pick::Ambiguous(2));
        let dup = vec![vec![(1, "aaa".to_string()), (2, "aaa".to_string())]];
        assert_eq!(select_open(&dup, Some("aaa")), Pick::Ambiguous(2));
    }

    #[test]
    fn select_historical_takes_the_newest_created_and_ties_to_the_earlier_candidate() {
        let per = vec![
            vec![(1, "2026-06-01T00:00:00Z".to_string())],
            vec![(2, "2026-06-03T00:00:00Z".to_string())],
            vec![(3, "2026-06-03T00:00:00Z".to_string())], // tie → the earlier candidate keeps
        ];
        assert_eq!(select_historical(&per), Some(2));
        assert_eq!(select_historical(&[vec![], vec![]]), None);
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
}
