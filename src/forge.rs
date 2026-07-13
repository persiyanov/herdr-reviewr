//! Read-only GitHub access: the pull request's identity, state, checks, and comments.
//!
//! See `specs/forge-host.md`. A fetch first derives [`PrFetchInput`] from local Git and one
//! validated config snapshot, then reads its canonical target through explicitly hosted `gh`
//! GraphQL calls. It never posts, resolves, re-runs, merges, or otherwise writes to GitHub. The
//! `PR` tab renders the [`PrSnapshot`] this module produces; degradation is in-band as [`PrView`].

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
    /// An open (or merged/closed) PR resolved for one of the worktree's candidate branches.
    Pr(Box<PrSnapshot>),
    /// No candidate branch has a PR; the queried candidate names, so the empty state can
    /// say what was looked for. Empty on a detached `HEAD` (nothing was queried).
    NoPr(Vec<String>),
    /// Two or more open PRs back the winning candidate branch and not exactly one matches
    /// the pinned `HEAD`; the count, so the user knows to pick on GitHub.
    Ambiguous(usize),
    /// `gh` is not on `PATH`.
    NoGh,
    /// `gh` is installed but not authenticated for this canonical host.
    NotAuthed(String),
    /// Origin is missing or has no hosted Git URL.
    NeedsGitHubOrigin,
    /// Origin names a hosted forge outside the supported GitHub hosts.
    UnsupportedHost(String),
    /// Origin names a supported host but not an owner/repository path.
    MalformedOrigin(String),
    /// Any other `gh` failure (rate limit, offline, …); the app freezes the last good view.
    Error(String),
}

impl PrView {
    /// A same-input failure that can be retried without discarding the visible snapshot.
    /// Both snapshot preservation and the empty-state renderer consume this projection so a
    /// newly added retryable failure cannot diverge between those surfaces. `refresh` is the
    /// active `refresh` binding's hint key, so the advertised retry key follows a rebind.
    pub fn retry_remedy(&self, refresh: char) -> Option<String> {
        match self {
            Self::NoGh => Some(format!("gh not found — install `gh`, then press {refresh}")),
            Self::NotAuthed(host) => Some(format!(
                "not signed in — run `gh auth login --hostname {host}`, then press {refresh}"
            )),
            Self::Error(message) => {
                Some(format!("GitHub unavailable — {message}; press {refresh} to retry now"))
            }
            _ => None,
        }
    }
}

/// One pull request's state, read fresh from GitHub each poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnapshot {
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

/// Run explicitly targeted `gh` arguments in `repo` and return stdout or a classified failure.
fn gh(repo: &Path, host: &str, args: &[&str], cancelled: &AtomicBool) -> Result<String, GhError> {
    let child = Command::new("gh")
        .current_dir(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GhError::NoGh),
        Err(e) => return Err(GhError::Other(e.to_string())),
    };

    // Drain both pipes while polling so a large GraphQL response cannot fill a pipe and block
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
                return Err(GhError::Other(error.to_string()));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if cancelled.load(Ordering::Acquire) {
        return Err(GhError::Other("request cancelled".to_string()));
    }
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    Err(classify_failure(&String::from_utf8_lossy(&stderr), host))
}

/// Map a failed `gh`'s stderr to a degraded state by its wording — `gh` has no stable exit
/// codes for these. An unrecognised failure is `Other` → a transient `Error` view.
fn classify_failure(stderr: &str, host: &str) -> GhError {
    let s = stderr.to_lowercase();
    if s.contains("not logged") || s.contains("authentication") || s.contains("gh auth login") {
        GhError::NotAuthed(host.to_owned())
    } else {
        GhError::Other(stderr.trim().to_string())
    }
}

/// A classified `gh` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug, PartialEq, Eq)]
enum GhError {
    NoGh,
    NotAuthed(String),
    Other(String),
}

impl From<GhError> for PrView {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NoGh => PrView::NoGh,
            GhError::NotAuthed(host) => PrView::NotAuthed(host),
            GhError::Other(m) => PrView::Error(m),
        }
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

