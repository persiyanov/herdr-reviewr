//! Read-only GitLab access via `glab`: the merge request's identity, state, checks, and
//! comments. See `specs/forge-host.md`. Reads its canonical target through explicitly hosted
//! `glab api` REST v4 calls. It never posts, resolves, re-runs, merges, or otherwise writes to
//! GitLab.

use serde_json::Value;

use super::{
    Check, CheckStatus, Comment, CommentKind, FetchTarget, Merge, PrFetchInput, PrSnapshot,
    PrState, PrView, Sync,
};

/// Read GitLab for one already-derived input, dispatched from [`super::backend_fetch`]. The
/// snapshot-or-empty `PrView` variants only (`Pr` / `NoPr` / `Ambiguous`) — the core handles
/// origin and candidate pre-checks before dispatch.
pub(crate) fn fetch(
    target: &FetchTarget<'_>,
    input: &PrFetchInput,
) -> Result<PrView, super::ForgeError> {
    fetch_inner(target, input).map_err(Into::into)
}

fn fetch_inner(target: &FetchTarget<'_>, input: &PrFetchInput) -> Result<PrView, GlError> {
    // Resolve the open MR across all candidates, one REST call per candidate — the v4 API has
    // no aliased-batch equivalent to GitHub's GraphQL query.
    let open = resolve_open(target, &input.candidates)?;
    let iid = match super::select_open(&open, input.head_oid.as_deref()) {
        super::Pick::One(n) => n,
        super::Pick::Ambiguous(count) => return Ok(PrView::Ambiguous(count)),
        super::Pick::None => {
            // No open MR anywhere: fall back to the newest-created merged/closed MR.
            let hist = resolve_historical(target, &input.candidates)?;
            match super::select_historical(&hist) {
                Some(n) => n,
                None => return Ok(PrView::NoPr(input.candidates.clone())),
            }
        }
    };
    let detail = mr_detail(target, iid)?;
    if detail.is_null() {
        return Ok(PrView::NoPr(input.candidates.clone()));
    }
    // Sync compares the fetch's pinned HEAD to the MR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's MR with another branch's count.
    let mr_sha = detail["sha"].as_str().unwrap_or_default();
    let sync = match input.head_oid.as_deref() {
        Some(pin) if !mr_sha.is_empty() => super::derive_sync(
            crate::git::ahead_behind_oids(target.repo, pin, mr_sha)
                .map_err(|e| GlError::Other(e.0))?,
        ),
        _ => Sync::Unknown,
    };
    let (checks, checks_truncated) = match detail["head_pipeline"]["id"].as_u64() {
        Some(pipeline_id) => fetch_checks(target, pipeline_id)?,
        None => (Vec::new(), false),
    };
    let (comments, comments_truncated) = fetch_comments(target, iid)?;
    let truncated = checks_truncated || comments_truncated;
    Ok(PrView::Pr(Box::new(build_snapshot(&detail, &checks, comments, sync, truncated))))
}

