//! Read-only Azure DevOps access: the pull request's identity, state, policies, and threads.
//!
//! The Azure DevOps provider behind `src/forge.rs` (`specs/forge-providers.md`). It follows
//! the neutral resolution contract in `specs/forge-host.md` — the branch's forge names
//! filter an enumeration by `sourceRefName` — through the `az` CLI with the `azure-devops`
//! extension, and fills the same normalized [`PrSnapshot`] the other providers do. It never
//! writes to Azure DevOps.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::forge::{
    AssocPr, Association, Check, CheckStatus, Comment, CommentKind, Merge, PrFetchInput,
    PrSnapshot, PrState, PrView, Sync, finish_comments, prose_row, push_unique, upsert_latest,
};

/// Read Azure DevOps for one already-derived input. Degradation stays in-band for the PR tab.
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

/// A classified `az` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug)]
enum AzError {
    NoAz,
    /// `az` runs but the `azure-devops` extension is absent, so no DevOps command exists.
    NoExtension,
    NotAuthed,
    /// The endpoint answered 403 or 404 — the addressed object is unknown or unreadable.
    Unavailable(String),
    LocalGit(String),
    Other(String),
}

impl AzError {
    fn into_view(self, host: &str) -> PrView {
        match self {
            Self::NoAz => PrView::NoCli(crate::git::Forge::AzureDevOps),
            Self::NoExtension => PrView::NoExtension(crate::git::Forge::AzureDevOps),
            Self::NotAuthed => PrView::NotAuthed(crate::git::Forge::AzureDevOps, host.to_owned()),
            Self::LocalGit(message) => PrView::GitError(message),
            Self::Unavailable(message) | Self::Other(message) => {
                PrView::Error(crate::git::Forge::AzureDevOps, message)
            }
        }
    }
}

/// The retryable error a panicked reader degrades into (`crate::forge::join_read`).
fn died(surface: &str) -> AzError {
    AzError::Other(format!("{surface} read panicked"))
}

/// Fold an unreadable optional surface into an empty one: it contributes nothing to the
/// snapshot, never fails the whole fetch (`specs/forge-providers.md`).
fn optional_surface(result: Result<Value, AzError>) -> Result<Value, AzError> {
    match result {
        Err(AzError::Unavailable(_)) => Ok(Value::Null),
        other => other,
    }
}

/// The organization URL every `az` call pins with `--organization`, so an inherited
/// `AZURE_DEVOPS_*` default can never redirect the fetch (`specs/forge-host.md`). A legacy
/// `{org}.visualstudio.com` host is its own organization URL; every other host scopes by the
/// organization path segment.
fn organization_url(target: &crate::git::RepoTarget) -> String {
    let host = target.host();
    if host.ends_with(".visualstudio.com") {
        format!("https://{host}")
    } else {
        format!("https://{host}/{}", target.owner())
    }
}

/// Run one `az` command and parse its JSON stdout. `args` carries the whole subcommand; the
/// runner appends the `--organization` pin and the JSON output mode.
fn az_json(
    repo: &Path,
    org_url: &str,
    args: &[&str],
    cancelled: &AtomicBool,
) -> Result<Value, AzError> {
    let mut cmd = Command::new("az");
    cmd.current_dir(repo).args(args).args(["--organization", org_url, "--output", "json"]);
    let stdout = crate::forge::run_provider(
        &mut cmd,
        cancelled,
        AzError::NoAz,
        classify_failure,
        AzError::Other,
    )?;
    serde_json::from_str(stdout.trim()).map_err(|error| AzError::Other(error.to_string()))
}

