//! Read-only Bitbucket Server / Data Center access: pull requests, checks, and activities.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::forge::{
    AssocPr, Association, Check, CheckStatus, Comment, CommentKind, FindingPlace, Merge,
    PrFetchInput, PrSnapshot, PrState, PrView, Sync, finish_comments, prose_row, push_unique,
    upsert_latest,
};

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

#[derive(Debug)]
enum BitbucketError {
    NoCurl,
    NotAuthed,
    Unavailable(String),
    LocalGit(String),
    Other(String),
}

impl BitbucketError {
    fn into_view(self, host: &str) -> PrView {
        match self {
            Self::NoCurl => PrView::NoCli(crate::git::Forge::Bitbucket),
            Self::NotAuthed => PrView::NotAuthed(crate::git::Forge::Bitbucket, host.to_owned()),
            Self::LocalGit(message) => PrView::GitError(message),
            Self::Unavailable(message) | Self::Other(message) => {
                PrView::Error(crate::git::Forge::Bitbucket, message)
            }
        }
    }
}

fn died(surface: &str) -> BitbucketError {
    BitbucketError::Other(format!("{surface} read panicked"))
}

fn optional_surface(result: Result<Value, BitbucketError>) -> Result<Value, BitbucketError> {
    match result {
        Err(BitbucketError::Unavailable(_)) => Ok(Value::Null),
        other => other,
    }
}

fn bb_token() -> Option<String> {
    let tok = std::env::var("BITBUCKET_TOKEN").ok()?;
    let tok = tok.trim();
    if tok.is_empty() { None } else { Some(tok.to_string()) }
}

fn classify_failure(stderr: &str) -> BitbucketError {
    let s = stderr.to_lowercase();
    if crate::forge::reports_status(&s, 401)
        || s.contains("401")
        || s.contains("unauthorized")
        || s.contains("authentication")
    {
        BitbucketError::NotAuthed
    } else if crate::forge::reports_status(&s, 403)
        || crate::forge::reports_status(&s, 404)
        || s.contains("403")
        || s.contains("404")
    {
        BitbucketError::Unavailable(stderr.trim().to_string())
    } else {
        BitbucketError::Other(stderr.trim().to_string())
    }
}

