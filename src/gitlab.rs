//! The read-only GitLab provider behind `src/forge.rs`, through `glab api`.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::forge::{
    AssocPr, Association, Check, CheckStatus, Comment, CommentKind, Merge, PrFetchInput,
    PrSnapshot, PrState, PrView, Reply, Sync, finish_comments, prose_row, push_unique,
    upsert_latest,
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
    /// A 404 or 403: GitLab answers 404 for private objects, so the two are one state.
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

/// An unreadable optional surface reads as empty instead of failing the fetch.
fn optional_surface(result: Result<Value, GlabError>) -> Result<Value, GlabError> {
    match result {
        Err(GlabError::Unavailable(_)) => Ok(Value::Null),
        other => other,
    }
}

/// One API read's argv: `--hostname` beats an inherited `GITLAB_HOST`, `--include` keeps page totals.
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
    let mut cmd = crate::proc::command("glab");
    cmd.current_dir(repo).args(glab_args(host, endpoint));
    crate::forge::run_provider(
        cmd,
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

/// Run several `glab api` reads concurrently, results in call order.
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

/// Run one `glab api -i` read: its `x-total-pages` and its JSON body.
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

/// Split a `--include` response into `x-total-pages` and the body after the last blank line.
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

/// A failed `glab`'s state by stderr wording; it has no stable exit codes.
fn classify_failure(stderr: &str) -> GlabError {
    let s = stderr.to_lowercase();
    if crate::forge::reports_status(&s, 401)
        || s.contains("unauthorized")
        || s.contains("authentication")
        || s.contains("glab auth login")
        || s.contains("no token")
        // An under-scoped token: only a re-login unblocks it.
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
    // A `glab mr checkout` pin outranks the lookup, unless GitLab no longer resolves it.
    let (mr, iid, project) = if let Some(pin) = input.local.pin_on(crate::git::Forge::GitLab)
        && let Some(mr) = pin_outcome(read_mr(repo, &pin.repo, pin.number, cancelled))?
    {
        (mr, pin.number, &pin.repo)
    } else {
        let Some((iid, project)) = associate_by_branch(repo, input, target, cancelled)? else {
            return Ok(PrView::NoPr);
        };
        let Some(mr) = read_mr(repo, project, iid, cancelled)? else {
            return Ok(PrView::NoPr);
        };
        (mr, iid, project)
    };
    let host = project.host();
    let project_path = crate::forge::urlencode(&project.full_path());
    // Against the pinned HEAD, so a mid-fetch checkout never mixes two branches.
    let mr_head = mr["sha"].as_str().unwrap_or_default();
    let sync = crate::forge::local_sync(repo, input.local.head_oid.as_deref(), mr_head)
        .map_err(|error| GlabError::LocalGit(error.0))?;

    // The four surfaces read concurrently.
    let target_path = project_path.as_str();
    let (discussions, approvals, checks, drafts) = std::thread::scope(|scope| {
        let discussions =
            scope.spawn(|| newest_discussions(repo, host, target_path, iid, cancelled));
        let approvals = scope.spawn(|| {
            optional_surface(glab_api(
                repo,
                host,
                &format!("projects/{target_path}/merge_requests/{iid}/approvals"),
                cancelled,
            ))
        });
        let checks = scope.spawn(|| fetch_checks(repo, host, target_path, &mr, cancelled));
        let drafts = scope.spawn(|| {
            optional_surface(glab_api(
                repo,
                host,
                &format!("projects/{target_path}/merge_requests/{iid}/draft_notes?per_page=100"),
                cancelled,
            ))
        });
        (
            crate::forge::join_read(discussions, || died("discussions")),
            crate::forge::join_read(approvals, || died("approvals")),
            crate::forge::join_read(checks, || died("checks")),
            crate::forge::join_read(drafts, || died("draft notes")),
        )
    });
    let (rows, discussions_capped) = discussions?;
    let approvals = approvals?;
    let (checks, jobs_capped) = checks?;
    let drafts = drafts?;

    Ok(PrView::Pr(Box::new(build_snapshot(
        &mr,
        sync,
        checks,
        merge_comments(&rows, &approvals, &drafts),
        discussions_capped,
        jobs_capped,
    ))))
}

/// The newest comment discussions, from the last two pages: GitLab lists oldest first.
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
    // Past ~10k rows GitLab omits the total, so page 1 stands in, marked capped.
    if total_pages.is_none() && raw_first >= crate::forge::SURFACE_CAP {
        return Ok((page1, true));
    }
    let total = total_pages.unwrap_or(1).max(1);
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

/// The last two pages, past page 1.
fn discussion_tail_pages(total: u64) -> Vec<u64> {
    [total.saturating_sub(1), total].into_iter().filter(|page| *page >= 2).collect()
}

/// The response's discussions that render, so the cap counts only those.
fn comment_discussions(response: Value) -> Vec<Value> {
    match response {
        Value::Array(rows) => rows.into_iter().filter(|d| comment_root(d).is_some()).collect(),
        _ => Vec::new(),
    }
}

/// The newest 100 of the fetched discussions, and whether any were dropped or left unread.
fn assemble_discussions(page1: Vec<Value>, total: u64, later: Vec<Value>) -> (Vec<Value>, bool) {
    // Page 1 fills slots a filtered-down tail leaves empty; an unread middle shows as a gap.
    let mut pool = page1;
    pool.extend(later);
    // Past three pages a middle goes unread.
    let truncated = total > 3 || pool.len() > crate::forge::SURFACE_CAP;
    (crate::forge::newest_capped(pool), truncated)
}

/// One merge request's detail. `None` when the response names no merge request.
fn read_mr(
    repo: &Path,
    project: &crate::git::RepoTarget,
    iid: u64,
    cancelled: &AtomicBool,
) -> Result<Option<Value>, GlabError> {
    let path = crate::forge::urlencode(&project.full_path());
    let mr = glab_api(
        repo,
        project.host(),
        &format!("projects/{path}/merge_requests/{iid}"),
        cancelled,
    )?;
    Ok(mr["iid"].as_u64().is_some().then_some(mr))
}

/// A pinned read's MR, or `None` to fall back to the lookup when the pin is stale.
fn pin_outcome(read: Result<Option<Value>, GlabError>) -> Result<Option<Value>, GlabError> {
    match read {
        Err(GlabError::Unavailable(_)) => Ok(None),
        read => read,
    }
}

/// The branch's merge request and its project, from one wave of listings and id lookups.
fn associate_by_branch<'a>(
    repo: &Path,
    input: &'a PrFetchInput,
    target: &'a crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> Result<Option<(u64, &'a crate::git::RepoTarget)>, GlabError> {
    let heads = &input.local.heads;
    let names = input.local.head_names();
    if names.is_empty() {
        return Ok(None);
    }
    let head = input.local.head_oid.as_deref();
    // On a fork clone both projects are asked; upstream's pick wins.
    let fork = crate::forge::fork_repository(input.origin_repository.as_ref(), target);
    let projects = id_projects(target, fork, heads);
    let path = |project: &crate::git::RepoTarget| crate::forge::urlencode(&project.full_path());
    let mut endpoints: Vec<String> =
        projects.iter().map(|project| format!("projects/{}", path(project))).collect();
    endpoints.extend(branch_listings(&path(target), &names));
    if let Some(fork) = fork {
        endpoints.extend(branch_listings(&path(fork), &names));
    }
    let mut responses = glab_api_fan_out(repo, target.host(), &endpoints, cancelled).into_iter();
    let ids = read_ids(&projects, &mut responses)?;
    let target_rows: Vec<_> = responses.by_ref().take(2 * names.len()).collect();
    let assoc = collect_assoc(target_rows, |node| mr_admitted(node, target, &ids, heads))?;
    if let Some(iid) = crate::forge::resolve_pick(repo, &assoc, head)
        .map_err(|error| GlabError::LocalGit(error.0))?
    {
        return Ok(Some((iid, target)));
    }
    if let Some(fork) = fork {
        let assoc =
            collect_assoc(responses.collect(), |node| mr_admitted(node, fork, &ids, heads))?;
        if let Some(iid) = crate::forge::resolve_pick(repo, &assoc, head)
            .map_err(|error| GlabError::LocalGit(error.0))?
        {
            return Ok(Some((iid, fork)));
        }
    }
    Ok(None)
}