/// Run explicitly targeted `glab` arguments in `repo` and return stdout or a classified failure.
fn glab(
    repo: &std::path::Path,
    host: &str,
    args: &[&str],
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<String, GlError> {
    match super::proc::run_tool("glab", repo, args, None, cancelled) {
        Ok(stdout) => Ok(stdout),
        Err(super::proc::RunFail::NotFound) => Err(GlError::NoGlab),
        Err(super::proc::RunFail::Cancelled) => Err(GlError::Other("request cancelled".to_string())),
        Err(super::proc::RunFail::Failed { stderr }) => Err(classify_failure(&stderr, host)),
        Err(super::proc::RunFail::Io(message)) => Err(GlError::Other(message)),
    }
}

/// Map a failed `glab`'s stderr to a degraded state by its wording — `glab` has no stable exit
/// codes for these. An unrecognised failure is `Other` → a transient `Error` view.
fn classify_failure(stderr: &str, host: &str) -> GlError {
    let s = stderr.to_lowercase();
    if s.contains("not authenticated") || s.contains("glab auth login") || s.contains("401") {
        GlError::NotAuthed(host.to_owned())
    } else {
        GlError::Other(stderr.trim().to_string())
    }
}

/// A classified `glab` failure, mapped to a [`super::ForgeError`].
#[derive(Debug, PartialEq, Eq)]
enum GlError {
    NoGlab,
    NotAuthed(String),
    Other(String),
}

impl From<GlError> for super::ForgeError {
    fn from(e: GlError) -> Self {
        match e {
            GlError::NoGlab => super::ForgeError::NoCli("glab"),
            GlError::NotAuthed(host) => {
                super::ForgeError::NotAuthed { forge: crate::git::Forge::GitLab, host }
            }
            GlError::Other(m) => super::ForgeError::Other(m),
        }
    }
}

/// The percent-encoded `owner/name` project path shared by every REST call for this target.
fn proj(target: &FetchTarget<'_>) -> String {
    enc(&format!("{}/{}", target.owner, target.name))
}

/// Run one `glab api --hostname <host> <path>` call and parse its JSON body.
fn api_get(target: &FetchTarget<'_>, path: &str) -> Result<Value, GlError> {
    let out = glab(target.repo, target.host, &["api", "--hostname", target.host, path], target.cancelled)?;
    serde_json::from_str(&out).map_err(|e| GlError::Other(e.to_string()))
}

/// The open MRs for every candidate name, one REST call per candidate. Each returned entry is
/// `(iid, sha)`, exactly the shape [`super::select_open`] consumes.
fn resolve_open(
    target: &FetchTarget<'_>,
    candidates: &[String],
) -> Result<Vec<Vec<(u64, String)>>, GlError> {
    candidates.iter().map(|branch| resolve_one_open(target, branch)).collect()
}

fn resolve_one_open(target: &FetchTarget<'_>, branch: &str) -> Result<Vec<(u64, String)>, GlError> {
    let path = format!(
        "projects/{}/merge_requests?source_branch={}&state=opened&per_page=100",
        proj(target),
        enc(branch)
    );
    let v = api_get(target, &path)?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| Some((m["iid"].as_u64()?, m["sha"].as_str().unwrap_or_default().to_string())))
        .collect())
}

/// The newest-created merged/closed MR per candidate name, one REST call per candidate. Each
/// returned entry is `(iid, created_at)`, exactly the shape [`super::select_historical`] consumes.
fn resolve_historical(
    target: &FetchTarget<'_>,
    candidates: &[String],
) -> Result<Vec<Vec<(u64, String)>>, GlError> {
    candidates.iter().map(|branch| resolve_one_historical(target, branch)).collect()
}

fn resolve_one_historical(
    target: &FetchTarget<'_>,
    branch: &str,
) -> Result<Vec<(u64, String)>, GlError> {
    let path = format!(
        "projects/{}/merge_requests?source_branch={}&order_by=created_at&sort=desc&per_page=20&state=all",
        proj(target),
        enc(branch)
    );
    let v = api_get(target, &path)?;
    let hit = v
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| matches!(m["state"].as_str(), Some("merged" | "closed")));
    Ok(hit
        .and_then(|m| Some((m["iid"].as_u64()?, m["created_at"].as_str().unwrap_or_default().to_string())))
        .into_iter()
        .collect())
}

/// The MR's full detail — identity, mergeability, head SHA, branches, and its head pipeline id.
fn mr_detail(target: &FetchTarget<'_>, iid: u64) -> Result<Value, GlError> {
    let path = format!("projects/{}/merge_requests/{iid}", proj(target));
    api_get(target, &path)
}

/// The head pipeline's jobs, normalised to [`Check`]s; `truncated` when the page came back full
/// (REST has no `pageInfo`, so a full page means "maybe more").
fn fetch_checks(target: &FetchTarget<'_>, pipeline_id: u64) -> Result<(Vec<Check>, bool), GlError> {
    let path = format!("projects/{}/pipelines/{pipeline_id}/jobs?per_page=100", proj(target));
    let v = api_get(target, &path)?;
    let arr = v.as_array().cloned().unwrap_or_default();
    let truncated = arr.len() == 100;
    let checks = arr
        .iter()
        .filter_map(|j| {
            let name = j["name"].as_str()?.to_string();
            Some(Check { name, status: job_status(j["status"].as_str().unwrap_or("")) })
        })
        .collect();
    Ok((checks, truncated))
}

/// The MR's discussions, normalised to [`Comment`]s; `truncated` when the page came back full.
fn fetch_comments(target: &FetchTarget<'_>, iid: u64) -> Result<(Vec<Comment>, bool), GlError> {
    let path = format!("projects/{}/merge_requests/{iid}/discussions?per_page=100", proj(target));
    let v = api_get(target, &path)?;
    let truncated = v.as_array().is_some_and(|a| a.len() == 100);
    Ok((map_discussions(&v), truncated))
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// Percent-encode for a URL path segment or query value: unreserved chars pass, all else %XX.
fn enc(s: &str) -> String {
    s.bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![b as char]
            }
            _ => format!("%{b:02X}").chars().collect(),
        })
        .collect()
}