/// Map a failed `az`'s stderr to a degraded state by its wording — like `gh` and `glab`, `az`
/// has no stable exit codes for these. The extension test leads: a missing extension also
/// mentions commands and would otherwise read as something else.
fn classify_failure(stderr: &str) -> AzError {
    let s = stderr.to_lowercase();
    if s.contains("requires the extension azure-devops")
        || s.contains("extension azure-devops")
        || s.contains("is not in the 'az' command group")
    {
        AzError::NoExtension
    } else if crate::forge::reports_status(&s, 401)
        || s.contains("you need to run the login command")
        || s.contains("requires user authentication")
        || s.contains("az login")
        || s.contains("az devops login")
        // TF400813 is Azure DevOps' unauthorized-identity error, raised for the anonymous
        // and the wrong-account reader alike.
        || s.contains("tf400813")
    {
        AzError::NotAuthed
    } else if crate::forge::reports_status(&s, 403)
        || crate::forge::reports_status(&s, 404)
        // TF401180: pull request not found. TF401019: repository not found. TF200016:
        // project not found. Unknown objects prove nothing; each call site decides whether
        // its surface is optional (`specs/forge-providers.md` — Query).
        || s.contains("tf401180")
        || s.contains("tf401019")
        || s.contains("tf200016")
    {
        AzError::Unavailable(stderr.trim().to_string())
    } else {
        AzError::Other(stderr.trim().to_string())
    }
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    target: &crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> Result<PrView, AzError> {
    let org_url = organization_url(target);
    let project = target.project();
    let repo_name = target.name();

    let head = input.local.head_oid.as_deref();
    let (mut assoc, project_guid) =
        associate_by_branch(repo, &org_url, project, repo_name, input, cancelled)?;
    let Some(id) = crate::forge::resolve_pick(repo, &assoc, head)
        .map_err(|error| AzError::LocalGit(error.0))?
    else {
        return Ok(PrView::NoPr);
    };
    // Every pick is enumeration-admitted and arrived as the complete pull request
    // (`branch_admitted`), so the fetch needs no detail read at all.
    let picked = [&mut assoc.open, &mut assoc.history]
        .into_iter()
        .flat_map(|bucket| bucket.iter_mut())
        .find(|pr| pr.number == id);
    let picked_tip = picked.as_ref().map(|pr| pr.head_oid.clone()).unwrap_or_default();
    let pr = picked.and_then(|pr| pr.raw.take()).unwrap_or(Value::Null);

    // The three surfaces are independent reads; they run in one concurrent wave so the
    // fetch's wall clock is one `az` call, not three. Each `az` invocation pays the CLI's
    // own startup on top of the request, so the wave count is the latency.
    let (threads, evaluations, statuses) = std::thread::scope(|scope| {
        let threads = scope.spawn(|| {
            az_json(
                repo,
                &org_url,
                &[
                    "devops",
                    "invoke",
                    "--area",
                    "git",
                    "--resource",
                    "pullRequestThreads",
                    "--route-parameters",
                    &format!("project={project}"),
                    &format!("repositoryId={repo_name}"),
                    &format!("pullRequestId={id}"),
                    "--api-version",
                    "7.1",
                ],
                cancelled,
            )
        });
        let evaluations = scope.spawn(|| {
            // Every association node that can yield a pick carries the project id, so a
            // pick without one is a malformed payload. A policy surface the reader cannot
            // address contributes no checks and no merge blocker instead of failing the
            // view (`specs/forge-providers.md`).
            match &project_guid {
                Some(guid) => fetch_evaluations(repo, &org_url, project, guid, id, cancelled),
                None => Ok(Value::Null),
            }
        });
        let statuses = scope.spawn(|| {
            if picked_tip.is_empty() {
                return Ok(Value::Null);
            }
            optional_surface(az_json(
                repo,
                &org_url,
                &[
                    "devops",
                    "invoke",
                    "--area",
                    "git",
                    "--resource",
                    "statuses",
                    "--route-parameters",
                    &format!("project={project}"),
                    &format!("repositoryId={repo_name}"),
                    &format!("commitId={picked_tip}"),
                    // The surface's one page; `latestOnly` collapses re-runs server-side
                    // and `one_page_capped` reports the overflow.
                    "--query-parameters",
                    "latestOnly=true",
                    "top=100",
                    "--api-version",
                    "7.1",
                ],
                cancelled,
            ))
        });
        (
            crate::forge::join_read(threads, || died("threads")),
            crate::forge::join_read(evaluations, || died("evaluations")),
            crate::forge::join_read(statuses, || died("statuses")),
        )
    });
    let threads = threads?;
    let evaluations = evaluations?;
    let statuses = statuses?;
    if pr["pullRequestId"].as_u64().is_none() {
        return Ok(PrView::NoPr);
    }
    // Sync compares the fetch's pinned HEAD to the PR's source tip, so a checkout or commit
    // landing mid-fetch never pairs one branch's PR with another branch's count. The pick
    // already derived that tip into `picked_tip`.
    let sync = crate::forge::local_sync(repo, input.local.head_oid.as_deref(), &picked_tip)
        .map_err(|error| AzError::LocalGit(error.0))?;

    let (rows, threads_capped) = newest_comment_threads(&threads);
    // A full checks page can hide older rows past it, exactly as a further thread page
    // does; either caps the surface (`specs/forge-host.md`).
    let checks_capped = one_page_capped(&evaluations) || one_page_capped(&statuses);
    let checks = build_checks(&evaluations, &statuses);
    Ok(PrView::Pr(Box::new(build_snapshot(
        &pr,
        target,
        &org_url,
        sync,
        checks,
        &rows,
        &evaluations,
        threads_capped || checks_capped,
    ))))
}

/// One policy-evaluations read for a pull request, keyed by the project GUID inside the
/// artifact id. An unreadable policy surface contributes no checks and no merge blocker
/// (`specs/forge-providers.md`).
fn fetch_evaluations(
    repo: &Path,
    org_url: &str,
    project: &str,
    project_guid: &str,
    id: u64,
    cancelled: &AtomicBool,
) -> Result<Value, AzError> {
    let artifact = format!("artifactId=vstfs:///CodeReview/CodeReviewId/{project_guid}/{id}");
    optional_surface(az_json(
        repo,
        org_url,
        &[
            "devops",
            "invoke",
            "--area",
            "policy",
            "--resource",
            "evaluations",
            "--route-parameters",
            &format!("project={project}"),
            // The surface's one page (`specs/forge-host.md`: each surface reads its
            // newest 100 rows); `one_page_capped` reports the overflow.
            "--query-parameters",
            &artifact,
            "$top=100",
            // `az devops invoke` rejects the dotted preview form (`7.1-preview.1`),
            // so the undotted preview alias addresses the endpoint.
            "--api-version",
            "7.1-preview",
        ],
        cancelled,
    ))
}

/// Ask Azure DevOps for the branch's pull requests: the newest 100 active and newest 100
/// completed enumerate in one concurrent wave, and a node joins when its source branch is
/// one of the names (`specs/forge-providers.md`). A fork node has no provable source
/// repository in the enumeration, so it joins only when the pinned `HEAD` contains its
/// source tip. Also returns the target's project GUID as the enumeration nodes report it,
/// so the policy read need not wait for anything else.
fn associate_by_branch(
    repo: &Path,
    org_url: &str,
    project: &str,
    repo_name: &str,
    input: &PrFetchInput,
    cancelled: &AtomicBool,
) -> Result<(Association, Option<String>), AzError> {
    let names = &input.local.names;
    let head = input.local.head_oid.as_deref();
    let mut assoc = Association::default();

    let enumerate = |status: &'static str| {
        az_json(
            repo,
            org_url,
            &[
                "repos",
                "pr",
                "list",
                "--project",
                project,
                "--repository",
                repo_name,
                "--status",
                status,
                "--top",
                "100",
            ],
            cancelled,
        )
    };
    let (active, completed) = std::thread::scope(|scope| {
        let active = scope.spawn(|| enumerate("active"));
        let completed = scope.spawn(|| enumerate("completed"));
        (
            crate::forge::join_read(active, || died("active")),
            crate::forge::join_read(completed, || died("completed")),
        )
    });
    let active = active?;
    let completed = completed?;

    let mut project_guid: Option<String> = None;
    let mut note_guid = |node: &Value| {
        if project_guid.is_none() {
            project_guid = project_guid_of(node);
        }
    };
    for node in active.as_array().into_iter().flatten() {
        note_guid(node);
        if let Some(pr) = branch_admitted(repo, node, names, head)? {
            push_unique(&mut assoc.open, pr);
        }
    }
    for node in completed.as_array().into_iter().flatten() {
        note_guid(node);
        if let Some(pr) = branch_admitted(repo, node, names, head)? {
            push_unique(&mut assoc.history, pr);
        }
    }
    Ok((assoc, project_guid))
}

