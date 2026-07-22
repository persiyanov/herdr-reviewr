//! The GitLab backend: association through REST commit lookups, detail through one GraphQL
//! call, all via `glab`.
//!
//! See `specs/forge-host.md`. Everything GitLab-shaped lives here — the REST paths, the
//! GraphQL detail query, the response JSON parsing, and `glab`'s stderr classification. The
//! forge-agnostic resolution logic (gates, picks, sync, the snapshot model) stays in `super`.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::{
    AssocPr, Association, Check, CheckStatus, CliError, Comment, CommentKind, FetchTarget, Merge,
    PrSnapshot, PrState, Sync,
};
use crate::git::Forge;

/// Run explicitly targeted `glab` arguments in `repo` and return stdout or a classified failure.
fn glab(
    repo: &Path,
    host: &str,
    args: &[&str],
    cancelled: &AtomicBool,
) -> Result<String, CliError> {
    super::run_cli("glab", repo, args, cancelled).map_err(|failure| match failure {
        super::CliFailure::Missing => CliError::Missing(Forge::GitLab),
        super::CliFailure::Other(message) => CliError::Other(message),
        super::CliFailure::Stderr(stderr) => classify_failure(&stderr, host),
    })
}

/// Map a failed `glab`'s stderr to a degraded state by its wording — `glab` has no stable
/// exit codes for these. An unrecognised failure is `Other` → a transient `Error` view.
fn classify_failure(stderr: &str, host: &str) -> CliError {
    let s = stderr.to_lowercase();
    if s.contains("glab auth login") || s.contains("unauthorized") || s.contains("http 401") {
        CliError::NotAuthed(Forge::GitLab, host.to_owned())
    } else {
        CliError::Other(stderr.trim().to_string())
    }
}

/// Run one REST GET through `glab api`, hostname-pinned, and parse the JSON body.
fn rest(repo: &Path, host: &str, path: &str, cancelled: &AtomicBool) -> Result<Value, CliError> {
    let out = glab(repo, host, &["api", path, "--hostname", host], cancelled)?;
    serde_json::from_str(&out).map_err(|e| CliError::Other(e.to_string()))
}

/// Run one REST GET where a 404 is a clean absence — an unknown commit or a hidden project —
/// never a failure (`specs/forge-host.md`).
fn rest_absent_ok(
    repo: &Path,
    host: &str,
    path: &str,
    cancelled: &AtomicBool,
) -> Result<Option<Value>, CliError> {
    match rest(repo, host, path, cancelled) {
        Ok(v) => Ok(Some(v)),
        Err(CliError::Other(message)) if message.contains("HTTP 404") => Ok(None),
        Err(error) => Err(error),
    }
}

/// Run one GraphQL query through `glab api graphql`, hostname-pinned. Variables ride as
/// `-f` fields — `glab` folds every non-`query` field into the GraphQL variables.
fn graphql(
    repo: &Path,
    host: &str,
    query: &str,
    vars: &[(String, String)],
    cancelled: &AtomicBool,
) -> Result<Value, CliError> {
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
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = glab(repo, host, &arg_refs, cancelled)?;
    serde_json::from_str(&out).map_err(|e| CliError::Other(e.to_string()))
}