/// opened → Open, merged → Merged, closed → Closed (default Open, like GitHub's `parse_state`,
/// so `locked` — an active MR frozen from further discussion — still reads as live).
fn parse_state(s: &str) -> PrState {
    match s {
        "merged" => PrState::Merged,
        "closed" => PrState::Closed,
        _ => PrState::Open,
    }
}

/// Fold GitLab's `detailed_merge_status` into a [`Merge`]. `conflict`/`broken_status` are actual
/// conflicts; `blocked_status`/`discussions_not_resolved`/`policies_denied` are gates a reviewer
/// can act on. `draft_status` only counts when the MR's own `draft` flag is *not* already set —
/// a draft MR's own marker already shows that, so folding it to Blocked too would be redundant.
/// Everything else (`mergeable`, `checking`, `unchecked`, `ci_must_pass`, …) folds into `Clean`.
fn derive_merge(detailed: Option<&str>, is_draft: bool) -> Merge {
    match detailed {
        Some("conflict" | "broken_status") => Merge::Conflicting,
        Some("blocked_status" | "discussions_not_resolved" | "policies_denied") => Merge::Blocked,
        Some("draft_status") if !is_draft => Merge::Blocked,
        _ => Merge::Clean,
    }
}

/// Normalise a pipeline job's `status` to a [`CheckStatus`].
fn job_status(s: &str) -> CheckStatus {
    match s {
        "success" => CheckStatus::Success,
        "failed" => CheckStatus::Failure,
        "running" => CheckStatus::Running,
        "skipped" | "manual" | "canceled" => CheckStatus::Skipped,
        // created/pending/waiting_for_resource/scheduled all read as still-pending.
        _ => CheckStatus::Pending,
    }
}

/// Whether a GitLab username is a service-account bot (`…_bot…`) — GitLab has no dedicated
/// bot-flag on legacy notes, so the convention is the username, unless the author object itself
/// carries `"bot": true` (newer API responses).
fn author_is_bot(author: &Value) -> bool {
    let username = author["username"].as_str().unwrap_or("");
    username.contains("_bot") || author["bot"].as_bool().unwrap_or(false)
}

