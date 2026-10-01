//! Read-only Gitea access: the pull request's identity, state, commit statuses, reviews, and
//! comments.
//!
//! The Gitea provider behind `src/forge.rs`. It follows the neutral resolution contract — the
//! branch's published heads filter an enumeration of the newest open and closed pull requests
//! by head repository and branch, since Gitea's listing takes no head filter — through
//! `tea api` REST calls, and fills the same normalized [`PrSnapshot`] the other providers do.
//! It never writes to Gitea.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::forge::{
    AssocPr, Association, Check, CheckStatus, Comment, CommentKind, Merge, PrFetchInput,
    PrSnapshot, PrState, PrView, Reply, Sync, finish_comments, prose_row, push_unique,
    upsert_latest,
};

/// Read Gitea for one already-derived input. Degradation stays in-band for the PR tab.
pub(crate) fn fetch(
    repo: &Path,
    input: &PrFetchInput,
    target: &crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> PrView {
    match fetch_inner(repo, input, target, cancelled) {
        Ok(view) => view,
        Err(error) => error.into_view(target.host()),
    }
}

/// A classified `tea` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug)]
enum TeaError {
    NoTea,
    /// No `tea` login serves this host, or the server refused its token (401).
    NotAuthed,
    /// The endpoint answered 403 or 404 — the addressed object is unknown or unreadable.
    Unavailable(String),
    LocalGit(String),
    Other(String),
}

impl TeaError {
    fn into_view(self, host: &str) -> PrView {
        match self {
            Self::NoTea => PrView::NoCli(crate::git::Forge::Gitea),
            Self::NotAuthed => PrView::NotAuthed(crate::git::Forge::Gitea, host.to_owned()),
            Self::LocalGit(message) => PrView::GitError(message),
            Self::Unavailable(message) | Self::Other(message) => {
                PrView::Error(crate::git::Forge::Gitea, message)
            }
        }
    }
}

/// The retryable error a panicked reader degrades into (`crate::forge::join_read`).
fn died(surface: &str) -> TeaError {
    TeaError::Other(format!("{surface} read panicked"))
}

/// The page size every paged read asks for. Gitea clamps it to `MAX_RESPONSE_ITEMS` (50 by
/// default), so a paged read takes the real size from what page 1 returned.
const PAGE_LIMIT: u64 = 50;

/// How many review-comment reads run at once. Gitea files each batch of inline comments as a
/// review of its own, so a busy pull request can carry dozens.
const REVIEW_READS_AT_ONCE: usize = 8;

/// Run one `tea` command in `repo`. Standard input is closed, so a read can never wait on a
/// prompt.
fn run_tea(
    repo: &Path,
    args: &[&str],
    cancelled: &AtomicBool,
) -> Result<crate::forge::CliOutput, TeaError> {
    let mut cmd = crate::proc::command("tea");
    cmd.current_dir(repo).args(args).stdin(Stdio::null());
    crate::forge::run_provider_output(
        &mut cmd,
        cancelled,
        TeaError::NoTea,
        classify_failure,
        TeaError::Other,
    )
}

/// Map a failed `tea`'s stderr to a degraded state by its wording. `tea api` exits zero on
/// every HTTP answer, so a non-zero exit is a local failure — a missing login, an unreachable
/// server — never a status.
fn classify_failure(stderr: &str) -> TeaError {
    let s = stderr.to_lowercase();
    if (s.contains("login") && s.contains("does not exist")) || s.contains("tea login add") {
        TeaError::NotAuthed
    } else {
        TeaError::Other(stderr.trim().to_string())
    }
}

/// The `tea` login that serves `host`: the one whose URL names that host, else the one whose
/// SSH host does. Every read names it with `--login`, so neither tea's own remote detection
/// nor its default login can send a fetch — or its token — to another instance.
fn login_for(repo: &Path, host: &str, cancelled: &AtomicBool) -> Result<String, TeaError> {
    let out = run_tea(repo, &["logins", "list", "--output", "json"], cancelled)?;
    let logins: Value = serde_json::from_str(out.stdout.trim())
        .map_err(|error| TeaError::Other(error.to_string()))?;
    matching_login(&logins, host).ok_or(TeaError::NotAuthed)
}

fn matching_login(logins: &Value, host: &str) -> Option<String> {
    let rows = logins.as_array()?;
    let is_host = |value: Option<&str>| value.is_some_and(|have| have.eq_ignore_ascii_case(host));
    let row = rows
        .iter()
        .find(|row| is_host(row["url"].as_str().and_then(url_host)))
        .or_else(|| rows.iter().find(|row| is_host(row["ssh_host"].as_str().map(bare_host))))?;
    row["name"].as_str().filter(|name| !name.is_empty()).map(str::to_string)
}

/// The host of a login URL: the scheme, credentials, port, and path dropped.
fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = bare_host(authority.rsplit_once('@').map_or(authority, |(_, host)| host));
    (!host.is_empty()).then_some(host)
}

/// A `host[:port]` reduced to its host.
fn bare_host(authority: &str) -> &str {
    authority.split(':').next().unwrap_or_default()
}

/// The `tea` argument list for one API read pinned to `login`. The `=` form keeps a login
/// name that starts with `-` a value. `--include` writes the status line and headers to
/// stderr, which is where the read's verdict and a listing's total live.
fn api_args(login: &str, endpoint: &str) -> Vec<String> {
    vec![
        "api".to_string(),
        format!("--login={login}"),
        "--include".to_string(),
        endpoint.to_owned(),
    ]
}

/// One successful API read: the body, and the `X-Total-Count` a paged listing carries.
#[derive(Debug)]
struct Response {
    total: Option<u64>,
    body: Value,
}

/// One fetch's reader: the repository `tea` runs in and the login every read is pinned to.
struct Api<'a> {
    repo: &'a Path,
    login: &'a str,
    cancelled: &'a AtomicBool,
}