/// The wave's leading id lookups; an unreadable non-target project just admits nothing.
fn read_ids<'a>(
    projects: &[&'a crate::git::RepoTarget],
    responses: &mut impl Iterator<Item = Result<Value, GlabError>>,
) -> Result<Vec<(&'a crate::git::RepoTarget, Option<u64>)>, GlabError> {
    let Some((target, others)) = projects.split_first() else { return Ok(Vec::new()) };
    let target_id = responses.next().transpose()?.and_then(|v| v["id"].as_u64());
    let Some(target_id) = target_id else {
        return Err(GlabError::Other("project lookup returned no id".to_string()));
    };
    let mut ids = vec![(*target, Some(target_id))];
    for project in others {
        let id = match responses.next() {
            Some(Ok(v)) => v["id"].as_u64(),
            Some(Err(GlabError::Unavailable(_))) | None => None,
            Some(Err(error)) => return Err(error),
        };
        ids.push((*project, id));
    }
    Ok(ids)
}

/// A project's id as the association's lookups read it.
fn project_id(
    ids: &[(&crate::git::RepoTarget, Option<u64>)],
    project: &crate::git::RepoTarget,
) -> Option<u64> {
    ids.iter().find(|(have, _)| have.is(project)).and_then(|(_, id)| *id)
}