/// Derive one complete fetch input without contacting GitHub.
pub fn fetch_input(
    repo: &Path,
    base: Option<&str>,
    config: &crate::config::PluginConfig,
) -> Result<PrFetchInput, String> {
    let local = crate::git::pr_local(repo, base, config.base_branches(), config.github_host())
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
    let target = match &input.origin {
        crate::git::OriginIdentity::Repository(target) => target,
        crate::git::OriginIdentity::Missing | crate::git::OriginIdentity::Hostless => {
            return Ok(PrView::NeedsGitHubOrigin);
        }
        crate::git::OriginIdentity::Unsupported(host) => {
            return Ok(PrView::UnsupportedHost(host.clone()));
        }
        crate::git::OriginIdentity::Malformed(host) => {
            return Ok(PrView::MalformedOrigin(host.clone()));
        }
    };
    if input.candidates.is_empty() {
        // A detached HEAD (e.g. after `gh pr merge --delete-branch`) has no branch identity
        // to publish, so nothing was derived. Show the empty state rather than querying
        // `headRefName:""`, which GitHub treats as unfiltered and would mis-resolve to an
        // unrelated PR.
        return Ok(PrView::NoPr(Vec::new()));
    }
    let target = FetchTarget {
        repo,
        host: &target.host,
        owner: &target.owner,
        name: &target.name,
        cancelled,
    };
    // Resolve the open PR across all candidates in one aliased call, then read its detail
    // directly — `mergeable` only populates on direct access, never through the list
    // connection (`specs/forge-host.md`).
    let open = resolve_candidates(&target, &input.candidates, OPEN, "headRefOid")?;
    let number = match select_open(&open, input.head_oid.as_deref()) {
        Pick::One(n) => n,
        Pick::Ambiguous(count) => return Ok(PrView::Ambiguous(count)),
        Pick::None => {
            // No open PR anywhere: fall back to the newest-created merged/closed PR.
            let hist = resolve_candidates(&target, &input.candidates, HISTORICAL, "createdAt")?;
            match select_historical(&hist) {
                Some(n) => n,
                None => return Ok(PrView::NoPr(input.candidates.clone())),
            }
        }
    };
    let detail = pr_detail(&target, number)?;
    let node = &detail["data"]["repository"]["pullRequest"];
    if node.is_null() {
        return Ok(PrView::NoPr(input.candidates.clone()));
    }
    // Sync compares the fetch's pinned HEAD to the PR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's PR with another branch's count.
    let pr_head = node["headRefOid"].as_str().unwrap_or_default();
    let sync = match input.head_oid.as_deref() {
        Some(pin) if !pr_head.is_empty() => derive_sync(
            crate::git::ahead_behind_oids(repo, pin, pr_head).map_err(|e| GhError::Other(e.0))?,
        ),
        _ => Sync::Unknown,
    };
    Ok(PrView::Pr(Box::new(build_snapshot(node, sync))))
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

/// The list filter for the open-PR resolve call. `first:100` keeps the surfaced ambiguity
/// count the real number of open PRs, not a cap.
const OPEN: &str = "states:OPEN, first:100";
/// The list filter for the historical fallback: the newest-created merged/closed PR per name.
const HISTORICAL: &str =
    "states:[MERGED,CLOSED], first:1, orderBy:{field:CREATED_AT, direction:DESC}";

struct FetchTarget<'a> {
    repo: &'a Path,
    host: &'a str,
    owner: &'a str,
    name: &'a str,
    cancelled: &'a AtomicBool,
}

/// The PRs for every candidate name in one aliased GraphQL call — alias `c{i}` ↔ candidate
/// `i`, names passed as variables (never interpolated into the query text). Each returned
/// entry is `(number, extra)` where `extra` is `headRefOid` (open) or `createdAt` (historical).
fn resolve_candidates(
    target: &FetchTarget<'_>,
    candidates: &[String],
    filter: &str,
    extra: &str,
) -> Result<Vec<Vec<(u64, String)>>, GhError> {
    let query = build_resolve_query(candidates.len(), filter, extra);
    let mut vars: Vec<(String, String)> = vec![
        ("o".to_string(), target.owner.to_string()),
        ("n".to_string(), target.name.to_string()),
    ];
    for (i, cand) in candidates.iter().enumerate() {
        vars.push((format!("b{i}"), cand.clone()));
    }
    let v = graphql(target.repo, target.host, &query, &vars, target.cancelled)?;
    Ok(parse_resolve(&v, candidates.len(), extra))
}

/// The winner among the candidates' open PRs (`specs/forge-host.md` "Resolution").
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    One(u64),
    Ambiguous(usize),
    None,
}