impl Api<'_> {
    fn get(&self, endpoint: &str) -> Result<Response, TeaError> {
        let args = api_args(self.login, endpoint);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_tea(self.repo, &args, self.cancelled)?;
        read_response(&out.stdout, &out.stderr)
    }

    /// Several reads at once, results in call order. Wall clock is the slowest single read.
    fn get_all(&self, endpoints: &[String]) -> Vec<Result<Response, TeaError>> {
        std::thread::scope(|scope| {
            let handles: Vec<_> =
                endpoints.iter().map(|endpoint| scope.spawn(move || self.get(endpoint))).collect();
            handles
                .into_iter()
                .map(|handle| crate::forge::join_read(handle, || died("api")))
                .collect()
        })
    }
}

/// Read one `tea api --include` answer. `tea` exits zero whatever the server said, so the
/// verdict is the status line `--include` writes to stderr: 2xx is the body, 401 a refused
/// token, 403 or 404 an unknown or unreadable object, and anything else a retryable failure
/// that carries the server's message.
fn read_response(body: &str, headers: &str) -> Result<Response, TeaError> {
    let Some(status) = status_code(headers) else {
        return Err(TeaError::Other("tea api reported no HTTP status".to_string()));
    };
    let failure = || {
        let message = serde_json::from_str::<Value>(body.trim())
            .ok()
            .and_then(|v| v["message"].as_str().map(|m| m.trim().trim_end_matches('.').to_string()))
            .filter(|m| !m.is_empty());
        match message {
            Some(message) => format!("HTTP {status}: {message}"),
            None => format!("HTTP {status}"),
        }
    };
    match status {
        200..=299 => {
            let body = serde_json::from_str(body.trim())
                .map_err(|error| TeaError::Other(error.to_string()))?;
            let total = header(headers, "x-total-count").and_then(|value| value.parse().ok());
            Ok(Response { total, body })
        }
        401 => Err(TeaError::NotAuthed),
        403 | 404 => Err(TeaError::Unavailable(failure())),
        _ => Err(TeaError::Other(failure())),
    }
}

/// The code on the response's `HTTP/…` status line.
fn status_code(headers: &str) -> Option<u16> {
    headers.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("HTTP/")?;
        rest.split_whitespace().nth(1)?.parse().ok()
    })
}

/// One header's value, by case-insensitive name.
fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A listing body as its rows.
fn rows_of(body: Value) -> Vec<Value> {
    match body {
        Value::Array(rows) => rows,
        _ => Vec::new(),
    }
}

/// A combined-status body as its per-context rows.
fn statuses_of(body: Value) -> Vec<Value> {
    match body {
        Value::Object(mut object) => rows_of(object.remove("statuses").unwrap_or_default()),
        _ => Vec::new(),
    }
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    target: &crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> Result<PrView, TeaError> {
    if input.local.heads.is_empty() {
        return Ok(PrView::NoPr);
    }
    let login = login_for(repo, target.host(), cancelled)?;
    let api = Api { repo, login: &login, cancelled };
    let Some((pr, home)) = associate_by_branch(&api, input, target)? else {
        return Ok(PrView::NoPr);
    };
    let Some(number) = pr["number"].as_u64() else {
        return Ok(PrView::NoPr);
    };
    // Sync compares the fetch's pinned HEAD to the PR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's PR with another branch's count.
    let pr_head = pr["head"]["sha"].as_str().unwrap_or_default();
    let sync = crate::forge::local_sync(repo, input.local.head_oid.as_deref(), pr_head)
        .map_err(|error| TeaError::LocalGit(error.0))?;

    // The surfaces are independent reads, run concurrently so the fetch's wall clock is the
    // slowest one. Inline comments hang off their reviews, so that pair runs in sequence.
    let base = format!("repos/{}/{}", home.owner(), home.name());
    let base = base.as_str();
    let api = &api;
    let (comments, reviews, checks) = std::thread::scope(|scope| {
        let comments = scope.spawn(|| api.get(&format!("{base}/issues/{number}/comments")));
        let reviews = scope.spawn(|| {
            let (reviews, capped) =
                newest_rows(api, &format!("{base}/pulls/{number}/reviews"), rows_of)?;
            let inline = review_comments(api, base, number, &reviews)?;
            Ok((reviews, capped, inline))
        });
        let checks = scope.spawn(|| fetch_checks(api, base, pr_head));
        (
            crate::forge::join_read(comments, || died("comments")),
            crate::forge::join_read(reviews, || died("reviews")),
            crate::forge::join_read(checks, || died("checks")),
        )
    });
    let (prose, prose_capped) = newest_prose(rows_of(comments?.body));
    let (reviews, reviews_capped, inline) = reviews?;
    let (checks, checks_capped) = checks?;
    let (threads, threads_capped) = newest_conversations(&inline);

    let comments = merge_comments(&prose, &reviews, &threads);
    Ok(PrView::Pr(Box::new(build_snapshot(
        &pr,
        sync,
        checks,
        comments,
        prose_capped || reviews_capped || threads_capped,
        checks_capped,
    ))))
}

/// Ask Gitea for the branch's pull request: the target's pick, else — on a fork clone — the
/// fork's own. Returns the pull request and the repository it lives in.
fn associate_by_branch<'a>(
    api: &Api<'_>,
    input: &'a PrFetchInput,
    target: &'a crate::git::RepoTarget,
) -> Result<Option<(Value, &'a crate::git::RepoTarget)>, TeaError> {
    if let Some(pr) = pick_in(api, input, target, true)? {
        return Ok(Some((pr, target)));
    }
    let Some(fork) = crate::forge::fork_repository(input.origin_repository.as_ref(), target) else {
        return Ok(None);
    };
    Ok(pick_in(api, input, fork, false)?.map(|pr| (pr, fork)))
}

/// The branch's pull request in `queried`, as the listing returned it — a listed pull request
/// is the complete one, so the pick needs no detail read.
fn pick_in(
    api: &Api<'_>,
    input: &PrFetchInput,
    queried: &crate::git::RepoTarget,
    required: bool,
) -> Result<Option<Value>, TeaError> {
    let mut assoc = enumerate(api, queried, &input.local.heads, required)?;
    let Some(number) =
        crate::forge::resolve_pick(api.repo, &assoc, input.local.head_oid.as_deref())
            .map_err(|error| TeaError::LocalGit(error.0))?
    else {
        return Ok(None);
    };
    Ok([&mut assoc.open, &mut assoc.history]
        .into_iter()
        .flat_map(|bucket| bucket.iter_mut())
        .find(|pr| pr.number == number)
        .and_then(|pr| pr.raw.take()))
}