/// The enumeration node's pick fields when its source branch is one of the names. A fork
/// node's source branch lives in an unnamed repository, so it is admitted only when the
/// pinned `HEAD` contains its source tip (`specs/forge-providers.md`).
fn branch_admitted(
    repo: &Path,
    node: &Value,
    names: &[String],
    head: Option<&str>,
) -> Result<Option<AssocPr>, AzError> {
    let Some(mut pr) = assoc_pr(node) else { return Ok(None) };
    if !names.contains(&pr.head_ref) {
        return Ok(None);
    }
    // This containment check answers a different question than the shared history
    // guard: whether a fork node — open ones included — is this clone's at all, since
    // the enumeration cannot name the fork repository.
    if !node["forkSource"].is_null() {
        let contained = match head {
            Some(head) if !pr.head_oid.is_empty() => {
                crate::git::contains_commit(repo, head, &pr.head_oid)
                    .map_err(|error| AzError::LocalGit(error.0))?
            }
            _ => false,
        };
        if !contained {
            return Ok(None);
        }
    }
    // An enumeration node is the complete pull request, so the pick it becomes needs no
    // detail read; the payload travels with the admission that proved it.
    pr.raw = Some(node.clone());
    Ok(Some(pr))
}

/// The project GUID one enumeration node reports on its repository's project.
fn project_guid_of(node: &Value) -> Option<String> {
    node["repository"]["project"]["id"].as_str().map(str::to_string)
}