fn bb_api(host: &str, endpoint: &str, cancelled: &AtomicBool) -> Result<Value, BitbucketError> {
    let token = bb_token().ok_or(BitbucketError::NotAuthed)?;
    let url = format!("https://{host}{endpoint}");
    let mut cmd = crate::proc::command("curl");
    cmd.args([
        "-s",
        "-S",
        "--fail",
        "-H",
        &format!("Authorization: Bearer {token}"),
        "-H",
        "Accept: application/json",
        &url,
    ]);
    let stdout = crate::forge::run_provider(
        &mut cmd,
        cancelled,
        BitbucketError::NoCurl,
        classify_failure,
        BitbucketError::Other,
    )?;
    serde_json::from_str(&stdout).map_err(|e| BitbucketError::Other(e.to_string()))
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    target: &crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> Result<PrView, BitbucketError> {
    let host = target.host();
    let project = target.owner();
    let repo_name = target.name();

    let head = input.local.head_oid.as_deref();
    let (assoc, raw_nodes) =
        associate_by_branch(host, project, repo_name, &input.local.names, cancelled)?;
    let pick = crate::forge::resolve_pick(repo, &assoc, head)
        .map_err(|error| BitbucketError::LocalGit(error.0))?;

    let Some(number) = pick else {
        return Ok(PrView::NoPr);
    };

    let pr = if let Some(node) = raw_nodes.into_iter().find(|n| n["id"].as_u64() == Some(number)) {
        node
    } else {
        bb_api(
            host,
            &format!("/rest/api/1.0/projects/{project}/repos/{repo_name}/pull-requests/{number}"),
            cancelled,
        )?
    };

    let pr_head = pr["fromRef"]["latestCommit"].as_str().unwrap_or_default();
    let sync = crate::forge::local_sync(repo, input.local.head_oid.as_deref(), pr_head)
        .map_err(|error| BitbucketError::LocalGit(error.0))?;

    let (activities, checks) = std::thread::scope(|scope| {
        let act = scope.spawn(|| {
            optional_surface(bb_api(
                host,
                &format!(
                    "/rest/api/1.0/projects/{project}/repos/{repo_name}/pull-requests/{number}/activities?limit=100"
                ),
                cancelled,
            ))
        });
        let chk = scope.spawn(|| {
            if pr_head.is_empty() {
                Ok(Value::Null)
            } else {
                optional_surface(bb_api(
                    host,
                    &format!("/rest/build-status/1.0/commits/{pr_head}"),
                    cancelled,
                ))
            }
        });
        (
            crate::forge::join_read(act, || died("activities")),
            crate::forge::join_read(chk, || died("checks")),
        )
    });

    let activities = activities?;
    let checks_val = checks?;

    let parsed_checks = parse_checks(&checks_val);
    let comments = parse_activities_and_reviewers(&activities, &pr);

    let truncated = activities["isLastPage"].as_bool() == Some(false);

    Ok(PrView::Pr(Box::new(build_snapshot(&pr, sync, parsed_checks, comments, truncated))))
}

fn associate_by_branch(
    host: &str,
    project: &str,
    repo_name: &str,
    names: &[String],
    cancelled: &AtomicBool,
) -> Result<(Association, Vec<Value>), BitbucketError> {
    let mut assoc = Association::default();
    let mut raw_nodes = Vec::new();

    for name in names {
        let branch = name
            .strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("origin/"))
            .unwrap_or(name);
        let encoded_branch = crate::forge::urlencode(&format!("refs/heads/{branch}"));
        let endpoint = format!(
            "/rest/api/1.0/projects/{project}/repos/{repo_name}/pull-requests?state=ALL&direction=OUTGOING&at={encoded_branch}&limit=25"
        );
        let resp = optional_surface(bb_api(host, &endpoint, cancelled))?;
        if let Some(values) = resp["values"].as_array() {
            for pr in values {
                let Some(number) = pr["id"].as_u64() else { continue };
                let head_oid = pr["fromRef"]["latestCommit"].as_str().unwrap_or("").to_string();
                let head_ref = pr["fromRef"]["displayId"].as_str().unwrap_or("").to_string();
                let created_at =
                    pr["createdDate"].as_u64().map(format_epoch_millis).unwrap_or_default();
                let closed_at =
                    pr["closedDate"].as_u64().map(format_epoch_millis).unwrap_or_default();

                let assoc_pr = AssocPr {
                    number,
                    head_oid,
                    head_ref,
                    created_at,
                    closed_at,
                    raw: Some(pr.clone()),
                };

                let is_open =
                    pr["open"].as_bool().unwrap_or_else(|| pr["state"].as_str() == Some("OPEN"));

                if is_open {
                    push_unique(&mut assoc.open, assoc_pr);
                } else {
                    push_unique(&mut assoc.history, assoc_pr);
                }
                raw_nodes.push(pr.clone());
            }
        }
    }

    if assoc.open.is_empty() {
        let endpoint = format!(
            "/rest/api/1.0/projects/{project}/repos/{repo_name}/pull-requests?state=OPEN&limit=25"
        );
        if let Ok(resp) = optional_surface(bb_api(host, &endpoint, cancelled))
            && let Some(values) = resp["values"].as_array()
        {
            for pr in values {
                let branch = pr["fromRef"]["displayId"].as_str().unwrap_or("");
                let matches_name = names.iter().any(|n| {
                    let clean = n
                        .strip_prefix("refs/heads/")
                        .or_else(|| n.strip_prefix("origin/"))
                        .unwrap_or(n);
                    clean == branch
                });
                if matches_name {
                    let Some(number) = pr["id"].as_u64() else { continue };
                    let head_oid = pr["fromRef"]["latestCommit"].as_str().unwrap_or("").to_string();
                    let head_ref = branch.to_string();
                    let created_at =
                        pr["createdDate"].as_u64().map(format_epoch_millis).unwrap_or_default();
                    let assoc_pr = AssocPr {
                        number,
                        head_oid,
                        head_ref,
                        created_at,
                        closed_at: String::new(),
                        raw: Some(pr.clone()),
                    };
                    push_unique(&mut assoc.open, assoc_pr);
                    raw_nodes.push(pr.clone());
                }
            }
        }
    }

    Ok((assoc, raw_nodes))
}

