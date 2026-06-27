//! Read-only GitHub access: the pull request's identity, state, checks, and comments.
//!
//! See `specs/forge-host.md`. Every call here only reads through `gh` — it never posts,
//! resolves, re-runs, merges, or otherwise writes to GitHub. The `PR` tab renders the
//! [`PrSnapshot`] this module produces; degradation (no `gh`, no PR, …) is in-band as a
//! [`PrView`] variant, never an error the UI must handle.

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// What the `PR` tab shows: the resolved snapshot, or a degraded state with its own remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrView {
    /// Not yet fetched — the tab shows a loading placeholder until the first poll lands.
    Loading,
    /// An open (or merged/closed) PR resolved for the branch.
    Pr(Box<PrSnapshot>),
    /// The branch has no PR; push and open one.
    NoPr,
    /// Two or more open PRs back the branch; the count, so the user knows to pick on GitHub.
    Ambiguous(usize),
    /// `gh` is not on `PATH`.
    NoGh,
    /// `gh` is installed but not authenticated.
    NotAuthed,
    /// The worktree's remote is not GitHub.
    NotGitHub,
    /// Any other `gh` failure (rate limit, offline, …); the app freezes the last good view.
    Error(String),
}

/// One pull request's state, read fresh from GitHub each poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnapshot {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PrState,
    pub is_draft: bool,
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

/// Run `gh` in `repo` and return stdout, or a classified failure. `gh` resolves the repo from
/// the worktree's remote, so no owner/repo is passed for `pr`/`repo` subcommands.
fn gh(repo: &Path, args: &[&str]) -> Result<String, GhError> {
    let out = Command::new("gh").current_dir(repo).args(args).output();
    let out = match out {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GhError::NoGh),
        Err(e) => return Err(GhError::Other(e.to_string())),
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(classify_failure(&String::from_utf8_lossy(&out.stderr)))
}

/// Map a failed `gh`'s stderr to a degraded state by its wording — `gh` has no stable exit
/// codes for these. An unrecognised failure is `Other` → a transient `Error` view.
fn classify_failure(stderr: &str) -> GhError {
    let s = stderr.to_lowercase();
    if s.contains("not logged") || s.contains("authentication") || s.contains("gh auth login") {
        GhError::NotAuthed
    } else if s.contains("none of the git remotes") || s.contains("not a github") {
        GhError::NotGitHub
    } else {
        GhError::Other(stderr.trim().to_string())
    }
}

/// A classified `gh` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug, PartialEq, Eq)]
enum GhError {
    NoGh,
    NotAuthed,
    NotGitHub,
    Other(String),
}

impl From<GhError> for PrView {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NoGh => PrView::NoGh,
            GhError::NotAuthed => PrView::NotAuthed,
            GhError::NotGitHub => PrView::NotGitHub,
            GhError::Other(m) => PrView::Error(m),
        }
    }
}

/// Read the PR for the worktree's branch, or a degraded view. Never errors — degradation is
/// in-band so the `PR` tab always has something to render (`specs/forge-host.md`).
#[must_use]
pub fn fetch(repo: &Path) -> PrView {
    match fetch_inner(repo) {
        Ok(view) => view,
        Err(e) => e.into(),
    }
}

fn fetch_inner(repo: &Path) -> Result<PrView, GhError> {
    let Some((owner, name)) = crate::git::github_slug(repo) else {
        return Ok(PrView::NotGitHub);
    };
    let Some(branch) = crate::git::current_branch(repo) else {
        // A detached HEAD (e.g. after `gh pr merge --delete-branch`) has no branch to resolve a
        // PR against. Show the empty state rather than querying `headRefName:""`, which GitHub
        // treats as unfiltered and would mis-resolve to an unrelated PR.
        return Ok(PrView::NoPr);
    };
    // Resolve the branch's PR number, then read all its detail directly. Two GraphQL calls, not
    // the six `gh` calls this replaced — and `mergeable` only populates on direct access, never
    // through the list connection (`specs/forge-host.md`).
    let number = match resolve(repo, &owner, &name, &branch)? {
        Resolution::One(n) => n,
        Resolution::None => return Ok(PrView::NoPr),
        Resolution::Many(count) => return Ok(PrView::Ambiguous(count)),
    };
    let detail = pr_detail(repo, &owner, &name, number)?;
    let node = &detail["data"]["repository"]["pullRequest"];
    if node.is_null() {
        return Ok(PrView::NoPr);
    }
    let sync = derive_sync(crate::git::ahead_behind(
        repo,
        node["headRefOid"].as_str().unwrap_or_default(),
    ));
    Ok(PrView::Pr(Box::new(build_snapshot(node, sync))))
}