/// Pick the open PR: the earliest candidate in derivation order holding any wins — the
/// recorded upstream outranks an inferred branch, which outranks the bare local name. On
/// one name backing several open PRs, exactly one head at the pinned `HEAD` wins; else the
/// ambiguity count is surfaced rather than a silent guess.
fn select_open(per_candidate: &[Vec<(u64, String)>], pinned_head: Option<&str>) -> Pick {
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
fn select_historical(per_candidate: &[Vec<(u64, String)>]) -> Option<u64> {
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

/// All of one PR's state in a single direct GraphQL call — identity, mergeability, checks,
/// reviews, plain comments, and review threads. Each list caps at 100 rows — ample for any real
/// PR in a review sidebar — and its `pageInfo` flags a fuller surface so the UI can mark it,
/// rather than paging to exhaustion (`specs/forge-host.md`). `reviews` reads `last:100` to keep
/// the newest, so its "more exist" flag is `hasPreviousPage`; the `first:` lists use `hasNextPage`.
fn pr_detail(target: &FetchTarget<'_>, number: u64) -> Result<Value, GhError> {
    let q = format!(
        "query($o:String!,$n:String!){{repository(owner:$o,name:$n){{\
         pullRequest(number:{number}){{\
         number title url body isDraft state mergeable mergeStateStatus baseRefName headRefName \
         headRefOid isCrossRepository \
         commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:100){{pageInfo{{hasNextPage}} nodes{{__typename \
         ... on CheckRun{{name status conclusion}} ... on StatusContext{{context state}}}}}}}}}}}}}} \
         reviews(last:100){{pageInfo{{hasPreviousPage}} nodes{{author{{login}} body state submittedAt}}}} \
         comments(first:100){{pageInfo{{hasNextPage}} nodes{{author{{login}} body createdAt}}}} \
         reviewThreads(first:100){{pageInfo{{hasNextPage}} nodes{{isResolved isOutdated path line \
         comments(first:1){{totalCount nodes{{author{{login}} body createdAt diffHunk}}}}}}}}}}}}}}"
    );
    let vars =
        [("o".to_string(), target.owner.to_string()), ("n".to_string(), target.name.to_string())];
    graphql(target.repo, target.host, &q, &vars, target.cancelled)
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

/// The aliased resolve query for `n` candidates: `c{i}: pullRequests(headRefName:$b{i}, …)`.
/// Branch names ride as `$b{i}` variables, never in the query text.
fn build_resolve_query(n: usize, filter: &str, extra: &str) -> String {
    use std::fmt::Write;
    let mut q = String::from("query($o:String!,$n:String!");
    for i in 0..n {
        let _ = write!(q, ",$b{i}:String!");
    }
    q.push_str("){repository(owner:$o,name:$n){");
    for i in 0..n {
        let _ =
            write!(q, "c{i}:pullRequests(headRefName:$b{i}, {filter}){{nodes{{number {extra}}}}} ");
    }
    q.push_str("}}");
    q
}

/// Per-candidate `(number, extra)` lists from the aliased response, index `i` ↔ alias
/// `c{i}`. A missing or null alias parses as an empty list.
fn parse_resolve(v: &Value, n: usize, extra: &str) -> Vec<Vec<(u64, String)>> {
    (0..n)
        .map(|i| {
            v["data"]["repository"][format!("c{i}").as_str()]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| {
                    Some((p["number"].as_u64()?, p[extra].as_str().unwrap_or_default().to_string()))
                })
                .collect()
        })
        .collect()
}

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
        body: node["body"].as_str().unwrap_or_default().to_string(),
        state: parse_state(node["state"].as_str().unwrap_or("OPEN")),
        is_draft: node["isDraft"].as_bool().unwrap_or(false),
        head_ref: node["headRefName"].as_str().unwrap_or_default().to_string(),
        head_is_fork: node["isCrossRepository"].as_bool().unwrap_or(false),
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
    fn resolve_query_aliases_candidates_and_never_inlines_names() {
        let q = build_resolve_query(2, OPEN, "headRefOid");
        assert_eq!(
            q,
            "query($o:String!,$n:String!,$b0:String!,$b1:String!)\
             {repository(owner:$o,name:$n){\
             c0:pullRequests(headRefName:$b0, states:OPEN, first:100){nodes{number headRefOid}} \
             c1:pullRequests(headRefName:$b1, states:OPEN, first:100){nodes{number headRefOid}} }}"
        );
        let h = build_resolve_query(1, HISTORICAL, "createdAt");
        assert!(h.contains(
            "states:[MERGED,CLOSED], first:1, orderBy:{field:CREATED_AT, direction:DESC}"
        ));
        assert!(h.contains("nodes{number createdAt}"));
    }

    #[test]
    fn parse_resolve_maps_aliases_in_order_and_null_to_empty() {
        let v = serde_json::json!({"data": {"repository": {
            "c0": {"nodes": [{"number": 7, "headRefOid": "abc"}]},
            "c1": null,
            "c2": {"nodes": [{"number": 9, "headRefOid": "def"}, {"number": 10, "headRefOid": "ghi"}]}
        }}});
        let per = parse_resolve(&v, 3, "headRefOid");
        assert_eq!(per[0], [(7, "abc".to_string())]);
        assert!(per[1].is_empty());
        assert_eq!(per[2], [(9, "def".to_string()), (10, "ghi".to_string())]);
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
