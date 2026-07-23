//! Read-only GitLab access: the merge request's identity, state, pipelines, and discussions.
//!
//! The GitLab provider behind `src/forge.rs` (`specs/forge-providers.md`). It follows the
//! neutral resolution contract in `specs/forge-host.md` — the branch's forge names list
//! merge requests by `source_branch` — through `glab api` REST calls, and fills the
//! same normalized [`PrSnapshot`] the GitHub provider does. It never writes to GitLab.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::forge::{
    AssocPr, Association, Check, CheckStatus, Comment, CommentKind, Merge, PrFetchInput,
    PrSnapshot, PrState, PrView, Sync, finish_comments, prose_row, push_unique, upsert_latest,
};

/// Read GitLab for one already-derived input. Degradation stays in-band for the PR tab.
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

/// A classified `glab` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug)]
enum GlabError {
    NoGlab,
    NotAuthed,
    /// The endpoint answered 404 or 403 — the addressed object is unknown or unreadable.
    /// GitLab answers 404 for private objects, so the two are one state.
    Unavailable(String),
    LocalGit(String),
    Other(String),
}

impl GlabError {
    fn into_view(self, host: &str) -> PrView {
        match self {
            Self::NoGlab => PrView::NoCli(crate::git::Forge::GitLab),
            Self::NotAuthed => PrView::NotAuthed(crate::git::Forge::GitLab, host.to_owned()),
            Self::LocalGit(message) => PrView::GitError(message),
            Self::Unavailable(message) | Self::Other(message) => {
                PrView::Error(crate::git::Forge::GitLab, message)
            }
        }
    }
}

/// The retryable error a panicked reader degrades into (`crate::forge::join_read`).
fn died(surface: &str) -> GlabError {
    GlabError::Other(format!("{surface} read panicked"))
}

/// Fold an unreadable optional surface to an empty payload: the fetch stands on what it
/// has instead of failing the whole view (`specs/forge-providers.md`).
fn optional_surface(result: Result<Value, GlabError>) -> Result<Value, GlabError> {
    match result {
        Err(GlabError::Unavailable(_)) => Ok(Value::Null),
        other => other,
    }
}

/// The `glab` argument list for one explicitly hosted API read. `--hostname` pins the
/// instance, so an inherited `GITLAB_HOST` override can never redirect the fetch
/// (`specs/forge-host.md`). `--include` keeps the response headers, which carry the pagination
/// totals; a read that ignores them simply drops them.
fn glab_args(host: &str, endpoint: &str) -> Vec<String> {
    vec![
        "api".to_string(),
        "--hostname".to_string(),
        host.to_owned(),
        endpoint.to_owned(),
        "--include".to_string(),
    ]
}

/// Run one `glab api` read against `host` and return its raw stdout.
fn glab_raw(
    repo: &Path,
    host: &str,
    endpoint: &str,
    cancelled: &AtomicBool,
) -> Result<String, GlabError> {
    let mut cmd = Command::new("glab");
    cmd.current_dir(repo).args(glab_args(host, endpoint));
    crate::forge::run_provider(
        &mut cmd,
        cancelled,
        GlabError::NoGlab,
        classify_failure,
        GlabError::Other,
    )
}

/// Run one `glab api` read and parse the JSON response.
fn glab_api(
    repo: &Path,
    host: &str,
    endpoint: &str,
    cancelled: &AtomicBool,
) -> Result<Value, GlabError> {
    glab_api_paged(repo, host, endpoint, cancelled).map(|(_, value)| value)
}

/// Run several `glab api` reads concurrently, returning their results in call order. Wall-clock
/// is the slowest single read. Callers stay bounded: the branch names are capped upstream.
fn glab_api_fan_out(
    repo: &Path,
    host: &str,
    endpoints: &[String],
    cancelled: &AtomicBool,
) -> Vec<Result<Value, GlabError>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = endpoints
            .iter()
            .map(|endpoint| scope.spawn(move || glab_api(repo, host, endpoint, cancelled)))
            .collect();
        handles.into_iter().map(|handle| crate::forge::join_read(handle, || died("api"))).collect()
    })
}

/// Run one `glab api -i` read and parse the `x-total-pages` header plus the JSON body.
/// The body is the text after the last blank line — GitLab returns compact one-line JSON.
fn glab_api_paged(
    repo: &Path,
    host: &str,
    endpoint: &str,
    cancelled: &AtomicBool,
) -> Result<(Option<u64>, Value), GlabError> {
    let out = glab_raw(repo, host, endpoint, cancelled)?;
    let (total_pages, body) = split_headers(&out);
    let value = serde_json::from_str(body).map_err(|e| GlabError::Other(e.to_string()))?;
    Ok((total_pages, value))
}

/// Split a `--include` response into the `x-total-pages` header value and the JSON body.
/// The body is the text after the last blank line — GitLab returns compact one-line JSON,
/// and raw newlines cannot appear inside a JSON string.
fn split_headers(out: &str) -> (Option<u64>, &str) {
    // Header lines end with CRLF; the blank separator line is then `\r\n\r\n` or `\n\n`.
    let at = out.rfind("\r\n\r\n").or_else(|| out.rfind("\n\n"));
    let (headers, body) = match at {
        Some(at) => out.split_at(at),
        None => ("", out),
    };
    let total_pages = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("x-total-pages").then(|| value.trim().parse().ok())?
    });
    (total_pages, body.trim())
}