fn build_snapshot(
    pr: &Value,
    sync: Sync,
    checks: Vec<Check>,
    comments: Vec<Comment>,
    truncated: bool,
) -> PrSnapshot {
    let number = pr["id"].as_u64().unwrap_or_default();
    let title = pr["title"].as_str().unwrap_or_default().to_string();
    let body = pr["description"].as_str().unwrap_or_default().to_string();

    let url = pr["links"]["self"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|l| l["href"].as_str())
        .unwrap_or_default()
        .to_string();

    let state = match pr["state"].as_str() {
        Some("OPEN") => PrState::Open,
        Some("MERGED") => PrState::Merged,
        _ => PrState::Closed,
    };

    let is_draft = pr["draft"].as_bool().unwrap_or(false);
    let head_ref = pr["fromRef"]["displayId"].as_str().unwrap_or_default().to_string();
    let base_ref = pr["toRef"]["displayId"].as_str().unwrap_or_default().to_string();
    let head_oid = pr["fromRef"]["latestCommit"].as_str().unwrap_or_default().to_string();

    let head_is_fork = pr["fromRef"]["repository"]["id"] != pr["toRef"]["repository"]["id"];

    let merge = if pr["properties"]["mergeResult"]["outcome"].as_str() == Some("CONFLICTED") {
        Merge::Conflicting
    } else {
        Merge::Clean
    };

    PrSnapshot {
        number,
        title,
        url,
        body,
        state,
        is_draft,
        head_ref,
        head_is_fork,
        head_oid,
        base_ref,
        merge,
        sync,
        checks,
        comments,
        truncated,
    }
}

fn parse_checks(checks_val: &Value) -> Vec<Check> {
    let mut checks = Vec::new();
    if let Some(values) = checks_val["values"].as_array() {
        for item in values {
            let name = item["name"]
                .as_str()
                .or_else(|| item["key"].as_str())
                .unwrap_or("build")
                .to_string();
            let status = match item["state"].as_str() {
                Some("SUCCESSFUL") => CheckStatus::Success,
                Some("FAILED") => CheckStatus::Failure,
                Some("INPROGRESS") => CheckStatus::Running,
                _ => CheckStatus::Pending,
            };
            upsert_latest(&mut checks, Check { name, status });
        }
    }
    checks
}