/// The newest 100 open and newest 100 closed pull requests of `queried`, two pages of each
/// in one concurrent wave, admitted against the branch's heads. Gitea lists newest-created
/// first and takes no head filter, so admission is client-side, as on Azure DevOps. An
/// unreadable listing fails the fetch on the target; on the fork it proves nothing.
fn enumerate(
    api: &Api<'_>,
    queried: &crate::git::RepoTarget,
    heads: &[crate::git::Head],
    required: bool,
) -> Result<Association, TeaError> {
    let base = format!("repos/{}/{}/pulls", queried.owner(), queried.name());
    let endpoints: Vec<String> = ["open", "closed"]
        .into_iter()
        .flat_map(|state| {
            let base = &base;
            (1..=2).map(move |page| format!("{base}?state={state}&limit={PAGE_LIMIT}&page={page}"))
        })
        .collect();
    let mut assoc = Association::default();
    for result in api.get_all(&endpoints) {
        let response = match result {
            Ok(response) => response,
            Err(TeaError::Unavailable(_)) if !required => continue,
            Err(error) => return Err(error),
        };
        for node in rows_of(response.body) {
            let open = node["state"].as_str() == Some("open");
            let Some(pr) = listed_pr(node, queried, heads) else { continue };
            if open {
                push_unique(&mut assoc.open, pr);
            } else {
                push_unique(&mut assoc.history, pr);
            }
        }
    }
    Ok(assoc)
}

/// A listed pull request's pick fields, carrying the node itself, when its head (repository,
/// branch) is one of the branch's heads (`forge::admits`).
fn listed_pr(
    node: Value,
    queried: &crate::git::RepoTarget,
    heads: &[crate::git::Head],
) -> Option<AssocPr> {
    let admitted = {
        let head_ref = head_branch(&node);
        let fork = head_repository(&node, queried.host());
        let head_repo = if same_repository(&node) {
            crate::forge::HeadRepo::Queried
        } else {
            crate::forge::HeadRepo::Other(fork.iter().collect())
        };
        crate::forge::admits(heads, queried, head_ref, &head_repo)
    };
    if !admitted {
        return None;
    }
    let mut pr = assoc_pr(&node)?;
    pr.raw = Some(node);
    Some(pr)
}

/// Whether the head branch lives in the base repository, by repository id — which survives
/// renames and transfers.
fn same_repository(pr: &Value) -> bool {
    match (pr["head"]["repo_id"].as_u64(), pr["base"]["repo_id"].as_u64()) {
        (Some(head), Some(base)) => head != 0 && head == base,
        _ => false,
    }
}

/// The head branch's name. Gitea turns `head.ref` into `refs/pull/N/head` once the branch is
/// deleted, as it usually is after a merge; `head.label` keeps the name.
fn head_branch(pr: &Value) -> &str {
    pr["head"]["label"].as_str().unwrap_or_default()
}

/// The repository a pull request's head names, as a target on the queried host. A deleted
/// fork nulls it, so its head names no repository.
fn head_repository(pr: &Value, host: &str) -> Option<crate::git::RepoTarget> {
    let (owner, name) = pr["head"]["repo"]["full_name"].as_str()?.split_once('/')?;
    crate::git::RepoTarget::with_path(crate::git::Forge::Gitea, host, &[owner, name])
}

/// One listing node reduced to the pick-relevant fields shared with the other providers.
fn assoc_pr(node: &Value) -> Option<AssocPr> {
    let closed_at = node["merged_at"].as_str().or(node["closed_at"].as_str()).unwrap_or_default();
    Some(AssocPr {
        number: node["number"].as_u64()?,
        head_oid: node["head"]["sha"].as_str().unwrap_or_default().to_string(),
        head_ref: head_branch(node).to_string(),
        created_at: utc(node["created_at"].as_str().unwrap_or_default()),
        closed_at: utc(closed_at),
        raw: None,
    })
}

/// The newest `SURFACE_CAP` rows of an oldest-first paged listing, oldest first, and whether
/// older rows went unread. Page 1 names the total (`X-Total-Count`) and the server's page
/// size, which place the newest rows; their pages then read in one concurrent wave. A row
/// that shifts across a page boundary between reads counts once, by its `id`.
fn newest_rows(
    api: &Api<'_>,
    endpoint: &str,
    rows_of: fn(Value) -> Vec<Value>,
) -> Result<(Vec<Value>, bool), TeaError> {
    let page = |n: u64| format!("{endpoint}?limit={PAGE_LIMIT}&page={n}");
    let first = api.get(&page(1))?;
    let mut rows = rows_of(first.body);
    let per_page = rows.len() as u64;
    let total = first.total.unwrap_or(per_page).max(per_page);
    let tail = tail_pages(total, per_page);
    let unread_middle = tail.first().is_some_and(|&first| first > 2);
    let endpoints: Vec<String> = tail.into_iter().map(page).collect();
    for result in api.get_all(&endpoints) {
        for row in rows_of(result?.body) {
            if row["id"].is_null() || !rows.iter().any(|have| have["id"] == row["id"]) {
                rows.push(row);
            }
        }
    }
    let truncated = unread_middle || rows.len() > crate::forge::SURFACE_CAP;
    Ok((crate::forge::newest_capped(rows), truncated))
}

/// The pages past page 1 that hold the newest `SURFACE_CAP` of `total` rows at `per_page` a
/// page: one page more than the cap spans, since the last page can be short.
fn tail_pages(total: u64, per_page: u64) -> Vec<u64> {
    if per_page == 0 {
        return Vec::new();
    }
    let pages = total.div_ceil(per_page);
    let span = (crate::forge::SURFACE_CAP as u64).div_ceil(per_page) + 1;
    (pages.saturating_sub(span) + 1..=pages).filter(|&page| page >= 2).collect()
}