/// One association node reduced to the pick-relevant fields shared with the other providers.
fn assoc_pr(node: &Value) -> Option<AssocPr> {
    Some(AssocPr {
        number: node["pullRequestId"].as_u64()?,
        head_oid: source_tip(node).to_string(),
        head_ref: head_ref_of(node),
        created_at: node["creationDate"].as_str().unwrap_or_default().to_string(),
        closed_at: match node["status"].as_str() {
            Some("completed") => node["closedDate"].as_str().unwrap_or_default().to_string(),
            _ => String::new(),
        },
        raw: None,
    })
}

/// The pull request's reported source tip.
fn source_tip(node: &Value) -> &str {
    node["lastMergeSourceCommit"]["commitId"].as_str().unwrap_or_default()
}

/// A `refs/heads/…` name reduced to its bare branch name.
fn bare_ref(name: &str) -> String {
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

/// The pull request's head branch name. A fork pull request reports the virtual
/// `refs/pull/{id}/source` as its `sourceRefName` and keeps the real branch on
/// `forkSource.name`.
fn head_ref_of(node: &Value) -> String {
    let name = node["forkSource"]["name"]
        .as_str()
        .unwrap_or_else(|| node["sourceRefName"].as_str().unwrap_or_default());
    bare_ref(name)
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// Assemble the snapshot from the picked pull request node, thread rows, and evaluations.
#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    pr: &Value,
    target: &crate::git::RepoTarget,
    org_url: &str,
    sync: Sync,
    checks: Vec<Check>,
    rows: &[&Value],
    evaluations: &Value,
    truncated: bool,
) -> PrSnapshot {
    let id = pr["pullRequestId"].as_u64().unwrap_or_default();
    PrSnapshot {
        number: id,
        title: pr["title"].as_str().unwrap_or_default().to_string(),
        url: format!(
            "{org_url}/{}/_git/{}/pullrequest/{id}",
            crate::forge::urlencode(target.project()),
            crate::forge::urlencode(target.name())
        ),
        body: pr["description"].as_str().unwrap_or_default().to_string(),
        // A missing status must not read as reviewable: the empty string falls through
        // `parse_state` to the closed arm — stale, never wrong (`specs/overview.md`).
        state: parse_state(pr["status"].as_str().unwrap_or_default()),
        is_draft: pr["isDraft"].as_bool().unwrap_or(false),
        head_ref: head_ref_of(pr),
        head_is_fork: pr["forkSource"].is_object(),
        head_oid: source_tip(pr).to_string(),
        base_ref: bare_ref(pr["targetRefName"].as_str().unwrap_or_default()),
        merge: derive_merge(pr, evaluations),
        sync,
        checks,
        comments: merge_comments(rows, pr),
        truncated,
    }
}

/// Only `active` and `completed` are ever picked; every other status, a missing one
/// included, is non-reviewable and reads as closed (`specs/forge-providers.md`).
fn parse_state(status: &str) -> PrState {
    match status {
        "active" => PrState::Open,
        "completed" => PrState::Merged,
        _ => PrState::Closed,
    }
}

/// Fold Azure DevOps' merge state to the blockers worth surfacing: a conflict is
/// `conflicting`, a rejected required policy is `blocked`, and everything else — including a
/// still-queued merge check — is `clean` (`specs/forge-providers.md`).
fn derive_merge(pr: &Value, evaluations: &Value) -> Merge {
    if pr["mergeStatus"].as_str() == Some("conflicts") {
        return Merge::Conflicting;
    }
    let rejected_required = evaluations["value"].as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            row["configuration"]["isBlocking"].as_bool().unwrap_or(false)
                && row["status"].as_str() == Some("rejected")
        })
    });
    if rejected_required {
        return Merge::Blocked;
    }
    Merge::Clean
}