/// The local branch's position relative to the PR head, from `git`'s ahead/behind counts. A
/// diverged branch (both nonzero) leads with the unpushed count — the headline case. `None`
/// (the PR head isn't local yet) reads as in sync rather than guessing.
fn derive_sync(ahead_behind: Option<(u32, u32)>) -> Sync {
    match ahead_behind {
        None | Some((0, 0)) => Sync::InSync,
        Some((0, behind)) => Sync::Behind(behind),
        Some((ahead, _)) => Sync::Unpushed(ahead),
    }
}

/// The branch's open PR number, none, or several. A merged/closed PR is consulted only when no
/// open PR exists (`specs/forge-host.md`).
enum Resolution {
    One(u64),
    None,
    Many(usize),
}

/// Resolve the branch's PR number via the list connection (cheap, number-only). Prefers an open
/// PR; falls back to the latest of any state.
fn resolve(repo: &Path, owner: &str, name: &str, branch: &str) -> Result<Resolution, GhError> {
    let nums = |filter: &str| -> Result<Vec<u64>, GhError> {
        let q = format!(
            "query($o:String!,$n:String!,$b:String!){{repository(owner:$o,name:$n){{\
             pullRequests(headRefName:$b, {filter}){{nodes{{number}}}}}}}}"
        );
        let v = graphql(repo, &q, owner, name, branch)?;
        Ok(v["data"]["repository"]["pullRequests"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["number"].as_u64())
            .collect())
    };
    // first:100 so the surfaced ambiguity count is the real number of open PRs, not a cap.
    let open = nums("states:OPEN, first:100")?;
    match open.as_slice() {
        [n] => return Ok(Resolution::One(*n)),
        [_, ..] => return Ok(Resolution::Many(open.len())),
        [] => {}
    }
    match nums("first:1, orderBy:{field:CREATED_AT, direction:DESC}")?.first() {
        Some(n) => Ok(Resolution::One(*n)),
        None => Ok(Resolution::None),
    }
}

/// All of one PR's state in a single direct GraphQL call — identity, mergeability, checks,
/// reviews, plain comments, and review threads. Each list caps at 100 rows — ample for any real
/// PR in a review sidebar — and its `pageInfo` flags a fuller surface so the UI can mark it,
/// rather than paging to exhaustion (`specs/forge-host.md`). `reviews` reads `last:100` to keep
/// the newest, so its "more exist" flag is `hasPreviousPage`; the `first:` lists use `hasNextPage`.
fn pr_detail(repo: &Path, owner: &str, name: &str, number: u64) -> Result<Value, GhError> {
    let q = format!(
        "query($o:String!,$n:String!){{repository(owner:$o,name:$n){{\
         pullRequest(number:{number}){{\
         number title url isDraft state mergeable mergeStateStatus baseRefName headRefOid \
         commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:100){{pageInfo{{hasNextPage}} nodes{{__typename \
         ... on CheckRun{{name status conclusion}} ... on StatusContext{{context state}}}}}}}}}}}}}} \
         reviews(last:100){{pageInfo{{hasPreviousPage}} nodes{{author{{login}} body state submittedAt}}}} \
         comments(first:100){{pageInfo{{hasNextPage}} nodes{{author{{login}} body createdAt}}}} \
         reviewThreads(first:100){{pageInfo{{hasNextPage}} nodes{{isResolved isOutdated path line \
         comments(first:1){{totalCount nodes{{author{{login}} body createdAt diffHunk}}}}}}}}}}}}}}"
    );
    graphql(repo, &q, owner, name, number.to_string().as_str())
}