/// Whether a listed MR's source project id and branch are one of the heads.
fn mr_admitted(
    node: &Value,
    queried: &crate::git::RepoTarget,
    ids: &[(&crate::git::RepoTarget, Option<u64>)],
    heads: &[crate::git::Head],
) -> bool {
    let source = node["source_project_id"].as_u64();
    let head_ref = node["source_branch"].as_str().unwrap_or_default();
    let head_repo = if source.is_some() && source == project_id(ids, queried) {
        crate::forge::HeadRepo::Queried
    } else {
        crate::forge::HeadRepo::Other(
            ids.iter()
                .filter(|(_, id)| source.is_some() && *id == source)
                .map(|(project, _)| *project)
                .collect(),
        )
    };
    crate::forge::admits(heads, queried, head_ref, &head_repo)
}

/// The projects to look up ids for, target first: the fork and every head on the target's host.
fn id_projects<'a>(
    target: &'a crate::git::RepoTarget,
    fork: Option<&'a crate::git::RepoTarget>,
    heads: &'a [crate::git::Head],
) -> Vec<&'a crate::git::RepoTarget> {
    let mut projects = vec![target];
    let candidates = fork.into_iter().chain(heads.iter().map(|head| &head.repo));
    for project in candidates {
        if project.forge() == target.forge()
            && project.host() == target.host()
            && !projects.iter().any(|have| have.is(project))
        {
            projects.push(project);
        }
    }
    projects
}

/// Per name, an opened page and an all-state page, so history never buries an open MR.
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