/// Every inline comment on the submitted reviews that carry any, read review by review in
/// bounded waves. Gitea files a reply under the review it answers, so one review's rows can
/// span several authors.
fn review_comments(
    api: &Api<'_>,
    base: &str,
    number: u64,
    reviews: &[Value],
) -> Result<Vec<Value>, TeaError> {
    let endpoints = review_comment_endpoints(base, number, reviews);
    let mut out = Vec::new();
    for chunk in endpoints.chunks(REVIEW_READS_AT_ONCE) {
        for result in api.get_all(chunk) {
            match result {
                Ok(response) => out.extend(rows_of(response.body)),
                // A review deleted between the two reads proves nothing.
                Err(TeaError::Unavailable(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(out)
}

/// The inline-comment read of every submitted review that carries any. A pending review is its
/// author's unsubmitted draft.
fn review_comment_endpoints(base: &str, number: u64, reviews: &[Value]) -> Vec<String> {
    reviews
        .iter()
        .filter(|review| review["state"].as_str() != Some("PENDING"))
        .filter(|review| review["comments_count"].as_u64().unwrap_or(0) > 0)
        .filter_map(|review| review["id"].as_u64())
        .map(|id| format!("{base}/pulls/{number}/reviews/{id}/comments"))
        .collect()
}

/// The head commit's statuses as the checks list. Gitea Actions and external CI both report
/// through them, and the pull request page reads them from the pull request's own repository.
/// No head is no checks.
fn fetch_checks(api: &Api<'_>, base: &str, head: &str) -> Result<(Vec<Check>, bool), TeaError> {
    if head.is_empty() || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok((Vec::new(), false));
    }
    match newest_rows(api, &format!("{base}/commits/{head}/status"), statuses_of) {
        Ok((rows, capped)) => Ok((build_checks(&rows), capped)),
        // A head the server no longer resolves has no statuses to show.
        Err(TeaError::Unavailable(_)) => Ok((Vec::new(), false)),
        Err(error) => Err(error),
    }
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// Assemble the snapshot from the listed pull request, its checks, and its merged comments.
fn build_snapshot(
    pr: &Value,
    sync: Sync,
    checks: Vec<Check>,
    comments: Vec<Comment>,
    comments_truncated: bool,
    checks_truncated: bool,
) -> PrSnapshot {
    PrSnapshot {
        number: pr["number"].as_u64().unwrap_or_default(),
        title: pr["title"].as_str().unwrap_or_default().to_string(),
        url: pr["html_url"].as_str().unwrap_or_default().to_string(),
        body: pr["body"].as_str().unwrap_or_default().to_string(),
        state: parse_state(pr),
        // Gitea derives `draft` from its work-in-progress title prefixes (`WIP:`, `[WIP]`).
        is_draft: pr["draft"].as_bool().unwrap_or(false),
        head_ref: head_branch(pr).to_string(),
        head_is_fork: is_cross_repository(pr),
        head_oid: pr["head"]["sha"].as_str().unwrap_or_default().to_string(),
        base_ref: pr["base"]["ref"].as_str().unwrap_or_default().to_string(),
        merge: derive_merge(pr),
        sync,
        checks,
        comments,
        comments_truncated,
        checks_truncated,
    }
}

/// `open` is open; a closed pull request is merged when Gitea says so. A missing state must
/// not read as reviewable, so it falls to closed — stale, never wrong.
fn parse_state(pr: &Value) -> PrState {
    match (pr["state"].as_str(), pr["merged"].as_bool()) {
        (Some("open"), _) => PrState::Open,
        (_, Some(true)) => PrState::Merged,
        _ => PrState::Closed,
    }
}

/// A head in another repository — a fork, or a deleted one — sets the fork marker.
fn is_cross_repository(pr: &Value) -> bool {
    match (pr["head"]["repo_id"].as_u64(), pr["base"]["repo_id"].as_u64()) {
        (Some(head), Some(base)) => head != base,
        _ => false,
    }
}

/// Fold Gitea's one merge signal, `mergeable`, to the blocker it can show: an open, ready pull
/// request the server cannot merge has conflicts. A draft is never mergeable, so its `false`
/// says nothing. Gitea also answers `false` while its conflict check runs after a push, which
/// the next poll clears. The API carries no branch-protection verdict, so `blocked` never shows.
fn derive_merge(pr: &Value) -> Merge {
    let ready = pr["state"].as_str() == Some("open") && !pr["draft"].as_bool().unwrap_or(false);
    if ready && pr["mergeable"].as_bool() == Some(false) {
        Merge::Conflicting
    } else {
        Merge::Clean
    }
}

/// The checks list, one row per status context.
fn build_checks(rows: &[Value]) -> Vec<Check> {
    let mut checks: Vec<Check> = Vec::new();
    for row in rows {
        let name = row["context"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let status = commit_status(row["status"].as_str().unwrap_or_default());
        upsert_latest(&mut checks, Check { name: name.to_string(), status });
    }
    checks
}

/// Normalise one Gitea commit status to a [`CheckStatus`]. Gitea Actions reports a queued and a
/// running job alike as `pending`. A `warning` says the work did not fail, and has no
/// counterpart on the other forges, so it reads as neutral — like a skipped check.
fn commit_status(state: &str) -> CheckStatus {
    match state {
        "success" => CheckStatus::Success,
        "failure" | "error" => CheckStatus::Failure,
        "warning" | "skipped" => CheckStatus::Skipped,
        _ => CheckStatus::Pending,
    }
}

/// A comment that renders: one carrying a body.
fn has_body(comment: &Value) -> bool {
    !comment["body"].as_str().unwrap_or("").trim().is_empty()
}

/// The newest 100 plain comments that render, oldest first, and whether any were dropped.
/// Gitea returns a pull request's comments in one unpaged list.
fn newest_prose(rows: Vec<Value>) -> (Vec<Value>, bool) {
    let rows: Vec<Value> = rows.into_iter().filter(has_body).collect();
    let truncated = rows.len() > crate::forge::SURFACE_CAP;
    (crate::forge::newest_capped(rows), truncated)
}

/// Where an inline comment sits: path, side, and line. Gitea reports a new-side line as
/// `position` and an old-side one as `original_position`, zero when absent. Its own
/// conversation view groups a thread by exactly this key.
fn comment_place(comment: &Value) -> (&str, Option<crate::model::Side>, Option<u64>) {
    let path = comment["path"].as_str().unwrap_or_default();
    let line = |key: &str| comment[key].as_u64().filter(|&line| line > 0);
    match (line("position"), line("original_position")) {
        (Some(new), _) => (path, crate::forge::finding_side(true, false), Some(new)),
        (None, Some(old)) => (path, crate::forge::finding_side(false, true), Some(old)),
        (None, None) => (path, None, None),
    }
}

/// Group inline comments into conversations by place, oldest first. Each conversation keeps
/// posting order — the root, then its replies — and one with nothing to render drops before
/// the newest-100 cap, so an emptied thread never spends a slot. Returns the conversations and
/// whether any were dropped.
fn newest_conversations(comments: &[Value]) -> (Vec<Vec<&Value>>, bool) {
    let mut sorted: Vec<&Value> = comments.iter().collect();
    sorted.sort_by(|a, b| {
        utc(a["created_at"].as_str().unwrap_or_default())
            .cmp(&utc(b["created_at"].as_str().unwrap_or_default()))
            .then(a["id"].as_u64().cmp(&b["id"].as_u64()))
    });
    let mut threads: Vec<(_, Vec<&Value>)> = Vec::new();
    for comment in sorted {
        let place = comment_place(comment);
        match threads.iter_mut().find(|(have, _)| *have == place) {
            Some((_, thread)) => thread.push(comment),
            None => threads.push((place, vec![comment])),
        }
    }
    let threads: Vec<Vec<&Value>> = threads
        .into_iter()
        .map(|(_, thread)| thread)
        .filter(|thread| thread.iter().any(|comment| has_body(comment)))
        .collect();
    let truncated = threads.len() > crate::forge::SURFACE_CAP;
    (crate::forge::newest_capped(threads), truncated)
}

/// Merge the plain comments, reviews, and inline conversations into one newest-first list:
/// a plain comment is a `comment` row, a submitted review a `review` row, and a conversation
/// a `finding` row carrying its replies.
fn merge_comments(prose: &[Value], reviews: &[Value], threads: &[Vec<&Value>]) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();
    for comment in prose {
        let author = login_of(comment);
        let bot = is_gitea_bot(&author);
        let body = comment["body"].as_str().unwrap_or("").trim().to_string();
        let created_at = utc(comment["created_at"].as_str().unwrap_or_default());
        out.push(prose_row(CommentKind::Comment, author, bot, body, created_at));
    }
    out.extend(reviews.iter().filter_map(review_row));
    out.extend(threads.iter().filter_map(|thread| finding(thread)));
    finish_comments(&mut out);
    out
}

/// A review as its PR-level `review` row: its own words, or for a bare verdict the verdict
/// itself. A review that only files inline comments, a pending draft, a review request, and a
/// dismissed bare verdict render nothing.
fn review_row(review: &Value) -> Option<Comment> {
    let body = review["body"].as_str().unwrap_or("").trim();
    let dismissed = review["dismissed"].as_bool().unwrap_or(false);
    let text = match review["state"].as_str()? {
        "PENDING" | "REQUEST_REVIEW" => return None,
        _ if !body.is_empty() => body.to_string(),
        _ if dismissed => return None,
        "APPROVED" => "Approved this pull request.".to_string(),
        "REQUEST_CHANGES" => "Requested changes to this pull request.".to_string(),
        _ => return None,
    };
    let author = login_of(review);
    let bot = is_gitea_bot(&author);
    let submitted_at = utc(review["submitted_at"].as_str().unwrap_or_default());
    Some(prose_row(CommentKind::Review, author, bot, text, submitted_at))
}

/// One conversation as its `finding` row: the first comment with a body is the root, every
/// later one a reply. Gitea records a resolution on the conversation's comment it was made
/// from, so any resolver resolves the thread.
fn finding(thread: &[&Value]) -> Option<Comment> {
    let root_i = thread.iter().position(|comment| has_body(comment))?;
    let root = thread[root_i];
    let (path, side, line) = comment_place(root);
    let place = crate::forge::FindingPlace::from_lines(path, line, line, side);
    let author = login_of(root);
    Some(Comment {
        kind: CommentKind::Finding,
        author_is_bot: is_gitea_bot(&author),
        author,
        anchor: place.anchor(),
        place: Some(place),
        body: root["body"].as_str().unwrap_or("").trim().to_string(),
        snippet: root["diff_hunk"].as_str().filter(|hunk| !hunk.is_empty()).map(str::to_string),
        created_at: utc(root["created_at"].as_str().unwrap_or_default()),
        is_resolved: thread.iter().any(|comment| comment["resolver"].is_object()),
        // Gitea keeps whether a comment's line has since changed off its API.
        is_outdated: false,
        replies: thread[root_i + 1..]
            .iter()
            .filter(|comment| has_body(comment))
            .map(|comment| {
                let author = login_of(comment);
                Reply {
                    author_is_bot: is_gitea_bot(&author),
                    author,
                    body: comment["body"].as_str().unwrap_or("").trim().to_string(),
                    created_at: utc(comment["created_at"].as_str().unwrap_or_default()),
                }
            })
            .collect(),
    })
}

fn login_of(row: &Value) -> String {
    row["user"]["login"].as_str().unwrap_or_default().to_string()
}

/// Whether a Gitea login is a service account: the shared name heuristics, or the
/// `gitea-actions` account Gitea Actions posts as. Gitea's user payload carries no bot flag.
fn is_gitea_bot(login: &str) -> bool {
    crate::forge::is_named_bot(login) || login.eq_ignore_ascii_case("gitea-actions")
}

/// A Gitea timestamp in the `…Z` UTC form every surface sorts and ages by. Gitea writes times
/// in the server's configured zone (`…+02:00`), where a lexical sort would misorder them and
/// an age would be off by the offset. A value that does not parse passes through unchanged.
fn utc(ts: &str) -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(ts, &Rfc3339)
        .ok()
        .and_then(|t| t.to_offset(time::UtcOffset::UTC).format(&Rfc3339).ok())
        .unwrap_or_else(|| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pr_node() -> Value {
        json!({
            "number": 1475,
            "title": "Add search",
            "html_url": "https://code.corp.example/acme/widgets/pulls/1475",
            "body": "Adds the search screen.",
            "state": "open",
            "draft": false,
            "mergeable": true,
            "merged": false,
            "created_at": "2026-09-26T22:54:09Z",
            "closed_at": null,
            "merged_at": null,
            "head": {"ref": "feature/search", "label": "feature/search", "sha": "cf97ccd6",
                     "repo_id": 209, "repo": {"full_name": "acme/widgets"}},
            "base": {"ref": "main", "repo_id": 209, "repo": {"full_name": "acme/widgets"}},
        })
    }

    fn gt(owner: &str, name: &str) -> crate::git::RepoTarget {
        crate::git::RepoTarget::with_path(
            crate::git::Forge::Gitea,
            "code.corp.example",
            &[owner, name],
        )
        .unwrap()
    }

    fn head(repo: &crate::git::RepoTarget, name: &str) -> crate::git::Head {
        crate::git::Head { repo: repo.clone(), name: name.to_string() }
    }

    #[test]
    fn snapshot_maps_the_pull_request_fields() {
        let s = build_snapshot(&pr_node(), Sync::InSync, Vec::new(), Vec::new(), false, false);
        assert_eq!(s.number, 1475);
        assert_eq!(s.title, "Add search");
        assert_eq!(s.url, "https://code.corp.example/acme/widgets/pulls/1475");
        assert_eq!(s.body, "Adds the search screen.");
        assert_eq!(s.state, PrState::Open);
        assert!(!s.is_draft);
        assert_eq!(s.head_ref, "feature/search");
        assert_eq!(s.head_oid, "cf97ccd6");
        assert!(!s.head_is_fork);
        assert_eq!(s.base_ref, "main");
        assert_eq!(s.merge, Merge::Clean);
        assert!(!s.comments_truncated && !s.checks_truncated);
    }

    #[test]
    fn state_follows_the_merged_flag_and_a_missing_state_reads_as_closed() {
        let mut pr = pr_node();
        assert_eq!(parse_state(&pr), PrState::Open);
        pr["state"] = json!("closed");
        assert_eq!(parse_state(&pr), PrState::Closed);
        pr["merged"] = json!(true);
        assert_eq!(parse_state(&pr), PrState::Merged);
        assert_eq!(parse_state(&json!({})), PrState::Closed);
    }

    #[test]
    fn an_unmergeable_ready_pull_request_is_conflicting_and_a_draft_is_not() {
        let mut pr = pr_node();
        assert_eq!(derive_merge(&pr), Merge::Clean);
        pr["mergeable"] = json!(false);
        assert_eq!(derive_merge(&pr), Merge::Conflicting);
        // A draft is never mergeable, so its `false` proves no conflict.
        pr["draft"] = json!(true);
        assert_eq!(derive_merge(&pr), Merge::Clean);
        pr["draft"] = json!(false);
        pr["state"] = json!("closed");
        assert_eq!(derive_merge(&pr), Merge::Clean, "a finished pull request has no blocker");
    }

    #[test]
    fn a_head_in_another_repository_sets_the_fork_marker() {
        let mut pr = pr_node();
        pr["head"]["repo_id"] = json!(311);
        assert!(is_cross_repository(&pr));
        // A deleted fork keeps its id at zero.
        pr["head"]["repo_id"] = json!(0);
        pr["head"]["repo"] = Value::Null;
        assert!(is_cross_repository(&pr));
    }

    #[test]
    fn a_listed_pull_request_joins_only_when_its_head_is_one_of_the_branchs_heads() {
        let upstream = gt("acme", "widgets");
        let fork = gt("contributor", "widgets");
        let node = |branch: &str, repo_id: u64, full_name: Option<&str>| {
            json!({
                "number": 7, "state": "open", "created_at": "2026-09-01T00:00:00Z",
                "head": {"ref": "refs/pull/7/head", "label": branch, "sha": "abc",
                         "repo_id": repo_id,
                         "repo": full_name.map(|name| json!({"full_name": name}))},
                "base": {"ref": "main", "repo_id": 209},
            })
        };
        let on_main = [head(&upstream, "main")];
        let fork_fix = [head(&fork, "fix")];
        let cases: &[(&str, &[crate::git::Head], Value, bool)] = &[
            ("own same-repository branch", &on_main, node("main", 209, Some("acme/widgets")), true),
            (
                "a stranger's fork main",
                &on_main,
                node("main", 400, Some("stranger/widgets")),
                false,
            ),
            (
                "fork clone, its own fix",
                &fork_fix,
                node("fix", 311, Some("contributor/widgets")),
                true,
            ),
            (
                "fork clone, upstream's own fix",
                &fork_fix,
                node("fix", 209, Some("acme/widgets")),
                false,
            ),
            (
                "the fork's name in another case",
                &fork_fix,
                node("fix", 311, Some("Contributor/Widgets")),
                true,
            ),
            ("a deleted fork", &fork_fix, node("fix", 0, None), false),
            ("another branch", &on_main, node("other", 209, Some("acme/widgets")), false),
        ];
        for (label, heads, node, expected) in cases {
            let pr = listed_pr(node.clone(), &upstream, heads);
            assert_eq!(pr.is_some(), *expected, "{label}");
        }
        // An admitted pick carries the complete node, so it needs no detail read.
        let pr = listed_pr(node("main", 209, Some("acme/widgets")), &upstream, &on_main).unwrap();
        assert_eq!(pr.raw.unwrap()["number"], 7);
    }

    #[test]
    fn a_listing_node_reduces_to_the_shared_pick_fields() {
        let node = json!({
            "number": 12, "created_at": "2026-09-20T10:00:00+02:00",
            "merged_at": "2026-09-21T10:00:00+02:00", "closed_at": "2026-09-21T10:00:00+02:00",
            "head": {"ref": "refs/pull/12/head", "label": "feature", "sha": "abc"},
        });
        let pr = assoc_pr(&node).unwrap();
        assert_eq!((pr.number, pr.head_oid.as_str(), pr.head_ref.as_str()), (12, "abc", "feature"));
        assert_eq!(pr.created_at, "2026-09-20T08:00:00Z", "the pick compares UTC times");
        assert_eq!(pr.closed_at, "2026-09-21T08:00:00Z");
        assert!(assoc_pr(&json!({"head": {}})).is_none(), "no number, no pick");
    }

    #[test]
    fn a_read_takes_its_verdict_from_the_status_line_not_the_exit_code() {
        let headers = "HTTP/2.0 200 OK\nX-Total-Count: 10\nContent-Type: application/json\n";
        let ok = read_response("[{\"id\":1}]\n", headers).unwrap();
        assert_eq!(ok.total, Some(10));
        assert_eq!(ok.body, json!([{"id": 1}]));
        let missing = "{\"errors\":null,\"message\":\"The target couldn't be found.\"}";
        assert!(matches!(
            read_response(missing, "HTTP/2.0 404 Not Found\n"),
            Err(TeaError::Unavailable(m)) if m == "HTTP 404: The target couldn't be found"
        ));
        assert!(matches!(
            read_response("{}", "HTTP/1.1 403 Forbidden\n"),
            Err(TeaError::Unavailable(_))
        ));
        let refused = "{\"message\":\"user does not exist [uid: 0, name: ]\"}";
        assert!(matches!(
            read_response(refused, "HTTP/2.0 401 Unauthorized\n"),
            Err(TeaError::NotAuthed)
        ));
        assert!(matches!(
            read_response("", "HTTP/2.0 502 Bad Gateway\n"),
            Err(TeaError::Other(m)) if m == "HTTP 502"
        ));
        // Without a status line nothing proves the read succeeded.
        assert!(matches!(read_response("[]", ""), Err(TeaError::Other(_))));
    }

    #[test]
    fn a_missing_login_is_not_authed_and_a_transport_failure_is_retryable() {
        assert!(matches!(
            classify_failure("Error: login name 'work' does not exist"),
            TeaError::NotAuthed
        ));
        assert!(matches!(
            classify_failure("dial tcp: lookup code.corp.example: no such host"),
            TeaError::Other(_)
        ));
    }

    #[test]
    fn the_login_is_the_one_serving_the_remote_host() {
        let logins = json!([
            {"name": "home", "url": "https://gitea.example.net", "ssh_host": "gitea.example.net"},
            {"name": "work", "url": "https://Code.Corp.Example:3000/",
             "ssh_host": "git.corp.example"},
        ]);
        assert_eq!(matching_login(&logins, "code.corp.example").as_deref(), Some("work"));
        // A remote on the SSH host names the same login.
        assert_eq!(matching_login(&logins, "git.corp.example").as_deref(), Some("work"));
        assert_eq!(matching_login(&logins, "gitea.example.net").as_deref(), Some("home"));
        assert_eq!(matching_login(&logins, "other.example"), None, "no default-login fallback");
        assert_eq!(matching_login(&json!([]), "code.corp.example"), None);
    }

    #[test]
    fn every_read_pins_the_login() {
        assert_eq!(
            api_args("-work", "repos/acme/widgets/pulls/1"),
            ["api", "--login=-work", "--include", "repos/acme/widgets/pulls/1"]
        );
    }

    #[test]
    fn the_tail_pages_hold_the_newest_hundred_rows() {
        assert_eq!(tail_pages(40, 40), Vec::<u64>::new(), "one page holds everything");
        assert_eq!(tail_pages(50, 50), Vec::<u64>::new());
        assert_eq!(tail_pages(101, 50), vec![2, 3]);
        assert_eq!(tail_pages(150, 50), vec![2, 3]);
        // Page 5 holds 30 rows, so three pages back are needed for a hundred.
        assert_eq!(tail_pages(230, 50), vec![3, 4, 5]);
        // A server clamping pages to 30 rows.
        assert_eq!(tail_pages(400, 30), vec![10, 11, 12, 13, 14]);
        assert_eq!(tail_pages(0, 0), Vec::<u64>::new());
    }

    #[test]
    fn statuses_map_to_checks_with_warnings_neutral() {
        assert_eq!(commit_status("success"), CheckStatus::Success);
        assert_eq!(commit_status("failure"), CheckStatus::Failure);
        assert_eq!(commit_status("error"), CheckStatus::Failure);
        assert_eq!(commit_status("pending"), CheckStatus::Pending);
        assert_eq!(commit_status("warning"), CheckStatus::Skipped);
        assert_eq!(commit_status("skipped"), CheckStatus::Skipped);
        let rows = statuses_of(json!({"state": "failure", "statuses": [
            {"context": "CI / build (push)", "status": "success"},
            {"context": "CI / e2e (push)", "status": "failure"},
            {"context": "", "status": "failure"},
        ]}));
        let checks = build_checks(&rows);
        assert_eq!(
            checks.iter().map(|c| (c.name.as_str(), c.status)).collect::<Vec<_>>(),
            [
                ("CI / build (push)", CheckStatus::Success),
                ("CI / e2e (push)", CheckStatus::Failure)
            ]
        );
        assert!(statuses_of(json!(null)).is_empty());
    }

    fn inline(id: u64, user: &str, at: &str, path: &str, new: u64, old: u64, body: &str) -> Value {
        json!({"id": id, "user": {"login": user}, "created_at": at, "path": path,
               "position": new, "original_position": old, "body": body,
               "diff_hunk": "@@ -1,0 +1,2 @@\n+a\n+b", "resolver": null})
    }

    #[test]
    fn inline_comments_group_into_conversations_by_place() {
        let comments = [
            // Filed under two reviews; the reply lands under the root's review in Gitea, but
            // either way the place is what groups them.
            inline(2, "author", "2026-09-22T18:53:06Z", "src/a.rs", 115, 0, "Yup, surprised too."),
            inline(1, "reviewer", "2026-09-22T16:19:07Z", "src/a.rs", 115, 0, "Is this raw?"),
            inline(3, "reviewer", "2026-09-22T17:00:00Z", "src/a.rs", 0, 115, "On the old side."),
            inline(4, "reviewer", "2026-09-22T17:30:00Z", "src/b.rs", 9, 0, "   "),
        ];
        let (threads, truncated) = newest_conversations(&comments);
        assert!(!truncated);
        assert_eq!(threads.len(), 2, "the old side is its own thread; an empty one drops");
        let merged = merge_comments(&[], &[], &threads);
        let new_side = merged.iter().find(|c| c.body == "Is this raw?").unwrap();
        assert_eq!(new_side.kind, CommentKind::Finding);
        assert_eq!(new_side.anchor, "src/a.rs:115");
        assert_eq!(new_side.place.as_ref().unwrap().side, Some(crate::model::Side::New));
        assert_eq!(new_side.replies.len(), 1);
        assert_eq!(new_side.replies[0].author, "author");
        assert_eq!(new_side.snippet.as_deref(), Some("@@ -1,0 +1,2 @@\n+a\n+b"));
        assert!(!new_side.is_resolved);
        let old_side = merged.iter().find(|c| c.body == "On the old side.").unwrap();
        assert_eq!(old_side.place.as_ref().unwrap().side, Some(crate::model::Side::Old));
        assert!(old_side.replies.is_empty());
    }

    #[test]
    fn a_resolver_on_any_comment_resolves_the_conversation() {
        let mut root = inline(1, "reviewer", "2026-09-22T16:00:00Z", "src/a.rs", 3, 0, "Fix?");
        root["resolver"] = json!({"login": "author"});
        let reply = inline(2, "author", "2026-09-22T17:00:00Z", "src/a.rs", 3, 0, "Done.");
        let comments = [root, reply];
        let (threads, _) = newest_conversations(&comments);
        let merged = merge_comments(&[], &[], &threads);
        assert!(merged[0].is_resolved);
    }

    #[test]
    fn conversations_keep_the_newest_hundred() {
        let comments: Vec<Value> = (0..120)
            .map(|i| {
                let at = format!("2026-09-22T10:{:02}:{:02}Z", i / 60, i % 60);
                inline(i, "reviewer", &at, "src/a.rs", i + 1, 0, "note")
            })
            .collect();
        let (threads, truncated) = newest_conversations(&comments);
        assert!(truncated);
        assert_eq!(threads.len(), 100);
        assert_eq!(threads[0][0]["id"], 20, "the oldest twenty drop");
    }

    #[test]
    fn inline_comments_are_read_only_from_submitted_reviews_that_carry_any() {
        let reviews = [
            json!({"id": 1, "state": "COMMENT", "comments_count": 2}),
            json!({"id": 2, "state": "PENDING", "comments_count": 3}),
            json!({"id": 3, "state": "APPROVED", "comments_count": 0}),
        ];
        assert_eq!(
            review_comment_endpoints("repos/acme/widgets", 7, &reviews),
            ["repos/acme/widgets/pulls/7/reviews/1/comments"],
            "a pending review's drafts are never read"
        );
    }

    #[test]
    fn reviews_render_their_words_or_their_verdict() {
        let review = |state: &str, body: &str, dismissed: bool| {
            json!({"state": state, "body": body, "dismissed": dismissed,
                   "user": {"login": "reviewer"}, "submitted_at": "2026-09-27T03:14:28Z"})
        };
        let row = |state, body, dismissed| review_row(&review(state, body, dismissed));
        assert_eq!(row("APPROVED", "", false).unwrap().body, "Approved this pull request.");
        assert_eq!(
            row("REQUEST_CHANGES", "", false).unwrap().body,
            "Requested changes to this pull request."
        );
        assert_eq!(row("APPROVED", "Ship it.", false).unwrap().body, "Ship it.");
        assert_eq!(row("COMMENT", "Overall fine.", false).unwrap().body, "Overall fine.");
        let approved = row("APPROVED", "", false).unwrap();
        assert_eq!(approved.kind, CommentKind::Review);
        assert_eq!(approved.created_at, "2026-09-27T03:14:28Z");
        // Inline-only reviews, drafts, requests, and dismissed bare verdicts render nothing.
        assert!(row("COMMENT", "", false).is_none());
        assert!(row("PENDING", "Draft words.", false).is_none());
        assert!(row("REQUEST_REVIEW", "", false).is_none());
        assert!(row("APPROVED", "", true).is_none());
        assert_eq!(row("APPROVED", "Kept words.", true).unwrap().body, "Kept words.");
    }

    #[test]
    fn plain_comments_render_newest_first_beside_reviews() {
        let prose = [
            json!({"user": {"login": "alice"}, "body": "First.",
                   "created_at": "2026-09-27T03:14:48Z"}),
            json!({"user": {"login": "bob"}, "body": "Second.",
                   "created_at": "2026-09-28T14:02:59Z"}),
        ];
        let reviews = [json!({"state": "APPROVED", "body": "", "user": {"login": "carol"},
                              "submitted_at": "2026-09-28T14:03:09Z"})];
        let merged = merge_comments(&prose, &reviews, &[]);
        assert_eq!(
            merged.iter().map(|c| (c.kind, c.author.as_str())).collect::<Vec<_>>(),
            [
                (CommentKind::Review, "carol"),
                (CommentKind::Comment, "bob"),
                (CommentKind::Comment, "alice")
            ]
        );
        let (kept, truncated) = newest_prose(vec![json!({"body": " "}), prose[0].clone()]);
        assert_eq!(kept.len(), 1, "an empty comment never spends a slot");
        assert!(!truncated);
    }

    #[test]
    fn gitea_actions_and_named_bots_count_as_bots() {
        assert!(is_gitea_bot("gitea-actions"));
        assert!(is_gitea_bot("renovate[bot]"));
        assert!(is_gitea_bot("deploy-bot"));
        assert!(!is_gitea_bot("talbot"));
        assert!(!is_gitea_bot("alice"));
    }

    #[test]
    fn timestamps_normalise_to_utc() {
        assert_eq!(utc("2026-09-28T14:03:09Z"), "2026-09-28T14:03:09Z");
        assert_eq!(utc("2026-09-28T14:03:09+02:00"), "2026-09-28T12:03:09Z");
        assert_eq!(utc("2026-01-01T01:00:00+02:00"), "2025-12-31T23:00:00Z", "across a year");
        assert_eq!(utc("2024-02-29T22:30:00-05:30"), "2024-03-01T04:00:00Z", "across a leap day");
        assert_eq!(utc(""), "");
        assert_eq!(utc("not a time at all"), "not a time at all");
        assert_eq!(utc("2026-09-28T14:03:09+0200"), "2026-09-28T14:03:09+0200", "unparsed stays");
    }
}