/// The checks list: policy evaluations and commit statuses normalized into one
/// (`specs/forge-providers.md`). A policy allowed to fail — one that is not blocking —
/// contributes a skipped check, never a failing one.
fn build_checks(evaluations: &Value, statuses: &Value) -> Vec<Check> {
    let mut checks: Vec<Check> = Vec::new();
    for row in evaluations["value"].as_array().into_iter().flatten() {
        let name = row["configuration"]["type"]["displayName"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let blocking = row["configuration"]["isBlocking"].as_bool().unwrap_or(false);
        let status = policy_status(row["status"].as_str().unwrap_or_default(), blocking);
        upsert_latest(&mut checks, Check { name: name.to_string(), status });
    }
    // Statuses arrive newest-first; iterate oldest-first so a re-run replaces its earlier run.
    let rows = statuses["value"].as_array().map(Vec::as_slice).unwrap_or_default();
    for row in rows.iter().rev() {
        let name = row["context"]["name"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let status = commit_status(row["state"].as_str().unwrap_or_default());
        upsert_latest(&mut checks, Check { name: name.to_string(), status });
    }
    checks
}

/// Normalise one policy evaluation status to a [`CheckStatus`]. A non-blocking policy's
/// failure leaves the pull request completable, so it is a warning, never a failing check
/// (`specs/forge-providers.md`).
fn policy_status(status: &str, blocking: bool) -> CheckStatus {
    match status {
        "approved" => CheckStatus::Success,
        "rejected" | "broken" => {
            if blocking {
                CheckStatus::Failure
            } else {
                CheckStatus::Skipped
            }
        }
        "running" => CheckStatus::Running,
        "notApplicable" => CheckStatus::Skipped,
        // queued — evaluation not started.
        _ => CheckStatus::Pending,
    }
}

/// Normalise one commit status state to a [`CheckStatus`].
fn commit_status(state: &str) -> CheckStatus {
    match state {
        "succeeded" => CheckStatus::Success,
        "failed" | "error" => CheckStatus::Failure,
        "notApplicable" => CheckStatus::Skipped,
        // pending / notSet — work not concluded.
        _ => CheckStatus::Pending,
    }
}

/// Whether a one-page read hit its 100-row bound: a continuation token, or a full page.
/// `az devops invoke` never follows continuations, so a full page can hide older rows.
fn one_page_capped(response: &Value) -> bool {
    !response["continuation_token"].is_null()
        || response["value"].as_array().is_some_and(|rows| rows.len() >= crate::forge::SURFACE_CAP)
}

/// The newest 100 comment threads from a threads response, oldest-first, and whether any were
/// dropped. Azure DevOps returns every thread in one page, published order, so the cap is
/// client-side (`specs/forge-host.md`: each surface reads its newest 100 rows). System-only
/// and empty threads drop first, so status churn never spends the surface's slots.
fn newest_comment_threads(threads: &Value) -> (Vec<&Value>, bool) {
    let rows: Vec<&Value> = threads["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|thread| comment_root(thread).is_some())
        .collect();
    let truncated = rows.len() > crate::forge::SURFACE_CAP;
    (crate::forge::newest_capped(rows), truncated)
}

/// The first human-authored comment carrying content — the thread's root — or `None` when the
/// thread is system-only, deleted, or empty and renders no comment. The one definition of
/// "a thread is a comment", shared by the newest-100 cap and the render.
fn comment_root(thread: &Value) -> Option<&Value> {
    thread["comments"].as_array()?.iter().find(|comment| is_comment(comment))
}

/// A comment that renders: human-authored, not deleted, and carrying content. The one
/// predicate behind the root pick and the reply count, so the two can never disagree.
fn is_comment(comment: &Value) -> bool {
    comment["commentType"].as_str() != Some("system")
        && !comment["isDeleted"].as_bool().unwrap_or(false)
        && !comment["content"].as_str().unwrap_or("").trim().is_empty()
}

/// Replies beyond the root: the rendering comments on the thread, less one.
fn reply_count(thread: &Value) -> u32 {
    let comments = thread["comments"]
        .as_array()
        .map_or(0, |comments| comments.iter().filter(|comment| is_comment(comment)).count());
    comments.saturating_sub(1) as u32
}

/// Merge the threads and reviewer votes into one newest-first comment list: PR-level threads
/// are `comment` rows, file-position threads are `finding` rows with the thread's resolved
/// status, and a reviewer vote is a `review` row (`specs/forge-providers.md`). A thread
/// carries no code context, so a finding has no snippet.
fn merge_comments(threads: &[&Value], pr: &Value) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();
    for thread in threads {
        let Some(root) = comment_root(thread) else { continue };
        let author = root["author"]["displayName"].as_str().unwrap_or("").to_string();
        let context = &thread["threadContext"];
        // A file-position thread is a finding; anything else is a plain comment.
        let (kind, anchor, is_resolved) = match context["filePath"].as_str() {
            Some(path) => {
                let path = path.trim_start_matches('/');
                let line = context["rightFileEnd"]["line"]
                    .as_u64()
                    .or_else(|| context["rightFileStart"]["line"].as_u64())
                    .or_else(|| context["leftFileEnd"]["line"].as_u64());
                let anchor = match line {
                    Some(line) => format!("{path}:{line}"),
                    None => path.to_string(),
                };
                let resolved = matches!(
                    thread["status"].as_str(),
                    Some("closed" | "fixed" | "byDesign" | "wontFix")
                );
                (CommentKind::Finding, anchor, resolved)
            }
            None => (CommentKind::Comment, "comment".to_string(), false),
        };
        out.push(Comment {
            kind,
            author_is_bot: is_azure_bot(&root["author"]),
            author,
            anchor,
            body: root["content"].as_str().unwrap_or("").trim().to_string(),
            snippet: None,
            created_at: root["publishedDate"].as_str().unwrap_or("").to_string(),
            is_resolved,
            is_outdated: false,
            reply_count: reply_count(thread),
        });
    }
    for reviewer in pr["reviewers"].as_array().into_iter().flatten() {
        // A container is a required-reviewer group; its rollup vote repeats a member's.
        if reviewer["isContainer"].as_bool().unwrap_or(false) {
            continue;
        }
        let author = reviewer["displayName"].as_str().unwrap_or("").to_string();
        let Some(body) = vote_body(reviewer["vote"].as_i64().unwrap_or(0)) else { continue };
        if author.is_empty() {
            continue;
        }
        let bot = is_azure_bot(reviewer);
        // The vote carries no timestamp, so votes sort after the dated rows in the
        // newest-first list.
        out.push(prose_row(CommentKind::Review, author, bot, body.to_string(), String::new()));
    }
    finish_comments(&mut out);
    out
}

/// The prose one reviewer vote renders as; a zero vote renders nothing.
fn vote_body(vote: i64) -> Option<&'static str> {
    match vote {
        10 => Some("Approved this pull request."),
        5 => Some("Approved this pull request with suggestions."),
        -5 => Some("Is waiting for the author."),
        -10 => Some("Rejected this pull request."),
        _ => None,
    }
}

/// Whether an Azure DevOps identity is a service account: the shared name heuristics, the
/// platform's own service identity, or a build-service account. Azure DevOps carries no bot
/// flag on an identity, so the name and unique name are the only signals.
fn is_azure_bot(identity: &Value) -> bool {
    let display = identity["displayName"].as_str().unwrap_or("");
    let unique = identity["uniqueName"].as_str().unwrap_or("").to_ascii_lowercase();
    crate::forge::is_named_bot(display)
        || display == "Microsoft.VisualStudio.Services.TFS"
        || display.to_ascii_lowercase().contains("build service")
        || unique.contains("build service")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_leading_comment_neither_hides_the_thread_nor_counts_as_a_reply() {
        let thread = json!({"comments": [
            {"commentType": "text", "isDeleted": false, "content": "  ",
             "author": {"displayName": "Editor"}},
            {"commentType": "text", "isDeleted": false, "content": "The real comment.",
             "author": {"displayName": "Author"}}
        ]});
        assert_eq!(comment_root(&thread).unwrap()["content"], "The real comment.");
        assert_eq!(reply_count(&thread), 0);
    }

    #[test]
    fn state_maps_active_and_completed_with_a_closed_fallback() {
        assert_eq!(parse_state("active"), PrState::Open);
        assert_eq!(parse_state("completed"), PrState::Merged);
        // Only active and completed are ever picked, so a missing status is the one
        // reachable fallback, and it must not read as reviewable (`specs/overview.md`).
        assert_eq!(parse_state(""), PrState::Closed);
    }

    #[test]
    fn merge_folds_conflicts_and_rejected_required_policies_only() {
        let none = json!(null);
        assert_eq!(derive_merge(&json!({"mergeStatus": "conflicts"}), &none), Merge::Conflicting);
        // A rejected blocking policy blocks; a rejected optional one does not.
        let rejected = |blocking: bool| {
            json!({"value": [
                {"status": "rejected", "configuration": {"isBlocking": blocking}}
            ]})
        };
        let pr = json!({"mergeStatus": "succeeded"});
        assert_eq!(derive_merge(&pr, &rejected(true)), Merge::Blocked);
        assert_eq!(derive_merge(&pr, &rejected(false)), Merge::Clean);
        // A still-queued merge check folds to clean (`specs/forge-providers.md`).
        assert_eq!(derive_merge(&json!({"mergeStatus": "queued"}), &none), Merge::Clean);
    }

    #[test]
    fn checks_merge_policies_and_statuses_with_rerun_replacement() {
        let evaluations = json!({"value": [
            {"status": "approved", "configuration":
                {"isBlocking": true, "type": {"displayName": "Build"}}},
            {"status": "rejected", "configuration":
                {"isBlocking": false, "type": {"displayName": "Optional lint"}}},
            {"status": "rejected", "configuration":
                {"isBlocking": true, "type": {"displayName": "Required reviewers"}}},
            {"status": "queued", "configuration":
                {"isBlocking": true, "type": {"displayName": "Comments"}}},
        ]});
        // Statuses arrive newest-first; the re-run of `ci/tests` must win over its failure.
        let statuses = json!({"value": [
            {"state": "succeeded", "context": {"name": "ci/tests"}},
            {"state": "failed", "context": {"name": "ci/tests"}},
            {"state": "pending", "context": {"name": "ci/docs"}},
        ]});
        let checks = build_checks(&evaluations, &statuses);
        let by_name: Vec<(&str, CheckStatus)> =
            checks.iter().map(|c| (c.name.as_str(), c.status)).collect();
        assert_eq!(
            by_name,
            vec![
                ("Build", CheckStatus::Success),
                ("Optional lint", CheckStatus::Skipped),
                ("Required reviewers", CheckStatus::Failure),
                ("Comments", CheckStatus::Pending),
                ("ci/docs", CheckStatus::Pending),
                ("ci/tests", CheckStatus::Success),
            ]
        );
    }

    #[test]
    fn association_reduces_a_pull_request_to_its_pick_fields() {
        let node = json!({
            "pullRequestId": 5,
            "status": "completed",
            "sourceRefName": "refs/heads/invBootstrap",
            "lastMergeSourceCommit": {"commitId": "3aae318f"},
            "creationDate": "2026-02-18T04:35:01Z",
            "closedDate": "2026-02-18T04:35:05Z",
        });
        let pr = assoc_pr(&node).unwrap();
        assert_eq!(pr.number, 5);
        assert_eq!(pr.head_oid, "3aae318f");
        assert_eq!(pr.head_ref, "invBootstrap");
        assert_eq!(pr.closed_at, "2026-02-18T04:35:05Z");
        assert_eq!(pr.created_at, "2026-02-18T04:35:01Z");
        // A fork pull request reports the virtual source ref; the branch lives on forkSource.
        // An open pull request has no merge date.
        let node = json!({
            "pullRequestId": 7,
            "status": "active",
            "sourceRefName": "refs/pull/7/source",
            "forkSource": {"name": "refs/heads/fork-feature", "repository": {"id": "b0bf"}},
        });
        let fork = assoc_pr(&node).unwrap();
        assert_eq!(fork.head_ref, "fork-feature");
        assert_eq!(fork.closed_at, "");
    }

    #[test]
    fn threads_map_to_comments_findings_and_votes() {
        let threads_value = json!({"value": [
            // A system thread renders nothing and spends no slot.
            {"comments": [{"commentType": "system", "content": "MerlinBot added 3 reviewers",
                "author": {"displayName": "Microsoft.VisualStudio.Services.TFS"}}]},
            // A PR-level thread is a comment row with its replies counted.
            {"status": "active", "comments": [
                {"commentType": "text", "content": "Looks good overall.",
                 "publishedDate": "2026-02-18T05:00:00Z",
                 "author": {"displayName": "Mark Wilkie"}},
                {"commentType": "text", "content": "Agreed.",
                 "author": {"displayName": "Michael Stuckey"}},
            ]},
            // A file-position thread is a finding with the thread's resolved status.
            {"status": "fixed",
             "threadContext": {"filePath": "/src/main.rs",
                "rightFileStart": {"line": 12}, "rightFileEnd": {"line": 14}},
             "comments": [
                {"commentType": "text", "content": "Off by one.",
                 "publishedDate": "2026-02-18T06:00:00Z",
                 "author": {"displayName": "Project Collection Build Service (org)"}},
            ]},
        ]});
        let (rows, truncated) = newest_comment_threads(&threads_value);
        assert!(!truncated);
        assert_eq!(rows.len(), 2, "the system thread drops");
        let pr = json!({"reviewers": [
            {"displayName": "Mark Wilkie", "vote": 10},
            {"displayName": "Waiting Reviewer", "vote": -5},
            {"displayName": "Quiet Reviewer", "vote": 0},
            {"displayName": "Leads", "vote": 10, "isContainer": true},
        ]});
        let comments = merge_comments(&rows, &pr);
        let finding = comments.iter().find(|c| c.kind == CommentKind::Finding).unwrap();
        assert_eq!(finding.anchor, "src/main.rs:14");
        assert!(finding.is_resolved);
        assert!(finding.author_is_bot, "a build-service identity is a bot");
        assert!(finding.snippet.is_none(), "a thread carries no code context");
        let comment = comments.iter().find(|c| c.kind == CommentKind::Comment).unwrap();
        assert_eq!(comment.author, "Mark Wilkie");
        assert_eq!(comment.reply_count, 1);
        let votes: Vec<&Comment> =
            comments.iter().filter(|c| c.kind == CommentKind::Review).collect();
        assert_eq!(votes.len(), 2, "a zero vote and a container render nothing");
        assert_eq!(votes[0].body, "Approved this pull request.");
        assert_eq!(votes[1].body, "Is waiting for the author.");
    }

    #[test]
    fn an_enumeration_node_admits_by_source_branch_name() {
        let node = json!({
            "pullRequestId": 5,
            "status": "completed",
            "sourceRefName": "refs/heads/feature",
            "lastMergeSourceCommit": {"commitId": "3aae318f"},
            "lastMergeCommit": {"commitId": "af56d96f"},
        });
        let names = vec!["feature".to_string()];
        let admitted = branch_admitted(Path::new("."), &node, &names, None).unwrap();
        assert_eq!(admitted.unwrap().number, 5);
        // A different branch name proves nothing.
        let other = vec!["other".to_string()];
        assert!(branch_admitted(Path::new("."), &node, &other, None).unwrap().is_none());
        // A fork node has no provable source repository, so with no pinned HEAD to
        // contain its tip it never admits.
        let fork = json!({
            "pullRequestId": 7,
            "status": "active",
            "sourceRefName": "refs/pull/7/source",
            "lastMergeSourceCommit": {"commitId": "3aae318f"},
            "forkSource": {"name": "refs/heads/feature", "repository": {"id": "b0bf"}},
        });
        assert!(branch_admitted(Path::new("."), &fork, &names, None).unwrap().is_none());
    }

    #[test]
    fn thread_cap_keeps_the_newest_hundred_and_reports_the_drop() {
        let mut rows = Vec::new();
        for i in 0..120 {
            rows.push(json!({"comments": [{
                "commentType": "text", "content": format!("comment {i}"),
                "author": {"displayName": "A"}}]}));
        }
        let threads_value = json!({"value": rows});
        let (kept, truncated) = newest_comment_threads(&threads_value);
        assert!(truncated);
        assert_eq!(kept.len(), 100);
        let first = comment_root(kept[0]).unwrap();
        assert_eq!(first["content"], "comment 20", "the oldest rows drop first");
    }

    #[test]
    fn a_full_checks_page_or_a_continuation_token_reports_the_surface_capped() {
        assert!(!one_page_capped(&json!({"count": 2, "value": [1, 2]})));
        assert!(one_page_capped(&json!({"continuation_token": "abc", "value": []})));
        let full: Vec<u64> = (0..100).collect();
        assert!(one_page_capped(&json!({"value": full})));
        assert!(!one_page_capped(&Value::Null));
    }

    #[test]
    fn project_guid_reads_from_the_enumeration_nodes_project() {
        let enumerated = json!({"repository": {"project": {"id": "37b9070b"}}});
        assert_eq!(project_guid_of(&enumerated).as_deref(), Some("37b9070b"));
        assert_eq!(project_guid_of(&json!({})), None);
    }

    #[test]
    fn failures_classify_from_the_cli_wording() {
        // Captured live from `az` 2.88.0 / azure-devops 1.0.6.
        assert!(matches!(
            classify_failure(
                "ERROR: The command requires the extension azure-devops. It will be installed first."
            ),
            AzError::NoExtension
        ));
        assert!(matches!(
            classify_failure(
                "ERROR: Before you can run Azure DevOps commands, you need to run the login command(az login if using AAD/MSA identity else az devops login if using PAT token) to setup credentials."
            ),
            AzError::NotAuthed
        ));
        assert!(matches!(
            classify_failure(
                "ERROR: TF400813: The user 'aaaa' is not authorized to access this resource."
            ),
            AzError::NotAuthed
        ));
        assert!(matches!(
            classify_failure(
                "ERROR: The requested resource requires user authentication: https://dev.azure.com/x/_apis/git/pullRequests/5"
            ),
            AzError::NotAuthed
        ));
        assert!(matches!(
            classify_failure("ERROR: TF401180: The requested pull request was not found."),
            AzError::Unavailable(_)
        ));
        assert!(matches!(
            classify_failure(
                "ERROR: TF401019: The Git repository with name or identifier x does not exist."
            ),
            AzError::Unavailable(_)
        ));
        assert!(matches!(classify_failure("something exploded"), AzError::Other(_)));
    }

    #[test]
    fn organization_url_derives_from_the_target_host_form() {
        let target = |host: &str| {
            crate::git::RepoTarget::with_path(
                crate::git::Forge::AzureDevOps,
                host,
                &["org", "project", "repo"],
            )
            .unwrap()
        };
        assert_eq!(organization_url(&target("dev.azure.com")), "https://dev.azure.com/org");
        assert_eq!(
            organization_url(&target("org.visualstudio.com")),
            "https://org.visualstudio.com"
        );
        assert_eq!(organization_url(&target("tfs.corp.example")), "https://tfs.corp.example/org");
    }
}