/// Percent-encode one URL path or query component (the RFC 3986 unreserved set is kept).
/// A project's full path rides in the URL, so its `/` separators must encode.
fn percent_encode(component: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(component.len());
    for byte in component.bytes() {
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

/// One REST MR row → the pick-relevant fields. GitLab's MR number is its `iid`; `sha` is
/// the MR head.
fn mr_of(node: &Value) -> Option<AssocPr> {
    Some(AssocPr {
        number: node["iid"].as_u64()?,
        head_oid: node["sha"].as_str().unwrap_or_default().to_string(),
        head_ref: node["source_branch"].as_str().unwrap_or_default().to_string(),
        merged_at: node["merged_at"].as_str().unwrap_or_default().to_string(),
        created_at: node["created_at"].as_str().unwrap_or_default().to_string(),
    })
}

/// Which admission class an OID lookup belongs to (`specs/forge-host.md` "Resolution").
#[derive(Clone, Copy)]
enum OidClass<'a> {
    /// A publication point: containment admits open and merged MRs.
    Point,
    /// An absorbed candidate: only a merged MR whose head is exactly an absorbed commit.
    Absorbed(&'a [String]),
    /// The pinned `HEAD`: only an MR whose head is exactly this commit.
    Head(&'a str),
}

/// Admit one commit lookup's MR rows into the association under its class's rule. Only MRs
/// based on the target project count — the numeric id survives renames and transfers.
fn admit_commit_mrs(assoc: &mut Association, rows: &Value, target_id: u64, class: OidClass<'_>) {
    for node in rows.as_array().into_iter().flatten() {
        if node["target_project_id"].as_u64() != Some(target_id) {
            continue;
        }
        let Some(mr) = mr_of(node) else { continue };
        if let OidClass::Absorbed(absorbed) = class {
            // An absorbed commit is base history, which proves nothing by containment.
            // Only the exact parked epilogue admits: a merged MR whose head is an
            // absorbed commit itself.
            let exact = absorbed.iter().any(|oid| oid == &mr.head_oid);
            if exact && node["state"].as_str() == Some("merged") {
                super::push_unique(&mut assoc.merged, mr);
            }
            continue;
        }
        if let OidClass::Head(head) = class
            && mr.head_oid != head
        {
            // The pinned HEAD is not published, so containment proves nothing. Only
            // exact identity admits: the worktree is parked on the MR's own head.
            continue;
        }
        match node["state"].as_str() {
            Some("opened") => super::push_unique(&mut assoc.open, mr),
            Some("merged") => super::push_unique(&mut assoc.merged, mr),
            _ => {}
        }
    }
}

/// Ask GitLab which MRs contain each publication point — one REST commit lookup per
/// nominated OID against the `source` project, with the closed-unmerged name lookup
/// against the target riding along (`specs/forge-host.md`). GitLab's GraphQL schema has
/// no commit→MRs field, so the association reads REST where GitHub reads one aliased
/// GraphQL call.
pub(super) fn associate_points(
    target: &FetchTarget<'_>,
    source: &crate::git::RepoTarget,
    points: &[crate::git::PublicationPoint],
    absorbed: &[String],
    head: Option<&str>,
) -> Result<Association, CliError> {
    // The target's numeric project id is the rename-proof base filter, one read per fetch.
    // A hidden or vanished target proves nothing — the empty association degrades to the
    // calm empty state, exactly like GitHub's null target repository.
    let target_path = percent_encode(&format!("{}/{}", target.owner, target.name));
    let project = rest_absent_ok(
        target.repo,
        target.host,
        &format!("projects/{target_path}"),
        target.cancelled,
    )?;
    let Some(target_id) = project.as_ref().and_then(|p| p["id"].as_u64()) else {
        return Ok(Association::default());
    };
    let source_path = percent_encode(&format!("{}/{}", source.owner(), source.name()));
    let mut assoc = Association::default();
    for (i, oid) in points
        .iter()
        .map(|p| p.oid.as_str())
        .chain(absorbed.iter().map(String::as_str))
        .chain(head)
        .enumerate()
    {
        let class = if i < points.len() {
            OidClass::Point
        } else if i < points.len() + absorbed.len() {
            OidClass::Absorbed(absorbed)
        } else {
            OidClass::Head(oid)
        };
        let path =
            format!("projects/{source_path}/repository/commits/{oid}/merge_requests?per_page=100");
        let Some(rows) = rest_absent_ok(target.repo, target.host, &path, target.cancelled)? else {
            // The commit is unknown to the source project — stale local refs, not failure.
            continue;
        };
        admit_commit_mrs(&mut assoc, &rows, target_id, class);
    }
    // The closed-unmerged epilogue: one lookup per `(point, tip name)` pair against the
    // target, capped at 8 pairs like the GitHub aliases. Only an exact head match admits.
    let pairs: Vec<(usize, &str)> = points
        .iter()
        .enumerate()
        .flat_map(|(i, point)| point.names.iter().map(move |name| (i, name.as_str())))
        .take(8)
        .collect();
    for (i, name) in pairs {
        let path = format!(
            "projects/{target_path}/merge_requests?state=closed&source_branch={}\
             &order_by=created_at&sort=desc&per_page=10",
            percent_encode(name)
        );
        let Some(rows) = rest_absent_ok(target.repo, target.host, &path, target.cancelled)? else {
            continue;
        };
        for node in rows.as_array().into_iter().flatten() {
            let Some(mr) = mr_of(node) else { continue };
            if mr.head_oid != points[i].oid {
                continue;
            }
            super::push_unique(&mut assoc.closed, mr);
        }
    }
    Ok(assoc)
}

/// Project one MR directly: identity, merge status, the head pipeline's jobs, and every
/// discussion's root note with its reply count. Discussions carry both the MR-level notes
/// and the inline findings; `first:100` caps each surface and `hasNextPage` flags a fuller
/// one (`specs/forge-host.md`).
const DETAIL_QUERY: &str = "query($path:ID!,$iid:String!){project(fullPath:$path){\
     mergeRequest(iid:$iid){\
     iid title webUrl description state draft sourceBranch targetBranch \
     diffHeadSha conflicts detailedMergeStatus \
     sourceProject{id} targetProject{id} \
     headPipeline{jobs(retried:false,first:100){pageInfo{hasNextPage} nodes{name status}}} \
     discussions(first:100){pageInfo{hasNextPage} nodes{resolved \
     notes(first:1){count nodes{system body createdAt author{username bot} \
     position{filePath newLine oldLine}}}}}}}}";

/// One MR's state in a single GraphQL call. Returns the MR node and its head OID, or
/// `None` when the MR vanished between the association and this read.
pub(super) fn pr_detail(
    target: &FetchTarget<'_>,
    number: u64,
) -> Result<Option<(Value, String)>, CliError> {
    let vars = vec![
        ("path".to_string(), format!("{}/{}", target.owner, target.name)),
        ("iid".to_string(), number.to_string()),
    ];
    let mut v = graphql(target.repo, target.host, DETAIL_QUERY, &vars, target.cancelled)?;
    let node = v["data"]["project"]["mergeRequest"].take();
    if node.is_null() {
        return Ok(None);
    }
    let head = node["diffHeadSha"].as_str().unwrap_or_default().to_string();
    Ok(Some((node, head)))
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// Assemble the snapshot from the GraphQL MR node and the computed `sync`.
pub(super) fn build_snapshot(node: &Value, sync: Sync) -> PrSnapshot {
    let jobs = &node["headPipeline"]["jobs"];
    let more = |conn: &Value| conn["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false);
    let truncated = more(jobs) || more(&node["discussions"]);
    let source_project = node["sourceProject"]["id"].as_str().unwrap_or_default();
    let target_project = node["targetProject"]["id"].as_str().unwrap_or_default();
    PrSnapshot {
        // GraphQL returns `iid` as a string.
        number: node["iid"].as_str().and_then(|iid| iid.parse().ok()).unwrap_or_default(),
        title: node["title"].as_str().unwrap_or_default().to_string(),
        url: node["webUrl"].as_str().unwrap_or_default().to_string(),
        body: node["description"].as_str().unwrap_or_default().to_string(),
        state: parse_state(node["state"].as_str().unwrap_or("opened")),
        is_draft: node["draft"].as_bool().unwrap_or(false),
        head_ref: node["sourceBranch"].as_str().unwrap_or_default().to_string(),
        // A fork MR's source project differs from the target; a deleted source counts too.
        head_is_fork: source_project != target_project,
        base_ref: node["targetBranch"].as_str().unwrap_or_default().to_string(),
        merge: derive_merge(
            node["conflicts"].as_bool().unwrap_or(false),
            node["detailedMergeStatus"].as_str(),
        ),
        sync,
        checks: normalize_checks(&jobs["nodes"]),
        comments: merge_comments(&node["discussions"]["nodes"]),
        truncated,
    }
}

fn parse_state(s: &str) -> PrState {
    match s {
        "merged" => PrState::Merged,
        "closed" => PrState::Closed,
        _ => PrState::Open,
    }
}

/// Fold GitLab's `conflicts` and `detailedMergeStatus` into a [`Merge`]. Only the
/// actionable blockers surface: conflicts, and the approval/discussion/policy gates a
/// reviewer acts on. Everything else — mergeable, still-checking, a running pipeline,
/// need-rebase — folds into `Clean` (shows nothing), mirroring the GitHub fold.
fn derive_merge(conflicts: bool, status: Option<&str>) -> Merge {
    if conflicts || status == Some("CONFLICT") {
        return Merge::Conflicting;
    }
    match status {
        Some(
            "BLOCKED_STATUS"
            | "DISCUSSIONS_NOT_RESOLVED"
            | "EXTERNAL_STATUS_CHECKS"
            | "JIRA_ASSOCIATION"
            | "NOT_APPROVED"
            | "POLICIES_DENIED"
            | "REQUESTED_CHANGES",
        ) => Merge::Blocked,
        _ => Merge::Clean,
    }
}

/// The latest job per name from the head pipeline. `retried:false` already excludes
/// superseded runs; a duplicate name still resolves to the last row.
fn normalize_checks(jobs: &Value) -> Vec<Check> {
    let mut out: Vec<Check> = Vec::new();
    for node in jobs.as_array().into_iter().flatten() {
        let name = node["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let status = check_status(node["status"].as_str().unwrap_or(""));
        if let Some(slot) = out.iter_mut().find(|c| c.name == name) {
            *slot = Check { name, status };
        } else {
            out.push(Check { name, status });
        }
    }
    out
}

/// One `CiJobStatus` → a [`CheckStatus`]. A manual gate is a skip, not a failure; a
/// canceled job reads as failed — something needs attention, like GitHub's cancelled
/// conclusion.
fn check_status(status: &str) -> CheckStatus {
    match status {
        "SUCCESS" => CheckStatus::Success,
        "FAILED" | "CANCELED" | "CANCELING" => CheckStatus::Failure,
        "RUNNING" => CheckStatus::Running,
        "SKIPPED" | "MANUAL" => CheckStatus::Skipped,
        // CREATED / PENDING / PREPARING / SCHEDULED / WAITING_* — queued work.
        _ => CheckStatus::Pending,
    }
}

/// One newest-first list from the discussion roots: a positioned root is an inline
/// `finding`, an unpositioned one a plain `comment`. System notes (assignments, review
/// requests) are noise, not review input. GitLab has no GitHub-style review body, so the
/// `review` kind never occurs here. A bot's MR-level posts collapse to its latest, like on
/// GitHub; the bot flag is GitLab's own account marker, which covers service-account bots
/// whatever their username.
fn merge_comments(discussions: &Value) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();
    for d in discussions.as_array().into_iter().flatten() {
        let root = &d["notes"]["nodes"][0];
        if root["system"].as_bool().unwrap_or(false) {
            continue;
        }
        let body = root["body"].as_str().unwrap_or("").trim().to_string();
        if body.is_empty() {
            continue;
        }
        let author = &root["author"];
        let login = author["username"].as_str().unwrap_or("").to_string();
        let author_is_bot = author["bot"].as_bool().unwrap_or(false);
        let created_at = root["createdAt"].as_str().unwrap_or("").to_string();
        let position = &root["position"];
        if let Some(path) = position["filePath"].as_str() {
            let line = position["newLine"].as_u64().or_else(|| position["oldLine"].as_u64());
            out.push(Comment {
                kind: CommentKind::Finding,
                author_is_bot,
                author: login,
                anchor: match line {
                    Some(line) => format!("{path}:{line}"),
                    None => path.to_string(),
                },
                body,
                // GitLab returns no diff hunk with a note.
                snippet: None,
                created_at,
                is_resolved: d["resolved"].as_bool().unwrap_or(false),
                // GitLab exposes no outdated flag on a discussion; never guessed.
                is_outdated: false,
                reply_count: d["notes"]["count"].as_u64().unwrap_or(1).saturating_sub(1) as u32,
            });
        } else {
            out.push(Comment {
                kind: CommentKind::Comment,
                author_is_bot,
                author: login,
                anchor: "comment".to_string(),
                body,
                snippet: None,
                created_at,
                is_resolved: false,
                is_outdated: false,
                reply_count: 0,
            });
        }
    }
    super::dedup_bot_prose(&mut out);
    // Newest first: ISO-8601 `…Z` strings sort lexically in chronological order.
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_surfaces_only_conflicts_and_reviewer_actionable_blockers() {
        assert_eq!(derive_merge(true, Some("MERGEABLE")), Merge::Conflicting);
        assert_eq!(derive_merge(false, Some("CONFLICT")), Merge::Conflicting);
        assert_eq!(derive_merge(false, Some("NOT_APPROVED")), Merge::Blocked);
        assert_eq!(derive_merge(false, Some("DISCUSSIONS_NOT_RESOLVED")), Merge::Blocked);
        assert_eq!(derive_merge(false, Some("POLICIES_DENIED")), Merge::Blocked);
        // Everything non-actionable folds into Clean: mergeable, checking, CI, rebase.
        assert_eq!(derive_merge(false, Some("MERGEABLE")), Merge::Clean);
        assert_eq!(derive_merge(false, Some("CHECKING")), Merge::Clean);
        assert_eq!(derive_merge(false, Some("CI_STILL_RUNNING")), Merge::Clean);
        assert_eq!(derive_merge(false, Some("NEED_REBASE")), Merge::Clean);
        assert_eq!(derive_merge(false, Some("NOT_OPEN")), Merge::Clean);
        assert_eq!(derive_merge(false, None), Merge::Clean);
    }

    #[test]
    fn parse_state_maps_the_gitlab_lifecycles() {
        assert_eq!(parse_state("merged"), PrState::Merged);
        assert_eq!(parse_state("closed"), PrState::Closed);
        assert_eq!(parse_state("opened"), PrState::Open);
        assert_eq!(parse_state("locked"), PrState::Open); // transient mid-merge → live
    }

    #[test]
    fn job_statuses_fold_to_check_statuses() {
        let jobs = serde_json::json!([
            {"name": "tests", "status": "SUCCESS"},
            {"name": "build", "status": "RUNNING"},
            {"name": "deploy", "status": "MANUAL"},
            {"name": "lint", "status": "SKIPPED"},
            {"name": "e2e", "status": "FAILED"},
            {"name": "scan", "status": "CANCELED"},
            {"name": "later", "status": "SCHEDULED"}
        ]);
        let checks = normalize_checks(&jobs);
        let status = |name: &str| checks.iter().find(|c| c.name == name).unwrap().status;
        assert_eq!(status("tests"), CheckStatus::Success);
        assert_eq!(status("build"), CheckStatus::Running);
        // A manual gate neither fails nor blocks the rollup.
        assert_eq!(status("deploy"), CheckStatus::Skipped);
        assert_eq!(status("lint"), CheckStatus::Skipped);
        assert_eq!(status("e2e"), CheckStatus::Failure);
        assert_eq!(status("scan"), CheckStatus::Failure);
        assert_eq!(status("later"), CheckStatus::Pending);
    }

    fn detail_node() -> Value {
        serde_json::json!({
            "iid": "81", "title": "t", "webUrl": "u", "description": "## Summary",
            "state": "opened", "draft": true, "sourceBranch": "feat/x", "targetBranch": "main",
            "diffHeadSha": "abc", "conflicts": false, "detailedMergeStatus": "MERGEABLE",
            "sourceProject": {"id": "gid://gitlab/Project/1"},
            "targetProject": {"id": "gid://gitlab/Project/1"},
            "headPipeline": {"jobs": {"pageInfo": {"hasNextPage": false}, "nodes": []}},
            "discussions": {"pageInfo": {"hasNextPage": false}, "nodes": []}
        })
    }

    #[test]
    fn snapshot_parses_the_string_iid_and_carries_identity() {
        let s = build_snapshot(&detail_node(), Sync::InSync);
        assert_eq!(s.number, 81);
        assert_eq!(s.body, "## Summary");
        assert_eq!(s.state, PrState::Open);
        assert!(s.is_draft);
        assert_eq!(s.head_ref, "feat/x");
        assert_eq!(s.base_ref, "main");
        assert!(!s.head_is_fork);
        assert!(!s.truncated);
        // Absent fields default rather than fail — a mid-rollout API response degrades soft.
        let bare = serde_json::json!({"iid": "5"});
        let s = build_snapshot(&bare, Sync::InSync);
        assert_eq!(s.number, 5);
        assert_eq!(s.head_ref, "");
    }

    #[test]
    fn a_differing_or_deleted_source_project_marks_the_fork() {
        let mut fork = detail_node();
        fork["sourceProject"]["id"] = serde_json::json!("gid://gitlab/Project/2");
        assert!(build_snapshot(&fork, Sync::InSync).head_is_fork);
        let mut deleted = detail_node();
        deleted["sourceProject"] = serde_json::Value::Null;
        assert!(build_snapshot(&deleted, Sync::InSync).head_is_fork);
    }

    #[test]
    fn truncated_flips_when_jobs_or_discussions_have_a_next_page() {
        let mut jobs_more = detail_node();
        jobs_more["headPipeline"]["jobs"]["pageInfo"]["hasNextPage"] = serde_json::json!(true);
        assert!(build_snapshot(&jobs_more, Sync::InSync).truncated);
        let mut discussions_more = detail_node();
        discussions_more["discussions"]["pageInfo"]["hasNextPage"] = serde_json::json!(true);
        assert!(build_snapshot(&discussions_more, Sync::InSync).truncated);
    }

    #[test]
    fn discussions_split_into_findings_and_comments_and_skip_system_noise() {
        let discussions = serde_json::json!([
            {"resolved": false, "notes": {"count": 1, "nodes": [
                {"system": true, "body": "assigned to @sean", "createdAt": "2026-07-21T10:00:00Z",
                 "author": {"username": "sean", "bot": false}, "position": null}]}},
            {"resolved": true, "notes": {"count": 3, "nodes": [
                {"system": false, "body": "off-by-one here", "createdAt": "2026-07-21T11:00:00Z",
                 "author": {"username": "reviewer", "bot": false},
                 "position": {"filePath": "src/a.rs", "newLine": 10, "oldLine": null}}]}},
            {"resolved": false, "notes": {"count": 1, "nodes": [
                {"system": false, "body": "please add a changelog entry", "createdAt": "2026-07-21T12:00:00Z",
                 "author": {"username": "reviewer", "bot": false}, "position": null}]}}
        ]);
        let cs = merge_comments(&discussions);
        assert_eq!(cs.len(), 2, "the system note is dropped");
        // Newest first across both kinds.
        assert_eq!(cs[0].kind, CommentKind::Comment);
        assert_eq!(cs[0].anchor, "comment");
        let f = &cs[1];
        assert_eq!(f.kind, CommentKind::Finding);
        assert_eq!(f.anchor, "src/a.rs:10");
        assert!(f.is_resolved);
        assert!(!f.is_outdated, "GitLab has no outdated flag; never guessed");
        assert_eq!(f.reply_count, 2);
        assert_eq!(f.snippet, None, "GitLab returns no diff hunk");
    }

    #[test]
    fn a_service_accounts_prose_collapses_by_the_bot_flag_not_its_name() {
        // GitLab bots carry no `[bot]` suffix — renovate runs as `group_…_bot_…` — so the
        // account's own `bot` marker drives the collapse.
        let note = |body: &str, at: &str| {
            serde_json::json!({"resolved": false, "notes": {"count": 1, "nodes": [
                {"system": false, "body": body, "createdAt": at,
                 "author": {"username": "group_17406_bot_1", "bot": true}, "position": null}]}})
        };
        let discussions = serde_json::json!([
            note("old status", "2026-07-21T09:00:00Z"),
            note("new status", "2026-07-21T10:00:00Z")
        ]);
        let cs = merge_comments(&discussions);
        assert_eq!(cs.len(), 1, "only the bot's latest MR-level post survives");
        assert_eq!(cs[0].body, "new status");
        assert!(cs[0].author_is_bot);
    }

    #[test]
    fn commit_rows_admit_by_class_and_filter_on_the_target_project() {
        let row = |iid: u64, state: &str, sha: &str, project: u64| {
            serde_json::json!({"iid": iid, "state": state, "sha": sha,
                "source_branch": "feat", "merged_at": null, "created_at": "2026-07-01T00:00:00Z",
                "target_project_id": project})
        };
        let rows = serde_json::json!([
            row(7, "opened", "abc", 1),
            row(8, "merged", "def", 1),
            row(9, "opened", "zzz", 2), // based elsewhere → dropped
            row(7, "opened", "abc", 1)  // duplicate → collapses
        ]);
        let mut assoc = Association::default();
        admit_commit_mrs(&mut assoc, &rows, 1, OidClass::Point);
        assert_eq!(assoc.open.iter().map(|p| p.number).collect::<Vec<_>>(), [7]);
        assert_eq!(assoc.merged.iter().map(|p| p.number).collect::<Vec<_>>(), [8]);

        // The absorbed class admits only a merged MR whose head IS the absorbed commit.
        let rows = serde_json::json!([
            row(10, "merged", "parked", 1),
            row(11, "merged", "other", 1),
            row(12, "opened", "parked", 1)
        ]);
        let absorbed = vec!["parked".to_string()];
        let mut assoc = Association::default();
        admit_commit_mrs(&mut assoc, &rows, 1, OidClass::Absorbed(&absorbed));
        assert_eq!(assoc.merged.iter().map(|p| p.number).collect::<Vec<_>>(), [10]);
        assert!(assoc.open.is_empty());

        // The head class admits only an exact head match, open or merged.
        let rows = serde_json::json!([
            row(13, "merged", "tip", 1),
            row(14, "opened", "tip", 1),
            row(15, "opened", "other", 1)
        ]);
        let mut assoc = Association::default();
        admit_commit_mrs(&mut assoc, &rows, 1, OidClass::Head("tip"));
        assert_eq!(assoc.open.iter().map(|p| p.number).collect::<Vec<_>>(), [14]);
        assert_eq!(assoc.merged.iter().map(|p| p.number).collect::<Vec<_>>(), [13]);
    }

    #[test]
    fn nested_namespaces_and_branch_names_percent_encode() {
        assert_eq!(percent_encode("group/sub/repo"), "group%2Fsub%2Frepo");
        assert_eq!(percent_encode("feat/kms-signing"), "feat%2Fkms-signing");
        assert_eq!(percent_encode("release-1.0_x~y"), "release-1.0_x~y");
        assert_eq!(percent_encode("a b#c"), "a%20b%23c");
    }

    #[test]
    fn glab_failure_classifies_by_stderr_wording() {
        assert_eq!(
            classify_failure(
                "To get started with GitLab CLI, please run: glab auth login",
                "gitlab.example.test"
            ),
            CliError::NotAuthed(Forge::GitLab, "gitlab.example.test".to_string())
        );
        assert_eq!(
            classify_failure("glab: 401 Unauthorized (HTTP 401)", "gitlab.com"),
            CliError::NotAuthed(Forge::GitLab, "gitlab.com".to_string())
        );
        assert_eq!(
            classify_failure("HTTP 500 something", "gitlab.com"),
            CliError::Other("HTTP 500 something".into())
        );
    }
}