/// Run a GraphQL `query` with the `$o`/`$n`/`$b` variables and parse the response.
fn graphql(
    repo: &Path,
    query: &str,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<Value, GhError> {
    let out = gh(
        repo,
        &[
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("o={owner}"),
            "-F",
            &format!("n={name}"),
            "-F",
            &format!("b={branch}"),
        ],
    )?;
    serde_json::from_str(&out).map_err(|e| GhError::Other(e.to_string()))
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// Assemble the snapshot from the `gh pr view` JSON, the computed `sync`, and the merged comments.
fn build_snapshot(node: &Value, sync: Sync) -> PrSnapshot {
    let contexts = &node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
    let rollup = &contexts["nodes"];
    // A surface whose page reports more in the direction it pages is a prefix, not the whole set.
    // Each query asks only for its own flag — `hasNextPage` for the `first:` lists, `hasPreviousPage`
    // for `reviews` (a `last:` list) — so OR-ing both reads whichever applies; the absent one is false.
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
        state: parse_state(node["state"].as_str().unwrap_or("OPEN")),
        is_draft: node["isDraft"].as_bool().unwrap_or(false),
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
        // Latest wins: a later array entry for the same name (a re-run) replaces the earlier.
        if let Some(slot) = out.iter_mut().find(|c| c.name == name) {
            *slot = Check { name, status };
        } else {
            out.push(Check { name, status });
        }
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

    dedup_bot_prose(&mut out);
    // Newest first: ISO-8601 `…Z` strings sort lexically in chronological order.
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

fn prose_comment(
    kind: CommentKind,
    user: &Value,
    body: String,
    created_at: Option<&str>,
) -> Comment {
    let login = user["login"].as_str().unwrap_or("").to_string();
    let anchor = match kind {
        CommentKind::Review => "review",
        _ => "comment",
    };
    Comment {
        kind,
        author_is_bot: is_bot(&login),
        author: login,
        anchor: anchor.to_string(),
        body,
        snippet: None,
        created_at: created_at.unwrap_or("").to_string(),
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
            || keep_newest.get(&c.author) == Some(&c.created_at)
    });
}

/// Whether a GitHub login is an app/bot (`…[bot]`).
fn is_bot(login: &str) -> bool {
    login.ends_with("[bot]")
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
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
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

        let mut comments_more = base.clone();
        comments_more["comments"]["pageInfo"]["hasNextPage"] = serde_json::json!(true);
        assert!(build_snapshot(&comments_more, Sync::InSync).truncated);

        let mut threads_more = base.clone();
        threads_more["reviewThreads"]["pageInfo"]["hasNextPage"] = serde_json::json!(true);
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
            state: PrState::Open,
            is_draft: false,
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
        assert_eq!(derive_sync(None), Sync::InSync);
        assert_eq!(derive_sync(Some((0, 0))), Sync::InSync);
        assert_eq!(derive_sync(Some((2, 0))), Sync::Unpushed(2));
        assert_eq!(derive_sync(Some((0, 3))), Sync::Behind(3));
        assert_eq!(derive_sync(Some((2, 3))), Sync::Unpushed(2)); // diverged → unpushed leads
    }

    #[test]
    fn gh_failure_classifies_by_stderr_wording() {
        assert_eq!(classify_failure("gh auth login required"), GhError::NotAuthed);
        assert_eq!(
            classify_failure("You are not logged into any GitHub hosts"),
            GhError::NotAuthed
        );
        assert_eq!(
            classify_failure("none of the git remotes point to a GitHub host"),
            GhError::NotGitHub
        );
        assert_eq!(
            classify_failure("HTTP 500 something"),
            GhError::Other("HTTP 500 something".into())
        );
    }
}