/// One project's admitted MRs; a 404 listing proves nothing.
fn collect_assoc(
    rows: Vec<Result<Value, GlabError>>,
    allowed: impl Fn(&Value) -> bool,
) -> Result<Association, GlabError> {
    let mut assoc = Association::default();
    for result in rows {
        let v = match result {
            Ok(v) => v,
            Err(GlabError::Unavailable(_)) => continue,
            Err(error) => return Err(error),
        };
        for node in v.as_array().into_iter().flatten() {
            if !allowed(node) {
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

/// The head pipeline's jobs as checks, and whether the page was capped.
fn fetch_checks(
    repo: &Path,
    host: &str,
    target_path: &str,
    mr: &Value,
    cancelled: &AtomicBool,
) -> Result<(Vec<Check>, bool), GlabError> {
    // The head's pipeline, not whichever ran last.
    let pipeline = &mr["head_pipeline"];
    let Some(pipeline_id) = pipeline["id"].as_u64() else {
        return Ok((Vec::new(), false));
    };
    // A fork MR's pipeline can live in the source project; the pipeline names its own home.
    let project = match pipeline["project_id"].as_u64() {
        Some(id) => id.to_string(),
        None => target_path.to_string(),
    };
    // A private fork's pipeline shows no checks instead of failing the view.
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
        // A capped page could hide a failed job, so the pipeline's own verdict stands in.
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

/// A job's status; an allowed-to-fail job never fails.
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

/// Assemble the snapshot from the MR detail and the merged comment rows.
fn build_snapshot(
    mr: &Value,
    sync: Sync,
    checks: Vec<Check>,
    comments: Vec<Comment>,
    comments_truncated: bool,
    checks_truncated: bool,
) -> PrSnapshot {
    PrSnapshot {
        number: mr["iid"].as_u64().unwrap_or_default(),
        title: mr["title"].as_str().unwrap_or_default().to_string(),
        url: mr["web_url"].as_str().unwrap_or_default().to_string(),
        body: mr["description"].as_str().unwrap_or_default().to_string(),
        // A missing state reads as closed, never reviewable.
        state: parse_state(mr["state"].as_str().unwrap_or_default()),
        is_draft: mr["draft"].as_bool().unwrap_or(false),
        head_ref: mr["source_branch"].as_str().unwrap_or_default().to_string(),
        head_is_fork: is_cross_project(mr),
        head_oid: mr["sha"].as_str().unwrap_or_default().to_string(),
        base_ref: mr["target_branch"].as_str().unwrap_or_default().to_string(),
        merge: derive_merge(mr),
        sync,
        checks,
        comments,
        comments_truncated,
        checks_truncated,
    }
}

/// `opened` maps to `open`; a locked MR reads as closed.
fn parse_state(state: &str) -> PrState {
    match state {
        "opened" => PrState::Open,
        "merged" => PrState::Merged,
        _ => PrState::Closed,
    }
}

/// A cross-project merge request sets the fork marker.
fn is_cross_project(mr: &Value) -> bool {
    match (mr["source_project_id"].as_u64(), mr["target_project_id"].as_u64()) {
        (Some(source), Some(target)) => source != target,
        _ => false,
    }
}

/// The merge blocker: a conflict, blocking discussions or approvals, else clean.
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
    // The flag covers instances older than 15.6, which lack `detailed_merge_status`.
    if blocked_status || mr["blocking_discussions_resolved"].as_bool() == Some(false) {
        return Merge::Blocked;
    }
    Merge::Clean
}

/// The discussion's root: its first note that renders, or `None` when none does.
fn comment_root(discussion: &Value) -> Option<&Value> {
    discussion["notes"].as_array()?.iter().find(|note| is_comment_note(note))
}

/// Whether a note renders: human-authored, with a body.
fn is_comment_note(note: &Value) -> bool {
    !note["system"].as_bool().unwrap_or(false)
        && !note["body"].as_str().unwrap_or("").trim().is_empty()
}

/// Replies beyond the root: every later comment note.
fn replies_from_discussion(discussion: &Value) -> Vec<Reply> {
    let Some(notes) = discussion["notes"].as_array() else {
        return Vec::new();
    };
    let Some(root_i) = notes.iter().position(is_comment_note) else {
        return Vec::new();
    };
    notes[root_i + 1..]
        .iter()
        .filter(|note| is_comment_note(note))
        .map(|note| {
            let author = note["author"]["username"].as_str().unwrap_or("").to_string();
            Reply {
                author_is_bot: is_gitlab_bot(&author),
                author,
                body: note["body"].as_str().unwrap_or("").trim().to_string(),
                created_at: note["created_at"].as_str().unwrap_or("").to_string(),
                draft_id: None,
            }
        })
        .collect()
}

/// Discussions, approvals, and draft notes as one newest-first list, drafts leading.
fn merge_comments(discussions: &[Value], approvals: &Value, drafts: &Value) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();
    let mut threads: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for discussion in discussions {
        let Some(root) = comment_root(discussion) else { continue };
        if let Some(id) = discussion["id"].as_str() {
            threads.insert(id, out.len());
        }
        let author = root["author"]["username"].as_str().unwrap_or("").to_string();
        let position = &root["position"];
        // A diff-position thread is a finding; anything else is a plain comment.
        let (kind, anchor, place, is_resolved) = if position.is_object() {
            let path = position["new_path"]
                .as_str()
                .or_else(|| position["old_path"].as_str())
                .unwrap_or("");
            let ((start, end), side) = gitlab_position(position);
            let place = crate::forge::FindingPlace::from_lines(path, start, end, side);
            (
                CommentKind::Finding,
                place.anchor(),
                Some(place),
                root["resolved"].as_bool().unwrap_or(false),
            )
        } else {
            (CommentKind::Comment, "comment".to_string(), None, false)
        };
        out.push(Comment {
            kind,
            author_is_bot: is_gitlab_bot(&author),
            author,
            anchor,
            place,
            body: root["body"].as_str().unwrap_or("").trim().to_string(),
            snippet: None,
            created_at: root["created_at"].as_str().unwrap_or("").to_string(),
            is_resolved,
            is_outdated: false,
            replies: replies_from_discussion(discussion),
            draft_id: None,
        });
    }
    merge_drafts(&mut out, &threads, drafts);
    for user in approvals["approved_by"].as_array().into_iter().flatten() {
        let author = user["user"]["username"].as_str().unwrap_or("").to_string();
        if author.is_empty() {
            continue;
        }
        let bot = is_gitlab_bot(&author);
        // Approvals have no timestamp, so they sort after the dated rows.
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

/// Drafts are always the caller's; the surface carries no username.
const DRAFT_AUTHOR: &str = "you";

/// Fold draft notes into `out`: a reply under its loaded thread, else a row of its own.
fn merge_drafts(
    out: &mut Vec<Comment>,
    threads: &std::collections::HashMap<&str, usize>,
    drafts: &Value,
) {
    for draft in drafts.as_array().into_iter().flatten() {
        let Some(id) = draft["id"].as_u64() else { continue };
        let note = draft["note"].as_str().unwrap_or("").trim();
        let body = if note.is_empty() { "_Empty draft._".to_string() } else { note.to_string() };
        let discussion = draft["discussion_id"].as_str().filter(|d| !d.is_empty());
        if let Some(&row) = discussion.and_then(|d| threads.get(d)) {
            out[row].replies.push(Reply {
                author: DRAFT_AUTHOR.to_string(),
                author_is_bot: false,
                body,
                created_at: String::new(),
                draft_id: Some(id),
            });
            continue;
        }
        // The thread is outside the fetched discussions.
        let body = match discussion {
            Some(d) => format!("_Draft reply to thread `{d}`, which is not loaded._\n\n{body}"),
            None => body,
        };
        let position = &draft["position"];
        let path = position["new_path"].as_str().or_else(|| position["old_path"].as_str());
        let (kind, anchor, place) = match path {
            Some(path) => {
                let ((start, end), side) = gitlab_position(position);
                let place = crate::forge::FindingPlace::from_lines(path, start, end, side);
                (CommentKind::Finding, place.anchor(), Some(place))
            }
            None => (CommentKind::Comment, "comment".to_string(), None),
        };
        out.push(Comment {
            kind,
            author: DRAFT_AUTHOR.to_string(),
            author_is_bot: false,
            anchor,
            place,
            body,
            snippet: None,
            created_at: String::new(),
            is_resolved: false,
            is_outdated: false,
            replies: Vec::new(),
            draft_id: Some(id),
        });
    }
}

fn gitlab_position(position: &Value) -> ((Option<u64>, Option<u64>), Option<crate::model::Side>) {
    let range = &position["line_range"];
    if range.is_object() {
        let (start, start_new) = gitlab_end(&range["start"]);
        let (end, end_new) = gitlab_end(&range["end"]);
        if start.is_some() || end.is_some() {
            let mixed = start.is_some() && end.is_some() && start_new != end_new;
            if mixed {
                let start =
                    range["start"]["new_line"].as_u64().or(range["start"]["old_line"].as_u64());
                let end = range["end"]["new_line"].as_u64().or(range["end"]["old_line"].as_u64());
                return ((start.or(end), end.or(start)), crate::forge::finding_side(true, false));
            }
            let on_new = start_new || end_new;
            return ((start.or(end), end.or(start)), crate::forge::finding_side(on_new, !on_new));
        }
    }
    let new = position["new_line"].as_u64();
    let old = position["old_line"].as_u64();
    let line = new.or(old);
    ((line, line), crate::forge::finding_side(new.is_some(), old.is_some()))
}

fn gitlab_end(end: &Value) -> (Option<u64>, bool) {
    match end["type"].as_str() {
        Some("old") => (end["old_line"].as_u64().or(end["new_line"].as_u64()), false),
        Some("new") => (end["new_line"].as_u64().or(end["old_line"].as_u64()), true),
        _ if end["new_line"].as_u64().is_some() => (end["new_line"].as_u64(), true),
        _ => (end["old_line"].as_u64(), false),
    }
}

/// Whether a username is a service account, judged by name: GitLab has no bot flag.
fn is_gitlab_bot(username: &str) -> bool {
    crate::forge::is_named_bot(username)
        || is_access_token_bot(username, "project_")
        || is_access_token_bot(username, "group_")
}

/// Whether `username` is `{prefix}{digits}_bot…`, GitLab's access-token account shape.
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
        assert!(replies_from_discussion(&discussion).is_empty());
    }

    #[test]
    fn snapshot_maps_the_merge_request_fields() {
        let s = build_snapshot(&mr_node(), Sync::InSync, Vec::new(), Vec::new(), false, false);
        assert_eq!(s.number, 42);
        assert_eq!(s.title, "Add search");
        assert_eq!(s.state, PrState::Open);
        assert!(!s.is_draft);
        assert_eq!(s.head_ref, "feature/search");
        assert!(!s.head_is_fork);
        assert_eq!(s.base_ref, "main");
        assert_eq!(s.merge, Merge::Clean);
        assert!(!s.comments_truncated && !s.checks_truncated);
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
        // A capped all-state page could bury an older open MR, so an opened page rides along.
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

    fn gl(path: &[&str]) -> crate::git::RepoTarget {
        crate::git::RepoTarget::with_path(crate::git::Forge::GitLab, "gitlab.com", path).unwrap()
    }

    #[test]
    fn a_merge_request_attaches_only_when_its_source_is_one_of_the_branchs_heads() {
        let upstream = gl(&["acme", "widgets"]);
        let fork = gl(&["contributor", "widgets"]);
        let head = |repo: &crate::git::RepoTarget, name: &str| crate::git::Head {
            repo: repo.clone(),
            name: name.to_string(),
        };
        let mr = |branch: &str, source: u64| json!({"source_branch": branch, "source_project_id": source});
        // Ids as the lookups read them: upstream 7, the fork 9. An unreadable project is None.
        let ids = [(&upstream, Some(7)), (&fork, Some(9))];
        let on_main = [head(&upstream, "main")];
        let fork_fix = [head(&fork, "fix")];
        let cases: &[(&str, &crate::git::RepoTarget, &[crate::git::Head], Value, bool)] = &[
            ("stranger fork main on main", &upstream, &on_main, mr("main", 42), false),
            ("own same-project main", &upstream, &on_main, mr("main", 7), true),
            ("fork clone, its own fix", &upstream, &fork_fix, mr("fix", 9), true),
            // A fork clone's branch that only tracks upstream: upstream's own `fix` is a stranger's.
            ("fork clone, upstream's own fix", &upstream, &fork_fix, mr("fix", 7), false),
            ("fork's internal MR, fork queried", &fork, &fork_fix, mr("fix", 9), true),
            ("upstream-sourced MR, fork queried", &fork, &fork_fix, mr("fix", 7), false),
            ("no source id", &upstream, &on_main, json!({"source_branch": "main"}), false),
        ];
        for (label, queried, heads, node, expected) in cases {
            assert_eq!(mr_admitted(node, queried, &ids, heads), *expected, "{label}");
        }
        // An unreadable fork project admits nothing sourced there.
        let unreadable = [(&upstream, Some(7)), (&fork, None)];
        assert!(!mr_admitted(&mr("fix", 9), &upstream, &unreadable, &fork_fix));
    }

    #[test]
    fn a_renamed_fork_matches_through_either_path_sharing_its_id() {
        let upstream = gl(&["acme", "widgets"]);
        let (new, old) = (gl(&["alice", "new"]), gl(&["alice", "old"]));
        let ids = [(&upstream, Some(7)), (&new, Some(9)), (&old, Some(9))];
        let heads = [
            crate::git::Head { repo: new.clone(), name: "work".into() },
            crate::git::Head { repo: old.clone(), name: "feature".into() },
        ];
        let mr = json!({"source_branch": "feature", "source_project_id": 9});
        assert!(mr_admitted(&mr, &upstream, &ids, &heads));
    }

    #[test]
    fn project_ids_fail_only_on_the_target_and_leave_the_listings_aligned() {
        let (upstream, fork, other) =
            (gl(&["acme", "w"]), gl(&["contributor", "w"]), gl(&["x", "w"]));
        let projects = [&upstream, &fork, &other];
        let listing = json!(["listing"]);
        let mut wave = vec![
            Ok(json!({"id": 7})),
            Err(GlabError::Unavailable("403".into())),
            Err(GlabError::Unavailable("404".into())),
            Ok(listing.clone()),
        ]
        .into_iter();
        let ids = read_ids(&projects, &mut wave).unwrap();
        assert_eq!(ids.iter().map(|(_, id)| *id).collect::<Vec<_>>(), [Some(7), None, None]);
        assert_eq!(wave.next().unwrap().unwrap(), listing, "the listings start right after");
        // The target's own failure surfaces verbatim.
        let mut wave =
            vec![Err(GlabError::Unavailable("404 Project Not Found".into()))].into_iter();
        assert!(matches!(read_ids(&projects, &mut wave),
            Err(GlabError::Unavailable(m)) if m.contains("Not Found")));
        let mut wave = vec![Ok(json!({}))].into_iter();
        assert!(read_ids(&projects, &mut wave).is_err(), "a target with no id cannot filter");
        // A transient failure on another project fails the fetch, never reads as "no MR".
        let mut wave = vec![Ok(json!({"id": 7})), Err(GlabError::Other("502".into()))].into_iter();
        assert!(matches!(read_ids(&projects, &mut wave), Err(GlabError::Other(_))));
    }

    #[test]
    fn a_pinned_merge_request_falls_back_only_when_gitlab_no_longer_has_it() {
        let mr = json!({"iid": 45});
        assert_eq!(pin_outcome(Ok(Some(mr.clone()))).unwrap(), Some(mr));
        assert_eq!(pin_outcome(Ok(None)).unwrap(), None);
        assert_eq!(pin_outcome(Err(GlabError::Unavailable("404".into()))).unwrap(), None);
        assert!(pin_outcome(Err(GlabError::Other("502".into()))).is_err());
        assert!(pin_outcome(Err(GlabError::NotAuthed)).is_err());
    }

    #[test]
    fn head_projects_ride_the_first_wave_once_each() {
        let upstream = gl(&["acme", "widgets"]);
        let fork = gl(&["contributor", "widgets"]);
        let other_host = crate::git::RepoTarget::with_path(
            crate::git::Forge::GitLab,
            "gitlab.corp",
            &["x", "y"],
        )
        .unwrap();
        let heads = [
            crate::git::Head { repo: fork.clone(), name: "a".into() },
            crate::git::Head { repo: gl(&["Contributor", "Widgets"]), name: "b".into() },
            crate::git::Head { repo: upstream.clone(), name: "c".into() },
            crate::git::Head { repo: other_host, name: "d".into() },
        ];
        let got: Vec<String> =
            id_projects(&upstream, Some(&fork), &heads).iter().map(|p| p.full_path()).collect();
        assert_eq!(got, ["acme/widgets", "contributor/widgets"]);
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
        let assoc = collect_assoc(rows, |node| node["source_project_id"] == 7).unwrap();
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
        // An echoed OID containing `404` or `401` must not read as absence or expiry.
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
        // The kept rows are the newest 100, none from page 1.
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
                     "position": {"new_path": "src/a.rs", "new_line": 10,
                      "line_range": {"start": {"new_line": 10}, "end": {"new_line": 12}}}},
                    {"system": false, "body": "Fixed.",
                     "author": {"username": "author"}, "created_at": "2026-07-22T12:00:00Z"}
                ]
            },
            {
                "notes": [
                    {"system": false, "body": "General question.",
                     "author": {"username": "someone"}, "created_at": "2026-07-22T09:00:00Z"}
                ]
            },
            {
                "notes": [
                    {"system": false, "body": "On the old column.",
                     "author": {"username": "reviewer"},
                     "created_at": "2026-07-22T13:00:00Z",
                     "position": {"old_path": "src/a.rs",
                      "line_range": {"start": {"type": "old", "old_line": 8, "new_line": 10},
                                     "end": {"type": "old", "old_line": 9, "new_line": 11}}}}
                ]
            }
        ]);
        let approvals = json!({"approved_by": [{"user": {"username": "reviewer"}}]});
        let comments = merge_comments(discussions.as_array().unwrap(), &approvals, &Value::Null);
        assert_eq!(comments.len(), 4);
        let finding = comments.iter().find(|c| c.body == "Looks wrong.").unwrap();
        assert_eq!(finding.kind, CommentKind::Finding);
        assert_eq!(finding.anchor, "src/a.rs:10-12");
        assert_eq!(finding.place.as_ref().unwrap().side, Some(crate::model::Side::New));
        assert!(finding.is_resolved);
        assert_eq!(finding.replies.len(), 1);
        assert_eq!(finding.replies[0].body, "Fixed.");
        let old = comments.iter().find(|c| c.body == "On the old column.").unwrap();
        assert_eq!(old.anchor, "src/a.rs:8-9");
        assert_eq!(old.place.as_ref().unwrap().side, Some(crate::model::Side::Old));
        assert!(comments.iter().any(|c| c.kind == CommentKind::Comment));
        assert_eq!(
            comments.iter().find(|c| c.kind == CommentKind::Review).unwrap().author,
            "reviewer"
        );
    }

    /// A draft note as GitLab returns it: no author object, no timestamp.
    fn draft(id: u64, note: &str, discussion: Option<&str>, position: &Value) -> Value {
        json!({"id": id, "author_id": 5, "note": note, "discussion_id": discussion,
            "position": position, "resolve_discussion": false})
    }

    fn null_position() -> Value {
        json!({"base_sha": null, "start_sha": null, "head_sha": null, "old_path": null,
            "new_path": null, "position_type": null, "old_line": null, "new_line": null,
            "line_range": null})
    }

    fn one_thread() -> Value {
        json!([{
            "id": "abc123",
            "notes": [{"system": false, "body": "Published.", "author": {"username": "reviewer"},
                "created_at": "2026-07-22T11:00:00Z"}]
        }, {
            "id": "def456",
            "notes": [{"system": false, "body": "Newer.", "author": {"username": "reviewer"},
                "created_at": "2026-07-22T12:00:00Z"}]
        }])
    }

    #[test]
    fn a_general_draft_is_a_leading_plain_comment() {
        let drafts = json!([draft(684_068, "General remark.", None, &null_position())]);
        let comments = merge_comments(one_thread().as_array().unwrap(), &json!({}), &drafts);
        assert_eq!(comments.len(), 3);
        let first = &comments[0];
        assert_eq!(first.draft_id, Some(684_068));
        assert_eq!(first.kind, CommentKind::Comment);
        assert_eq!((first.author.as_str(), first.anchor.as_str()), ("you", "comment"));
        assert_eq!(first.body, "General remark.");
        assert!(first.place.is_none());
        assert_eq!(comments[1].body, "Newer.");
        assert!(comments[1..].iter().all(|c| c.draft_id.is_none()));
    }

    #[test]
    fn a_positioned_draft_is_a_finding_on_its_lines() {
        let position = json!({"position_type": "text", "old_path": "src/a.rs",
            "new_path": "src/a.rs", "old_line": null, "new_line": 14,
            "line_range": {"start": {"type": "new", "new_line": 12},
                           "end": {"type": "new", "new_line": 14}}});
        let old_only = json!({"position_type": "text", "old_path": "src/gone.rs",
            "new_path": null, "old_line": 3, "new_line": null, "line_range": null});
        let drafts = json!([
            draft(684_069, "Range finding.", None, &position),
            draft(684_070, "Old side.", None, &old_only),
        ]);
        let comments = merge_comments(&[], &json!({}), &drafts);
        assert_eq!(comments.len(), 2);
        let finding = &comments[0];
        assert_eq!(finding.kind, CommentKind::Finding);
        assert_eq!(finding.anchor, "src/a.rs:12-14");
        assert_eq!(finding.place.as_ref().unwrap().side, Some(crate::model::Side::New));
        assert_eq!(comments[1].anchor, "src/gone.rs:3");
        assert_eq!(comments[1].place.as_ref().unwrap().side, Some(crate::model::Side::Old));
    }

    #[test]
    fn a_reply_draft_lands_under_its_thread_and_lifts_it() {
        let drafts = json!([draft(684_071, "Agreed, fix it.", Some("abc123"), &null_position())]);
        let comments = merge_comments(one_thread().as_array().unwrap(), &json!({}), &drafts);
        assert_eq!(comments.len(), 2, "a reply adds no row");
        let thread = &comments[0];
        assert_eq!(thread.body, "Published.");
        assert_eq!(thread.draft_id, None);
        assert_eq!(thread.replies.len(), 1);
        assert_eq!(thread.replies[0].draft_id, Some(684_071));
        assert_eq!(thread.replies[0].author, "you");
        assert_eq!(thread.replies[0].body, "Agreed, fix it.");
    }

    #[test]
    fn a_reply_draft_to_an_unloaded_thread_stands_alone_with_a_hint() {
        let drafts = json!([draft(7, "Late reply.", Some("feedbeef"), &null_position())]);
        let comments = merge_comments(one_thread().as_array().unwrap(), &json!({}), &drafts);
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].draft_id, Some(7));
        assert!(comments[0].body.contains("`feedbeef`"), "{}", comments[0].body);
        assert!(comments[0].body.ends_with("Late reply."));
    }

    #[test]
    fn an_empty_draft_still_shows() {
        let drafts = json!([draft(9, "   ", None, &null_position()), {"note": "no id"}]);
        let comments = merge_comments(&[], &json!({}), &drafts);
        assert_eq!(comments.len(), 1, "a draft without an id is dropped");
        assert_eq!(comments[0].draft_id, Some(9));
        assert_eq!(comments[0].body, "_Empty draft._");
    }

    #[test]
    fn an_unavailable_draft_surface_shows_no_drafts() {
        let drafts = optional_surface(Err(GlabError::Unavailable("404 Not Found".into()))).unwrap();
        let comments = merge_comments(one_thread().as_array().unwrap(), &json!({}), &drafts);
        assert_eq!(comments.len(), 2);
        assert!(comments.iter().all(|c| c.draft_id.is_none()));
        assert!(optional_surface(Err(GlabError::Other("boom".into()))).is_err());
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