/// Discussions → Comments. A note with `system:true` is dropped. A note whose discussion
/// carries a `position` (inline) → Finding with anchor `new_path:new_line` (falling back to
/// `old_path:old_line` when `new_*` is null → also `is_outdated=true`), `resolved` from the
/// first note's `resolved`, `reply_count` = `notes.len()-1`. A non-positioned, non-system note
/// → Comment (`individual_note` discussions have exactly one note). Author identity and body
/// come from the first note in the discussion.
fn map_discussions(discussions: &Value) -> Vec<Comment> {
    let mut out = Vec::new();
    for d in discussions.as_array().into_iter().flatten() {
        let notes = match d["notes"].as_array() {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let root = &notes[0];
        if root["system"].as_bool().unwrap_or(false) {
            continue;
        }
        let author = &root["author"];
        let username = author["username"].as_str().unwrap_or("").to_string();
        let body = root["body"].as_str().unwrap_or("").trim().to_string();
        let created_at = root["created_at"].as_str().unwrap_or("").to_string();
        let reply_count = u32::try_from(notes.len().saturating_sub(1)).unwrap_or(u32::MAX);

        let position = root.get("position").filter(|p| !p.is_null());
        let comment = if let Some(pos) = position {
            let (anchor, is_outdated) = match (pos["new_path"].as_str(), pos["new_line"].as_u64())
            {
                (Some(p), Some(l)) if !p.is_empty() => (format!("{p}:{l}"), false),
                _ => {
                    let old_path = pos["old_path"].as_str().unwrap_or("");
                    let old_line = pos["old_line"].as_u64().unwrap_or(0);
                    (format!("{old_path}:{old_line}"), true)
                }
            };
            Comment {
                kind: CommentKind::Finding,
                author: username.clone(),
                author_is_bot: author_is_bot(author),
                anchor,
                body,
                snippet: None,
                created_at,
                is_resolved: root["resolved"].as_bool().unwrap_or(false),
                is_outdated,
                reply_count,
            }
        } else {
            Comment {
                kind: CommentKind::Comment,
                author: username.clone(),
                author_is_bot: author_is_bot(author),
                anchor: "comment".to_string(),
                body,
                snippet: None,
                created_at,
                is_resolved: false,
                is_outdated: false,
                reply_count: 0,
            }
        };
        out.push(comment);
    }
    // Newest first, matching GitHub's merged-comment ordering.
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

/// Snapshot assembly from detail + jobs + comments + sync. `truncated` when any surface
/// returned exactly `per_page` rows (REST has no `pageInfo`; a full page means "maybe more").
fn build_snapshot(
    detail: &Value,
    jobs: &[Check],
    comments: Vec<Comment>,
    sync: Sync,
    truncated: bool,
) -> PrSnapshot {
    let is_draft = detail["draft"].as_bool().unwrap_or(false);
    let head_is_fork = match (detail["source_project_id"].as_u64(), detail["target_project_id"].as_u64())
    {
        (Some(source), Some(target)) => source != target,
        _ => false,
    };
    PrSnapshot {
        number: detail["iid"].as_u64().unwrap_or_default(),
        title: detail["title"].as_str().unwrap_or_default().to_string(),
        url: detail["web_url"].as_str().unwrap_or_default().to_string(),
        state: parse_state(detail["state"].as_str().unwrap_or("opened")),
        is_draft,
        head_ref: detail["source_branch"].as_str().unwrap_or_default().to_string(),
        head_is_fork,
        base_ref: detail["target_branch"].as_str().unwrap_or_default().to_string(),
        merge: derive_merge(detail["detailed_merge_status"].as_str(), is_draft),
        sync,
        checks: jobs.to_vec(),
        comments,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_maps_the_three_gitlab_lifecycles() {
        assert_eq!(parse_state("opened"), PrState::Open);
        assert_eq!(parse_state("merged"), PrState::Merged);
        assert_eq!(parse_state("closed"), PrState::Closed);
        assert_eq!(parse_state("locked"), PrState::Open); // default is the live case
    }

    #[test]
    fn derive_merge_folds_conflict_and_blocked_but_not_a_flagged_draft() {
        assert_eq!(derive_merge(Some("conflict"), false), Merge::Conflicting);
        assert_eq!(derive_merge(Some("broken_status"), false), Merge::Conflicting);
        assert_eq!(derive_merge(Some("blocked_status"), false), Merge::Blocked);
        assert_eq!(derive_merge(Some("discussions_not_resolved"), false), Merge::Blocked);
        assert_eq!(derive_merge(Some("policies_denied"), false), Merge::Blocked);
        // draft_status only blocks when the MR's own draft flag is NOT already set.
        assert_eq!(derive_merge(Some("draft_status"), true), Merge::Clean);
        assert_eq!(derive_merge(Some("draft_status"), false), Merge::Blocked);
        // Everything else folds to Clean, including still-computing/unchecked states.
        assert_eq!(derive_merge(Some("mergeable"), false), Merge::Clean);
        assert_eq!(derive_merge(Some("checking"), false), Merge::Clean);
        assert_eq!(derive_merge(Some("unchecked"), false), Merge::Clean);
        assert_eq!(derive_merge(Some("ci_must_pass"), false), Merge::Clean);
        assert_eq!(derive_merge(None, false), Merge::Clean);
    }

    #[test]
    fn job_status_maps_every_documented_arm() {
        assert_eq!(job_status("success"), CheckStatus::Success);
        assert_eq!(job_status("failed"), CheckStatus::Failure);
        assert_eq!(job_status("running"), CheckStatus::Running);
        assert_eq!(job_status("created"), CheckStatus::Pending);
        assert_eq!(job_status("pending"), CheckStatus::Pending);
        assert_eq!(job_status("waiting_for_resource"), CheckStatus::Pending);
        assert_eq!(job_status("scheduled"), CheckStatus::Pending);
        assert_eq!(job_status("skipped"), CheckStatus::Skipped);
        assert_eq!(job_status("manual"), CheckStatus::Skipped);
        assert_eq!(job_status("canceled"), CheckStatus::Skipped);
    }

    #[test]
    fn map_discussions_drops_system_notes_and_maps_inline_and_plain() {
        let discussions = serde_json::json!([
            {
                "id": "d0",
                "notes": [
                    {"id": 1, "system": true, "body": "changed the description",
                     "author": {"username": "persijano"}, "created_at": "2026-07-11T09:00:00.000Z"}
                ]
            },
            {
                "id": "d1",
                "notes": [
                    {"id": 2, "system": false, "resolved": true, "body": "fix this",
                     "author": {"username": "persijano"}, "created_at": "2026-07-11T10:00:00.000Z",
                     "position": {"new_path": "src/a.rs", "new_line": 12, "old_path": null, "old_line": null}},
                    {"id": 3, "system": false, "body": "on it",
                     "author": {"username": "reviewer2"}, "created_at": "2026-07-11T10:05:00.000Z"},
                    {"id": 4, "system": false, "body": "done",
                     "author": {"username": "persijano"}, "created_at": "2026-07-11T10:10:00.000Z"}
                ]
            },
            {
                "id": "d2",
                "notes": [
                    {"id": 5, "system": false, "body": "looks good overall",
                     "author": {"username": "reviewer2"}, "created_at": "2026-07-11T11:00:00.000Z"}
                ]
            },
            {
                "id": "d3",
                "notes": [
                    {"id": 6, "system": false, "body": "automated note",
                     "author": {"username": "project_42_bot_abc"}, "created_at": "2026-07-11T12:00:00.000Z"}
                ]
            }
        ]);
        let cs = map_discussions(&discussions);
        assert_eq!(cs.len(), 3); // the system note is dropped

        let finding = cs.iter().find(|c| c.kind == CommentKind::Finding).unwrap();
        assert!(finding.is_resolved);
        assert_eq!(finding.reply_count, 2);
        assert_eq!(finding.anchor, "src/a.rs:12");
        assert!(!finding.is_outdated);

        let plain = cs.iter().find(|c| c.author == "reviewer2").unwrap();
        assert_eq!(plain.kind, CommentKind::Comment);
        assert_eq!(plain.anchor, "comment");

        let bot = cs.iter().find(|c| c.author == "project_42_bot_abc").unwrap();
        assert!(bot.author_is_bot);
    }

    #[test]
    fn map_discussions_falls_back_to_the_old_position_when_new_is_null() {
        let discussions = serde_json::json!([
            {
                "id": "d0",
                "notes": [
                    {"id": 1, "system": false, "resolved": false, "body": "stale line",
                     "author": {"username": "persijano"}, "created_at": "2026-07-11T10:00:00.000Z",
                     "position": {"new_path": null, "new_line": null, "old_path": "src/b.rs", "old_line": 7}}
                ]
            }
        ]);
        let cs = map_discussions(&discussions);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].anchor, "src/b.rs:7");
        assert!(cs[0].is_outdated);
    }

    #[test]
    fn build_snapshot_maps_iid_draft_fork_and_state() {
        let detail = serde_json::json!({
            "iid": 42, "title": "Add feature", "web_url": "https://gitlab.example.com/g/p/-/merge_requests/42",
            "state": "opened", "draft": true, "source_branch": "feat/x", "target_branch": "main",
            "source_project_id": 1, "target_project_id": 2,
            "detailed_merge_status": "draft_status", "sha": "abc123"
        });
        let s = build_snapshot(&detail, &[], Vec::new(), Sync::InSync, false);
        assert_eq!(s.number, 42);
        assert!(s.is_draft);
        assert!(s.head_is_fork); // source_project_id != target_project_id
        assert_eq!(s.state, PrState::Open);
        assert_eq!(s.merge, Merge::Clean); // draft_status + draft flag already set → Clean
        assert_eq!(s.head_ref, "feat/x");
        assert_eq!(s.base_ref, "main");

        // Same-project MR is not a fork.
        let mut same = detail.clone();
        same["target_project_id"] = serde_json::json!(1);
        assert!(!build_snapshot(&same, &[], Vec::new(), Sync::InSync, false).head_is_fork);

        // Absent fields default rather than fail — a mid-rollout API response degrades soft.
        let bare = serde_json::json!({"iid": 7});
        let s = build_snapshot(&bare, &[], Vec::new(), Sync::InSync, false);
        assert_eq!(s.head_ref, "");
        assert!(!s.head_is_fork);
    }

    #[test]
    fn enc_percent_encodes_reserved_bytes() {
        assert_eq!(enc("group/sub"), "group%2Fsub");
        assert_eq!(enc("feat/x#1"), "feat%2Fx%231");
        assert_eq!(enc("safe-._~123"), "safe-._~123");
    }

    #[test]
    fn glab_failure_classifies_by_stderr_wording() {
        assert_eq!(
            classify_failure("glab auth login required", "gitlab.example.com"),
            GlError::NotAuthed("gitlab.example.com".to_string())
        );
        assert_eq!(
            classify_failure("You are not authenticated", "gitlab.com"),
            GlError::NotAuthed("gitlab.com".to_string())
        );
        assert_eq!(
            classify_failure("HTTP 401 unauthorized", "gitlab.com"),
            GlError::NotAuthed("gitlab.com".to_string())
        );
        assert_eq!(
            classify_failure("HTTP 500 something", "gitlab.com"),
            GlError::Other("HTTP 500 something".into())
        );
    }
}