/// Map a failed `glab`'s stderr to a degraded state by its wording — like `gh`, `glab` has
/// no stable exit codes for these. An unrecognised failure is `Other` → a transient `Error`.
fn classify_failure(stderr: &str) -> GlabError {
    let s = stderr.to_lowercase();
    if crate::forge::reports_status(&s, 401)
        || s.contains("unauthorized")
        || s.contains("authentication")
        || s.contains("glab auth login")
        || s.contains("no token")
        // GitLab answers an under-scoped token with 403 `insufficient_scope`; only a
        // re-login with the right scopes unblocks it.
        || s.contains("insufficient_scope")
    {
        GlabError::NotAuthed
    } else if crate::forge::reports_status(&s, 403) || crate::forge::reports_status(&s, 404) {
        GlabError::Unavailable(stderr.trim().to_string())
    } else {
        GlabError::Other(stderr.trim().to_string())
    }
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    target: &crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> Result<PrView, GlabError> {
    let host = target.host();
    let target_path = crate::forge::urlencode(&target.full_path());

    let head = input.local.head_oid.as_deref();
    // A fork clone: `origin` is the fork, the target is upstream. Both projects are
    // asked, and upstream's pick outranks the fork's own (`specs/forge-host.md`).
    let fork_path = crate::forge::fork_repository(input.origin_repository.as_ref(), target)
        .map(|origin| crate::forge::urlencode(&origin.full_path()));
    let Some((iid, project_path)) = associate_by_branch(
        repo,
        host,
        &target_path,
        fork_path.as_deref(),
        &input.local.names,
        head,
        cancelled,
    )?
    else {
        return Ok(PrView::NoPr);
    };

    let mr =
        glab_api(repo, host, &format!("projects/{project_path}/merge_requests/{iid}"), cancelled)?;
    if mr["iid"].as_u64().is_none() {
        return Ok(PrView::NoPr);
    }
    // Sync compares the fetch's pinned HEAD to the MR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's MR with another branch's count.
    let mr_head = mr["sha"].as_str().unwrap_or_default();
    let sync = crate::forge::local_sync(repo, input.local.head_oid.as_deref(), mr_head)
        .map_err(|error| GlabError::LocalGit(error.0))?;

    // The three detail surfaces are independent reads; they run concurrently so the
    // fetch's wall clock is the slowest call, not the sum.
    let target_path = project_path.as_str();
    let (discussions, approvals, checks) = std::thread::scope(|scope| {
        let discussions =
            scope.spawn(|| newest_discussions(repo, host, target_path, iid, cancelled));
        let approvals = scope.spawn(|| {
            // An unavailable approvals surface contributes no review rows instead of
            // failing the whole view (`specs/forge-providers.md`).
            optional_surface(glab_api(
                repo,
                host,
                &format!("projects/{target_path}/merge_requests/{iid}/approvals"),
                cancelled,
            ))
        });
        let checks = scope.spawn(|| fetch_checks(repo, host, target_path, &mr, cancelled));
        (
            crate::forge::join_read(discussions, || died("discussions")),
            crate::forge::join_read(approvals, || died("approvals")),
            crate::forge::join_read(checks, || died("checks")),
        )
    });
    let (rows, discussions_capped) = discussions?;
    let approvals = approvals?;
    let (checks, jobs_capped) = checks?;

    Ok(PrView::Pr(Box::new(build_snapshot(
        &mr,
        sync,
        checks,
        &rows,
        &approvals,
        discussions_capped || jobs_capped,
    ))))
}

/// The newest comment discussions. GitLab returns discussions oldest-first with no sort
/// control, so the newest rows live on the last pages: read `x-total-pages`, then fetch the
/// final two pages. Each page keeps only its comment discussions before the cap, so a stream
/// of system events never spends the surface's 100 slots (`specs/forge-host.md`).
fn newest_discussions(
    repo: &Path,
    host: &str,
    target_path: &str,
    iid: u64,
    cancelled: &AtomicBool,
) -> Result<(Vec<Value>, bool), GlabError> {
    let base = format!("projects/{target_path}/merge_requests/{iid}/discussions?per_page=100");
    let (total_pages, first) = glab_api_paged(repo, host, &format!("{base}&page=1"), cancelled)?;
    let raw_first = first.as_array().map_or(0, Vec::len);
    let page1 = comment_discussions(first);
    // GitLab omits `x-total-pages` past ~10k rows. With no total the newest pages are
    // unreachable, so the oldest page stands in — a capped prefix, never presented as complete.
    if total_pages.is_none() && raw_first >= crate::forge::SURFACE_CAP {
        return Ok((page1, true));
    }
    let total = total_pages.unwrap_or(1).max(1);
    // The endpoint returns oldest-first with no sort control, so the newest rows live on the
    // last pages. Read them concurrently (`specs/forge-host.md`: each surface reads its newest
    // 100 rows).
    let endpoints: Vec<String> = discussion_tail_pages(total)
        .into_iter()
        .map(|page| format!("{base}&page={page}"))
        .collect();
    let mut later: Vec<Value> = Vec::new();
    for result in glab_api_fan_out(repo, host, &endpoints, cancelled) {
        later.extend(comment_discussions(result?));
    }
    Ok(assemble_discussions(page1, total, later))
}

/// The page numbers beyond page 1 to fetch — the last two pages, which hold the newest rows.
/// A single-page thread needs none.
fn discussion_tail_pages(total: u64) -> Vec<u64> {
    [total.saturating_sub(1), total].into_iter().filter(|page| *page >= 2).collect()
}

/// Move a discussions response into its comment discussions, dropping system-only and empty
/// threads so the cap counts what actually renders (`merge_comments`).
fn comment_discussions(response: Value) -> Vec<Value> {
    match response {
        Value::Array(rows) => rows.into_iter().filter(|d| comment_root(d).is_some()).collect(),
        _ => Vec::new(),
    }
}

/// Keep the newest 100 comment discussions from the fetched pages, and report whether any were
/// dropped. Rows arrive oldest-first. The fetched set is page 1 plus the last two pages, so it is
/// contiguous through `total == 3`; beyond that an unread middle separates page 1, and the fetch
/// reports itself truncated.
fn assemble_discussions(page1: Vec<Value>, total: u64, later: Vec<Value>) -> (Vec<Value>, bool) {
    // Oldest-first throughout: page 1 leads the fetched run, the tail pages follow. Page 1 is
    // already in hand, and a page of system events can filter down to nothing, so keeping it
    // spends the surface's slots on real comments instead of leaving them empty. Rows carry
    // their own timestamps and render newest-first, so an unread middle shows as a gap, never
    // as the wrong order.
    let mut pool = page1;
    pool.extend(later);
    // Pages between the first and the tail go unread past three, and a pool over the cap is
    // itself a prefix.
    let truncated = total > 3 || pool.len() > crate::forge::SURFACE_CAP;
    (crate::forge::newest_capped(pool), truncated)
}

/// Ask GitLab for the branch's merge requests: one `source_branch` listing per name
/// against the target project — and the fork project on a fork clone — with the project
/// lookups that prove each MR's source, all in one concurrent wave
/// (`specs/forge-providers.md`). Returns the picked MR and the project path it lives in;
/// the target's pick outranks the fork's.
fn associate_by_branch(
    repo: &Path,
    host: &str,
    target_path: &str,
    fork_path: Option<&str>,
    names: &[String],
    head: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<Option<(u64, String)>, GlabError> {
    let mut endpoints: Vec<String> = vec![format!("projects/{target_path}")];
    if let Some(fork) = fork_path {
        endpoints.push(format!("projects/{fork}"));
    }
    endpoints.extend(branch_listings(target_path, names));
    if let Some(fork) = fork_path {
        endpoints.extend(branch_listings(fork, names));
    }
    let mut responses = glab_api_fan_out(repo, host, &endpoints, cancelled).into_iter();
    // The target project's numeric id — the source filter every listed MR must match
    // (`specs/forge-host.md`: a stranger's same-named fork branch never attaches).
    let project = responses.next().transpose()?.unwrap_or(Value::Null);
    let Some(target_id) = project["id"].as_u64() else {
        return Err(GlabError::Other("project lookup returned no id".to_string()));
    };
    let fork_id = match fork_path {
        // An unreadable fork project proves nothing and never fails the fetch: the
        // upstream lookup still runs, it just cannot admit anything fork-sourced.
        Some(_) => match responses.next() {
            Some(Ok(project)) => project["id"].as_u64(),
            Some(Err(GlabError::Unavailable(_))) | None => None,
            Some(Err(error)) => return Err(error),
        },
        None => None,
    };
    let target_rows: Vec<_> = responses.by_ref().take(2 * names.len()).collect();
    // On a fork clone the upstream lookup keeps only fork-sourced MRs — upstream's own
    // same-named branch is a stranger's (`specs/forge-providers.md` GitLab).
    let assoc = collect_assoc(target_rows, |source| match fork_path {
        Some(_) => fork_id.is_some() && source == fork_id,
        None => source == Some(target_id),
    })?;
    let pick = crate::forge::resolve_pick(repo, &assoc, head)
        .map_err(|error| GlabError::LocalGit(error.0))?;
    if let Some(iid) = pick {
        return Ok(Some((iid, target_path.to_string())));
    }
    if let Some(fork) = fork_path {
        let assoc =
            collect_assoc(responses.collect(), |source| fork_id.is_some() && source == fork_id)?;
        let pick = crate::forge::resolve_pick(repo, &assoc, head)
            .map_err(|error| GlabError::LocalGit(error.0))?;
        if let Some(iid) = pick {
            return Ok(Some((iid, fork.to_string())));
        }
    }
    Ok(None)
}

/// The per-name merge-request listings against `project`: an opened page apart from the
/// created-ordered all-state page, both newest-created-first and capped at 20. The opened
/// page keeps the shared resolver's open-before-history precedence whole when a reused
/// branch name's history runs past the cap (`specs/forge-providers.md` GitLab).
fn branch_listings(project: &str, names: &[String]) -> Vec<String> {
    names
        .iter()
        .flat_map(|name| {
            let listing = |state: &str| {
                format!(
                    "projects/{project}/merge_requests?source_branch={}{state}&per_page=20\
                     &order_by=created_at&sort=desc",
                    crate::forge::urlencode(name)
                )
            };
            [listing("&state=opened"), listing("")]
        })
        .collect()
}

/// Fold one project's listings into an association, keeping only MRs whose source project
/// `allowed` admits. A 404 listing proves nothing and never fails the fetch
/// (`specs/forge-providers.md`).
fn collect_assoc(
    rows: Vec<Result<Value, GlabError>>,
    allowed: impl Fn(Option<u64>) -> bool,
) -> Result<Association, GlabError> {
    let mut assoc = Association::default();
    for result in rows {
        let v = match result {
            Ok(v) => v,
            Err(GlabError::Unavailable(_)) => continue,
            Err(error) => return Err(error),
        };
        for node in v.as_array().into_iter().flatten() {
            if !allowed(node["source_project_id"].as_u64()) {
                continue;
            }
            let Some(mr) = assoc_mr(node) else { continue };
            match node["state"].as_str().unwrap_or_default() {
                "opened" => push_unique(&mut assoc.open, mr),
                _ => push_unique(&mut assoc.history, mr),
            }
        }
    }
    Ok(assoc)
}

/// One listing node reduced to the pick-relevant fields shared with the other providers.
fn assoc_mr(node: &Value) -> Option<AssocPr> {
    let closed_at = node["merged_at"].as_str().or(node["closed_at"].as_str()).unwrap_or_default();
    Some(AssocPr {
        number: node["iid"].as_u64()?,
        head_oid: node["sha"].as_str().unwrap_or_default().to_string(),
        head_ref: node["source_branch"].as_str().unwrap_or_default().to_string(),
        created_at: node["created_at"].as_str().unwrap_or_default().to_string(),
        closed_at: closed_at.to_string(),
        // A listing node is a reduced row, never the full merge request.
        raw: None,
    })
}

/// The head pipeline's jobs as the checks list, one row per job — no pipeline is an empty
/// list (`specs/forge-providers.md`). Returns the rows and whether the job page was capped.
fn fetch_checks(
    repo: &Path,
    host: &str,
    target_path: &str,
    mr: &Value,
    cancelled: &AtomicBool,
) -> Result<(Vec<Check>, bool), GlabError> {
    // The MR detail names its own head pipeline, so the checks are the head's jobs rather than
    // whichever pipeline ran last (`specs/forge-providers.md`). No head pipeline is no checks.
    let pipeline = &mr["head_pipeline"];
    let Some(pipeline_id) = pipeline["id"].as_u64() else {
        return Ok((Vec::new(), false));
    };
    // A fork MR's pipeline can live in the source project; the pipeline names its own home.
    let project = match pipeline["project_id"].as_u64() {
        Some(id) => id.to_string(),
        None => target_path.to_string(),
    };
    // A fork MR's pipeline can be unreadable to the reviewer (private fork). An
    // unreadable pipeline project shows an empty checks list instead of failing the view
    // (`specs/forge-providers.md`).
    let (job_pages, jobs) = match glab_api_paged(
        repo,
        host,
        &format!("projects/{project}/pipelines/{pipeline_id}/jobs?per_page=100&page=1"),
        cancelled,
    ) {
        Ok(paged) => paged,
        Err(GlabError::Unavailable(_)) => return Ok((Vec::new(), false)),
        Err(error) => return Err(error),
    };
    let rows = jobs.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut checks: Vec<Check> = Vec::new();
    // Jobs arrive newest-first; iterate oldest-first so a re-run replaces its earlier run.
    for job in rows.iter().rev() {
        let name = job["name"].as_str().unwrap_or_default().to_string();
        if name.is_empty() {
            continue;
        }
        let allow_failure = job["allow_failure"].as_bool().unwrap_or(false);
        let status = job_status(job["status"].as_str().unwrap_or_default(), allow_failure);
        upsert_latest(&mut checks, Check { name, status });
    }
    // A header-less response past the cap can only be reported as capped.
    let capped = job_pages.map_or(rows.len() >= crate::forge::SURFACE_CAP, |total| total > 1);
    if capped {
        // The rollup reads the rows it has, so a prefix of a large pipeline could report a pass
        // while an unread job failed. The pipeline states its own verdict, which stands in for
        // the jobs left unread (`specs/forge-providers.md`).
        let status = pipeline_status(pipeline["status"].as_str().unwrap_or_default());
        upsert_latest(&mut checks, Check { name: "pipeline".to_string(), status });
    }
    Ok((checks, capped))
}

/// Normalise the head pipeline's own status to a [`CheckStatus`].
fn pipeline_status(status: &str) -> CheckStatus {
    match status {
        "success" => CheckStatus::Success,
        "failed" => CheckStatus::Failure,
        "running" => CheckStatus::Running,
        "canceled" | "skipped" | "manual" => CheckStatus::Skipped,
        _ => CheckStatus::Pending,
    }
}

/// Normalise one GitLab job status to a [`CheckStatus`]. An allowed-to-fail job leaves the
/// pipeline green and the merge request mergeable, so its failure is a warning, never a
/// failing check (`specs/forge-providers.md`).
fn job_status(status: &str, allow_failure: bool) -> CheckStatus {
    match status {
        "success" => CheckStatus::Success,
        // `when: manual` jobs default to allow_failure, and a cancelled one reaches here too.
        "failed" | "canceled" if allow_failure => CheckStatus::Skipped,
        "failed" | "canceled" => CheckStatus::Failure,
        "running" => CheckStatus::Running,
        "skipped" | "manual" => CheckStatus::Skipped,
        // created / pending / waiting_for_resource / scheduled — queued work.
        _ => CheckStatus::Pending,
    }
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// Assemble the snapshot from the MR detail, discussion rows, and approvals responses.
fn build_snapshot(
    mr: &Value,
    sync: Sync,
    checks: Vec<Check>,
    rows: &[Value],
    approvals: &Value,
    truncated: bool,
) -> PrSnapshot {
    PrSnapshot {
        number: mr["iid"].as_u64().unwrap_or_default(),
        title: mr["title"].as_str().unwrap_or_default().to_string(),
        url: mr["web_url"].as_str().unwrap_or_default().to_string(),
        body: mr["description"].as_str().unwrap_or_default().to_string(),
        // A missing state must not read as reviewable: the empty string falls through
        // `parse_state` to the closed arm — stale, never wrong (`specs/overview.md`).
        state: parse_state(mr["state"].as_str().unwrap_or_default()),
        is_draft: mr["draft"].as_bool().unwrap_or(false),
        head_ref: mr["source_branch"].as_str().unwrap_or_default().to_string(),
        head_is_fork: is_cross_project(mr),
        head_oid: mr["sha"].as_str().unwrap_or_default().to_string(),
        base_ref: mr["target_branch"].as_str().unwrap_or_default().to_string(),
        merge: derive_merge(mr),
        sync,
        checks,
        comments: merge_comments(rows, approvals),
        truncated,
    }
}

/// `opened` maps to `open`; a locked MR reads as closed (`specs/forge-providers.md`).
fn parse_state(state: &str) -> PrState {
    match state {
        "opened" => PrState::Open,
        "merged" => PrState::Merged,
        _ => PrState::Closed,
    }
}

/// A cross-project merge request sets the fork marker (`specs/forge-providers.md`).
fn is_cross_project(mr: &Value) -> bool {
    match (mr["source_project_id"].as_u64(), mr["target_project_id"].as_u64()) {
        (Some(source), Some(target)) => source != target,
        _ => false,
    }
}

/// Fold GitLab's merge state to the blockers worth surfacing: a conflict is `conflicting`,
/// unresolved blocking discussions or missing required approvals are `blocked`, and
/// everything else — including a still-checking status — is `clean` (`specs/forge-providers.md`).
fn derive_merge(mr: &Value) -> Merge {
    if mr["has_conflicts"].as_bool().unwrap_or(false)
        || mr["detailed_merge_status"].as_str() == Some("conflict")
    {
        return Merge::Conflicting;
    }
    let blocked_status = matches!(
        mr["detailed_merge_status"].as_str(),
        Some("blocked_status" | "discussions_not_resolved" | "not_approved" | "policies_denied")
    );
    // `detailed_merge_status` arrived in GitLab 15.6; the blocking flag covers the older
    // self-hosted instances that omit it.
    if blocked_status || mr["blocking_discussions_resolved"].as_bool() == Some(false) {
        return Merge::Blocked;
    }
    Merge::Clean
}

/// The first non-system note carrying a body — the comment's root — or `None` when the
/// discussion is a system-only or empty thread that renders no comment. The one definition of
/// "a discussion is a comment", shared by the newest-100 cap and the render.
fn comment_root(discussion: &Value) -> Option<&Value> {
    discussion["notes"].as_array()?.iter().find(|note| is_comment_note(note))
}

/// A note that renders: human-authored and carrying a body. The one predicate behind the
/// root pick and the reply count, so the two can never disagree.
fn is_comment_note(note: &Value) -> bool {
    !note["system"].as_bool().unwrap_or(false)
        && !note["body"].as_str().unwrap_or("").trim().is_empty()
}

/// Replies beyond the root: the comment notes on the thread, less one.
fn reply_count(discussion: &Value) -> u32 {
    let notes = discussion["notes"]
        .as_array()
        .map_or(0, |notes| notes.iter().filter(|note| is_comment_note(note)).count());
    notes.saturating_sub(1) as u32
}

/// Merge the discussion threads and approvals into one newest-first comment list:
/// MR-level notes are `comment` rows, diff-position discussions are `finding` rows, and an
/// approval is a `review` row (`specs/forge-providers.md`).
fn merge_comments(discussions: &[Value], approvals: &Value) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();
    for discussion in discussions {
        let Some(root) = comment_root(discussion) else { continue };
        let author = root["author"]["username"].as_str().unwrap_or("").to_string();
        let position = &root["position"];
        // A diff-position thread is a finding; anything else is a plain comment.
        let (kind, anchor, is_resolved) = if position.is_object() {
            let path = position["new_path"]
                .as_str()
                .or_else(|| position["old_path"].as_str())
                .unwrap_or("");
            let anchor =
                match position["new_line"].as_u64().or_else(|| position["old_line"].as_u64()) {
                    Some(line) => format!("{path}:{line}"),
                    None => path.to_string(),
                };
            (CommentKind::Finding, anchor, root["resolved"].as_bool().unwrap_or(false))
        } else {
            (CommentKind::Comment, "comment".to_string(), false)
        };
        out.push(Comment {
            kind,
            author_is_bot: is_gitlab_bot(&author),
            author,
            anchor,
            body: root["body"].as_str().unwrap_or("").trim().to_string(),
            snippet: None,
            created_at: root["created_at"].as_str().unwrap_or("").to_string(),
            is_resolved,
            is_outdated: false,
            reply_count: reply_count(discussion),
        });
    }
    for user in approvals["approved_by"].as_array().into_iter().flatten() {
        let author = user["user"]["username"].as_str().unwrap_or("").to_string();
        if author.is_empty() {
            continue;
        }
        let bot = is_gitlab_bot(&author);
        // The approvals surface carries no timestamp, so approvals sort after the
        // dated rows in the newest-first list.
        out.push(prose_row(
            CommentKind::Review,
            author,
            bot,
            "Approved this merge request.".to_string(),
            String::new(),
        ));
    }
    finish_comments(&mut out);
    out
}

/// Whether a GitLab username is a service account: the shared name heuristics, or GitLab's
/// access-token bots (`project_{id}_bot…` / `group_{id}_bot…`). GitLab exposes no bot flag
/// on a note's author, so the name is the only signal.
fn is_gitlab_bot(username: &str) -> bool {
    crate::forge::is_named_bot(username)
        || is_access_token_bot(username, "project_")
        || is_access_token_bot(username, "group_")
}

/// Whether `username` is `{prefix}{digits}_bot…` — the exact shape GitLab mints for
/// project and group access-token accounts.
fn is_access_token_bot(username: &str, prefix: &str) -> bool {
    let Some(rest) = username.strip_prefix(prefix) else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with("_bot")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mr_node() -> Value {
        json!({
            "iid": 42,
            "title": "Add search",
            "web_url": "https://gitlab.com/group/sub/repo/-/merge_requests/42",
            "description": "Adds the search screen.",
            "state": "opened",
            "draft": false,
            "source_branch": "feature/search",
            "target_branch": "main",
            "sha": "abc123",
            "source_project_id": 7,
            "target_project_id": 7,
            "has_conflicts": false,
            "detailed_merge_status": "mergeable",
            "blocking_discussions_resolved": true,
        })
    }

    #[test]
    fn an_empty_leading_note_neither_hides_the_thread_nor_counts_as_a_reply() {
        let discussion = json!({"notes": [
            {"system": false, "body": "  ", "author": {"username": "editor"}},
            {"system": false, "body": "The real comment.", "author": {"username": "author"}}
        ]});
        assert_eq!(comment_root(&discussion).unwrap()["body"], "The real comment.");
        assert_eq!(reply_count(&discussion), 0);
    }

    #[test]
    fn snapshot_maps_the_merge_request_fields() {
        let s = build_snapshot(&mr_node(), Sync::InSync, Vec::new(), &[], &json!({}), false);
        assert_eq!(s.number, 42);
        assert_eq!(s.title, "Add search");
        assert_eq!(s.state, PrState::Open);
        assert!(!s.is_draft);
        assert_eq!(s.head_ref, "feature/search");
        assert!(!s.head_is_fork);
        assert_eq!(s.base_ref, "main");
        assert_eq!(s.merge, Merge::Clean);
        assert!(!s.truncated);
    }

    #[test]
    fn state_and_fork_mappings_follow_the_provider_contract() {
        assert_eq!(parse_state("opened"), PrState::Open);
        assert_eq!(parse_state("merged"), PrState::Merged);
        assert_eq!(parse_state("closed"), PrState::Closed);
        assert_eq!(parse_state("locked"), PrState::Closed);
        let mut mr = mr_node();
        mr["source_project_id"] = json!(9);
        assert!(is_cross_project(&mr));
    }

    #[test]
    fn branch_listings_pair_an_opened_page_with_the_finished_history_page() {
        // The all-state page is created-ordered and capped at 20, so on a reused branch
        // name it could bury an older still-open MR behind newer finished rows. The
        // opened page keeps the shared resolver's open-before-history promise whole.
        let listings = branch_listings("group%2Frepo", &["feat".to_string()]);
        assert_eq!(listings.len(), 2);
        assert!(listings[0].contains("source_branch=feat") && listings[0].contains("state=opened"));
        assert!(listings[1].contains("source_branch=feat") && !listings[1].contains("state="));
        for listing in &listings {
            assert!(listing.starts_with("projects/group%2Frepo/merge_requests?"));
            // Newest-created-first is what lets the finished page serve as history.
            assert!(listing.contains("order_by=created_at") && listing.contains("sort=desc"));
        }
    }

    #[test]
    fn collect_assoc_filters_by_source_project_and_skips_unavailable_listings() {
        let node = |iid: u64, state: &str, source: u64| {
            json!({"iid": iid, "state": state, "sha": "abc", "source_branch": "feat",
                "created_at": "2026-07-01T00:00:00Z", "merged_at": null, "closed_at": null,
                "source_project_id": source})
        };
        let rows: Vec<Result<Value, GlabError>> = vec![
            Ok(json!([node(1, "opened", 7), node(2, "merged", 7), node(3, "opened", 9)])),
            // An unavailable listing proves nothing and never fails the fold.
            Err(GlabError::Unavailable("404".to_string())),
        ];
        let assoc = collect_assoc(rows, |source| source == Some(7)).unwrap();
        assert_eq!(assoc.open.iter().map(|mr| mr.number).collect::<Vec<_>>(), [1]);
        assert_eq!(assoc.history.iter().map(|mr| mr.number).collect::<Vec<_>>(), [2]);
        // A hard error still fails.
        let rows: Vec<Result<Value, GlabError>> = vec![Err(GlabError::Other("boom".to_string()))];
        assert!(collect_assoc(rows, |_| true).is_err());
    }

    #[test]
    fn merge_folds_conflict_blocked_and_clean() {
        let mut mr = mr_node();
        assert_eq!(derive_merge(&mr), Merge::Clean);
        mr["has_conflicts"] = json!(true);
        assert_eq!(derive_merge(&mr), Merge::Conflicting);
        mr["has_conflicts"] = json!(false);
        mr["detailed_merge_status"] = json!("not_approved");
        assert_eq!(derive_merge(&mr), Merge::Blocked);
        mr["detailed_merge_status"] = json!("checking");
        assert_eq!(derive_merge(&mr), Merge::Clean);
        mr["blocking_discussions_resolved"] = json!(false);
        assert_eq!(derive_merge(&mr), Merge::Blocked);
    }

    #[test]
    fn job_statuses_normalise_to_check_statuses() {
        assert_eq!(job_status("success", false), CheckStatus::Success);
        assert_eq!(job_status("failed", false), CheckStatus::Failure);
        assert_eq!(job_status("canceled", false), CheckStatus::Failure);
        assert_eq!(job_status("running", false), CheckStatus::Running);
        assert_eq!(job_status("pending", false), CheckStatus::Pending);
        assert_eq!(job_status("skipped", false), CheckStatus::Skipped);
        // An allowed-to-fail job leaves the pipeline green, so it never reads as failing.
        assert_eq!(job_status("failed", true), CheckStatus::Skipped);
        assert_eq!(job_status("success", true), CheckStatus::Success);
    }

    #[test]
    fn a_status_is_read_from_its_marker_or_line_lead_but_never_from_an_oid() {
        // Both shapes `glab` emits for one failed request.
        assert!(crate::forge::reports_status("glab: 404 not found (http 404)", 404));
        assert!(crate::forge::reports_status("{\"message\":\"404 project not found\"}", 404));
        assert!(crate::forge::reports_status("glab: 401 unauthorized (http 401)", 401));
        // A transport error echoes the endpoint; a 40-hex OID carries those digits about one
        // time in a hundred and must not read as absence or as an expired token.
        let transport = "get \"https://gitlab.com/api/v4/projects/1/repository/commits/\
                         de401f404a3b/merge_requests\": i/o timeout";
        assert!(!crate::forge::reports_status(transport, 404));
        assert!(!crate::forge::reports_status(transport, 401));
        assert!(matches!(classify_failure(transport), GlabError::Other(_)));
    }

    #[test]
    fn the_discussion_tail_keeps_the_newest_hundred_rows() {
        let rows: Vec<Value> = (0..250).map(|i| json!(i)).collect();
        let kept = crate::forge::newest_capped(rows);
        assert_eq!(kept.len(), 100);
        assert_eq!(kept.first().unwrap(), &json!(150));
        assert_eq!(kept.last().unwrap(), &json!(249));

        let short: Vec<Value> = (0..3).map(|i| json!(i)).collect();
        assert_eq!(crate::forge::newest_capped(short).len(), 3);
    }

    #[test]
    fn only_the_last_two_pages_beyond_page_one_are_fetched() {
        assert_eq!(discussion_tail_pages(1), Vec::<u64>::new());
        assert_eq!(discussion_tail_pages(2), vec![2]);
        assert_eq!(discussion_tail_pages(3), vec![2, 3]);
        assert_eq!(discussion_tail_pages(4), vec![3, 4]);
    }

    /// `n` stand-in comment rows numbered `[from, from + n)`, oldest-first.
    fn page_rows(from: i64, n: i64) -> Vec<Value> {
        (from..from + n).map(|i| json!(i)).collect()
    }

    #[test]
    fn a_single_page_thread_keeps_every_row_and_is_not_capped() {
        let (rows, truncated) = assemble_discussions(page_rows(0, 40), 1, Vec::new());
        assert_eq!(rows.len(), 40);
        assert!(!truncated);
    }

    #[test]
    fn two_pages_keep_the_newest_hundred_across_both_pages() {
        // Page 1 is [0, 100); page 2 is [100, 150). The newest 100 span the two.
        let (rows, truncated) = assemble_discussions(page_rows(0, 100), 2, page_rows(100, 50));
        assert_eq!(rows.len(), 100);
        assert_eq!(rows.first().unwrap(), &json!(50));
        assert_eq!(rows.last().unwrap(), &json!(149));
        assert!(truncated);
    }

    #[test]
    fn two_pages_below_the_cap_show_everything_and_are_not_truncated() {
        // A busy MR whose two raw pages filter down to 60 comments shows all 60, no `+more`.
        let (rows, truncated) = assemble_discussions(page_rows(0, 40), 2, page_rows(40, 20));
        assert_eq!(rows.len(), 60);
        assert!(!truncated, "everything fetched and shown is not truncated");
    }

    #[test]
    fn three_or_more_pages_drop_page_one_and_keep_the_newest_hundred() {
        // Page 1 [0,100) is the oldest and must not appear. Tail pages are [100,250); the kept
        // rows are the newest 100 of the tail.
        let (rows, truncated) = assemble_discussions(page_rows(0, 100), 3, page_rows(100, 150));
        assert_eq!(rows.len(), 100);
        assert_eq!(rows.first().unwrap(), &json!(150));
        assert_eq!(rows.last().unwrap(), &json!(249));
        assert!(!rows.contains(&json!(99)), "the oldest page must be dropped");
        assert!(truncated);
    }

    #[test]
    fn comment_discussions_drops_system_and_empty_threads_but_counts_them_raw() {
        let response = json!([
            {"notes": [{"system": false, "body": "real comment", "author": {"username": "a"}}]},
            {"notes": [{"system": true, "body": "changed the milestone"}]},
            {"notes": [{"system": false, "body": "   "}]},
        ]);
        let rows = comment_discussions(response);
        assert_eq!(rows.len(), 1, "only the real comment survives the filter");
    }

    #[test]
    fn discussions_map_to_findings_comments_and_approvals_to_reviews() {
        let discussions = json!([
            {
                "notes": [
                    {"system": true, "body": "approved this merge request",
                     "author": {"username": "reviewer"}, "created_at": "2026-07-22T10:00:00Z"}
                ]
            },
            {
                "notes": [
                    {"system": false, "body": "Looks wrong.",
                     "author": {"username": "reviewer"},
                     "created_at": "2026-07-22T11:00:00Z",
                     "resolved": true,
                     "position": {"new_path": "src/a.rs", "new_line": 12}},
                    {"system": false, "body": "Fixed.",
                     "author": {"username": "author"}, "created_at": "2026-07-22T12:00:00Z"}
                ]
            },
            {
                "notes": [
                    {"system": false, "body": "General question.",
                     "author": {"username": "someone"}, "created_at": "2026-07-22T09:00:00Z"}
                ]
            }
        ]);
        let approvals = json!({"approved_by": [{"user": {"username": "reviewer"}}]});
        let comments = merge_comments(discussions.as_array().unwrap(), &approvals);
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].kind, CommentKind::Finding);
        assert_eq!(comments[0].anchor, "src/a.rs:12");
        assert!(comments[0].is_resolved);
        assert_eq!(comments[0].reply_count, 1);
        assert_eq!(comments[1].kind, CommentKind::Comment);
        assert_eq!(comments[2].kind, CommentKind::Review);
        assert_eq!(comments[2].author, "reviewer");
    }

    #[test]
    fn association_node_reduces_to_the_shared_pick_fields() {
        let node = json!({
            "iid": 7, "sha": "abc", "source_branch": "feature",
            "merged_at": "2026-07-21T00:00:00Z", "created_at": "2026-07-20T00:00:00Z",
            "state": "merged", "target_project_id": 3
        });
        let mr = assoc_mr(&node).unwrap();
        assert_eq!((mr.number, mr.head_oid.as_str()), (7, "abc"));
    }

    #[test]
    fn a_404_classifies_as_not_found_and_auth_wording_as_not_authed() {
        assert!(matches!(classify_failure("404 Commit Not Found"), GlabError::Unavailable(_)));
        assert!(matches!(classify_failure("HTTP 403 Forbidden"), GlabError::Unavailable(_)));
        assert!(matches!(classify_failure("HTTP 401: Unauthorized"), GlabError::NotAuthed));
        assert!(matches!(classify_failure("HTTP 500 something"), GlabError::Other(_)));
    }

    #[test]
    fn gitlab_service_accounts_count_as_bots() {
        assert!(is_gitlab_bot("project_123_bot_a1b2c3"));
        assert!(is_gitlab_bot("group_9_bot"));
        assert!(is_gitlab_bot("renovate[bot]"));
        assert!(!is_gitlab_bot("project_manager"));
        assert!(!is_gitlab_bot("group_bot_wrangler"));
        assert!(!is_gitlab_bot("project_ro_bottle"));
        assert!(!is_gitlab_bot("alice"));
        // Observed on gitlab.com: triage and dependency automation post under `-bot` names.
        assert!(is_gitlab_bot("gitlab-bot"));
        assert!(is_gitlab_bot("gitlab-dependency-update-bot"));
        assert!(!is_gitlab_bot("talbot"), "the hyphen keeps human names human");
    }

    #[test]
    fn glab_invocations_pin_the_hostname() {
        assert_eq!(
            glab_args("git.corp.example", "projects/x"),
            ["api", "--hostname", "git.corp.example", "projects/x", "--include"]
        );
    }

    #[test]
    fn paged_responses_split_headers_from_the_body() {
        let out = "HTTP/2.0 200 OK\r\nX-Total-Pages: 3\r\nX-Page: 1\r\n\r\n[{\"iid\":1}]\n";
        let (total, body) = split_headers(out);
        assert_eq!(total, Some(3));
        assert_eq!(body, "[{\"iid\":1}]");
        let (none, body) = split_headers("[1,2]");
        assert_eq!(none, None);
        assert_eq!(body, "[1,2]");
    }

    #[test]
    fn urlencode_addresses_a_nested_project_path() {
        assert_eq!(crate::forge::urlencode("group/sub/repo"), "group%2Fsub%2Frepo");
        assert_eq!(crate::forge::urlencode("feature/x y"), "feature%2Fx%20y");
    }
}