fn parse_activities_and_reviewers(activities: &Value, pr: &Value) -> Vec<Comment> {
    let mut out = Vec::new();

    if let Some(values) = activities["values"].as_array() {
        for act in values {
            let action = act["action"].as_str().unwrap_or("");
            if action == "COMMENTED" {
                if let Some(comment) = act["comment"].as_object() {
                    let comment_val = Value::Object(comment.clone());
                    let author = comment_val["author"]["displayName"]
                        .as_str()
                        .or_else(|| comment_val["author"]["name"].as_str())
                        .unwrap_or("")
                        .to_string();
                    let body = comment_val["text"].as_str().unwrap_or("").trim().to_string();
                    if body.is_empty() {
                        continue;
                    }

                    let anchor_val = &comment_val["anchor"];
                    let (kind, anchor, place) = if anchor_val.is_object() {
                        let path = anchor_val["path"].as_str().unwrap_or("");
                        let line = anchor_val["line"].as_u64();
                        let line_type = anchor_val["lineType"].as_str().unwrap_or("");
                        let file_type = anchor_val["fileType"].as_str().unwrap_or("");
                        let side = if file_type == "FROM" || line_type == "REMOVED" {
                            Some(crate::model::Side::Old)
                        } else {
                            Some(crate::model::Side::New)
                        };
                        let place = FindingPlace::from_lines(path, line, line, side);
                        (CommentKind::Finding, place.anchor(), Some(place))
                    } else {
                        (CommentKind::Comment, "comment".to_string(), None)
                    };

                    let is_resolved = comment_val["threadResolved"].as_bool().unwrap_or(false);
                    let is_outdated = anchor_val["orphaned"].as_bool().unwrap_or(false);
                    let created_at = comment_val["createdDate"]
                        .as_u64()
                        .map(format_epoch_millis)
                        .unwrap_or_default();
                    let reply_count = count_replies(&comment_val);
                    let snippet = extract_snippet(act);

                    out.push(Comment {
                        kind,
                        author_is_bot: is_bitbucket_bot(&author),
                        author,
                        anchor,
                        place,
                        body,
                        snippet,
                        created_at,
                        is_resolved,
                        is_outdated,
                        reply_count,
                    });
                }
            } else if action == "APPROVED" {
                let author = act["user"]["displayName"]
                    .as_str()
                    .or_else(|| act["user"]["name"].as_str())
                    .unwrap_or("")
                    .to_string();
                if !author.is_empty() {
                    let created_at =
                        act["createdDate"].as_u64().map(format_epoch_millis).unwrap_or_default();
                    out.push(prose_row(
                        CommentKind::Review,
                        author,
                        false,
                        "Approved this pull request.".to_string(),
                        created_at,
                    ));
                }
            }
        }
    }

    if let Some(reviewers) = pr["reviewers"].as_array() {
        for rev in reviewers {
            if rev["status"].as_str() == Some("APPROVED") {
                let author = rev["user"]["displayName"]
                    .as_str()
                    .or_else(|| rev["user"]["name"].as_str())
                    .unwrap_or("")
                    .to_string();
                if !author.is_empty()
                    && !out.iter().any(|c| c.kind == CommentKind::Review && c.author == author)
                {
                    out.push(prose_row(
                        CommentKind::Review,
                        author,
                        false,
                        "Approved this pull request.".to_string(),
                        String::new(),
                    ));
                }
            }
        }
    }

    finish_comments(&mut out);
    out
}

fn count_replies(comment: &Value) -> u32 {
    let mut count = 0;
    if let Some(children) = comment["comments"].as_array() {
        count += children.len() as u32;
        for child in children {
            count += count_replies(child);
        }
    }
    count
}

fn extract_snippet(activity: &Value) -> Option<String> {
    let hunks = activity["diff"]["hunks"].as_array()?;
    let mut snippet = String::new();
    for hunk in hunks {
        if let Some(segments) = hunk["segments"].as_array() {
            for seg in segments {
                let prefix = match seg["type"].as_str() {
                    Some("ADDED") => "+",
                    Some("REMOVED") => "-",
                    _ => " ",
                };
                if let Some(lines) = seg["lines"].as_array() {
                    for line in lines {
                        if let Some(text) = line["line"].as_str() {
                            snippet.push_str(prefix);
                            snippet.push_str(text);
                            snippet.push('\n');
                        }
                    }
                }
            }
        }
    }
    if snippet.is_empty() { None } else { Some(snippet) }
}

fn is_bitbucket_bot(author: &str) -> bool {
    let a = author.to_ascii_lowercase();
    a.contains("bot") || a.contains("renovate") || a.contains("jenkins") || a.starts_with("svc-")
}

#[allow(clippy::many_single_char_names, clippy::unreadable_literal)]
fn format_epoch_millis(ms: u64) -> String {
    let secs = ms / 1000;
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let mins = (day_secs % 3600) / 60;
    let s = day_secs % 60;

    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{mins:02}:{s:02}Z")
}
