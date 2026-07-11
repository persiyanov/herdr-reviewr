//! Read-only Bitbucket Data Center access via `curl`: the pull request's identity, state,
//! checks, and comments. See `specs/forge-host.md`. Reads its canonical target through
//! explicitly hosted REST calls under `/rest/api/latest`. It never posts, resolves, re-runs,
//! merges, or otherwise writes to Bitbucket. Unlike `gh`/`glab`, there is no Bitbucket CLI, so
//! this backend authenticates itself: a bearer token from `BITBUCKET_TOKEN` or `git credential
//! fill`, carried to `curl` on stdin so it never appears in argv (`ps`/`/proc` visible args).

use serde_json::Value;

use super::{
    Check, CheckStatus, Comment, CommentKind, FetchTarget, ForgeError, Merge, PrFetchInput,
    PrSnapshot, PrState, PrView, Sync, enc,
};

/// Read Bitbucket for one already-derived input, dispatched from [`super::backend_fetch`]. The
/// snapshot-or-empty `PrView` variants only (`Pr` / `NoPr` / `Ambiguous`) — the core handles
/// origin and candidate pre-checks before dispatch.
pub(crate) fn fetch(target: &FetchTarget<'_>, input: &PrFetchInput) -> Result<PrView, ForgeError> {
    fetch_inner(target, input)
}

fn fetch_inner(target: &FetchTarget<'_>, input: &PrFetchInput) -> Result<PrView, ForgeError> {
    let token = resolve_token(target)?;

    // Resolve the open PR across all candidates, one REST call per candidate — Bitbucket DC's
    // REST API has no aliased-batch equivalent to GitHub's GraphQL query.
    let (open, open_truncated) = resolve_open(target, &token, &input.candidates)?;
    let mut truncated = open_truncated;
    let id = match super::select_open(&open, input.head_oid.as_deref()) {
        super::Pick::One(n) => n,
        super::Pick::Ambiguous(count) => return Ok(PrView::Ambiguous(count)),
        super::Pick::None => {
            // No open PR anywhere: fall back to the newest-created declined/merged PR.
            let (hist, hist_truncated) = resolve_historical(target, &token, &input.candidates)?;
            truncated |= hist_truncated;
            match super::select_historical(&hist) {
                Some(n) => n,
                None => return Ok(PrView::NoPr(input.candidates.clone())),
            }
        }
    };

    let detail = pr_detail(target, &token, id)?;
    if detail.is_null() {
        return Ok(PrView::NoPr(input.candidates.clone()));
    }

    // Sync compares the fetch's pinned HEAD to the PR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's PR with another branch's count.
    let head_sha = detail["fromRef"]["latestCommit"].as_str().unwrap_or_default();
    let sync = match input.head_oid.as_deref() {
        Some(pin) if !head_sha.is_empty() => super::derive_sync(
            crate::git::ahead_behind_oids(target.repo, pin, head_sha)
                .map_err(|e| ForgeError::Other(e.0))?,
        ),
        _ => Sync::Unknown,
    };

    // Mergeability only makes sense (and is only queried) while the PR is still open.
    let merge = if detail["state"].as_str() == Some("OPEN") {
        fetch_merge(target, &token, id)?
    } else {
        Merge::Clean
    };

    let (checks, checks_truncated) = if head_sha.is_empty() {
        (Vec::new(), false)
    } else {
        fetch_checks(target, &token, head_sha)?
    };
    truncated |= checks_truncated;

    let (comments, comments_truncated) = fetch_comments(target, &token, id)?;
    truncated |= comments_truncated;

    Ok(PrView::Pr(Box::new(build_snapshot(&detail, merge, checks, comments, sync, truncated))))
}

// ---- Auth ---------------------------------------------------------------------------------

/// Resolve the bearer token for `target.host`: `BITBUCKET_TOKEN` first, else `git credential
/// fill`, else [`ForgeError::NoToken`]. The env var is read fresh on every fetch — never cached
/// globally — so a rotated token takes effect on the next poll without a restart. The credential
/// helper is only spawned when the env var is absent/empty, so a poll with `BITBUCKET_TOKEN` set
/// never pays for a `git credential fill` subprocess.
fn resolve_token(target: &FetchTarget<'_>) -> Result<String, ForgeError> {
    let env = std::env::var("BITBUCKET_TOKEN").ok();
    token_from(env, || credential_password(target))
        .map_err(|_| ForgeError::NoToken(target.host.to_string()))
}

/// The pure auth decision, independent of how `env`/`credential_password` were obtained: a
/// non-empty env var wins, then a non-empty credential-helper password, else no token.
/// `credential_password` is only invoked when `env` is absent/empty, so callers can pass a
/// closure that spawns a subprocess without paying for it on the common env-var-set path. The
/// `Err`'s host is a placeholder — [`resolve_token`] rebuilds it with the real host, since this
/// function has no host to report.
fn token_from(
    env: Option<String>,
    credential_password: impl FnOnce() -> Option<String>,
) -> Result<String, ForgeError> {
    if let Some(t) = env.filter(|s| !s.is_empty()) {
        return Ok(t);
    }
    if let Some(p) = credential_password().filter(|s| !s.is_empty()) {
        return Ok(p);
    }
    Err(ForgeError::NoToken(String::new()))
}

/// Ask `git credential fill` for the password it has stored for `target.host` over HTTPS, or
/// `None` on any failure (tool missing, cancelled, no matching credential). Thin and untested —
/// the decision logic lives in [`token_from`].
fn credential_password(target: &FetchTarget<'_>) -> Option<String> {
    let input = format!("protocol=https\nhost={}\n\n", target.host);
    let out = super::proc::run_tool(
        "git",
        target.repo,
        &["credential", "fill"],
        Some(&input),
        target.cancelled,
    )
    .ok()?;
    out.lines().find_map(|l| l.strip_prefix("password=").map(str::to_string))
}

// ---- Transport -----------------------------------------------------------------------------

/// GET `url` with the bearer token via a stdin curl config, so the token is invisible to
/// `ps`/`/proc`. `--fail` maps HTTP errors to exit 22 with the status line on stderr.
fn curl_get(target: &FetchTarget<'_>, token: &str, url: &str) -> Result<Value, ForgeError> {
    let config = format!("header = \"Authorization: Bearer {token}\"\n");
    let out = super::proc::run_tool(
        "curl",
        target.repo,
        &["--silent", "--show-error", "--fail", "--config", "-", url],
        Some(&config),
        target.cancelled,
    )
    .map_err(|f| classify(f, target.host))?;
    serde_json::from_str(&out).map_err(|e| ForgeError::Other(e.to_string()))
}

/// `NotFound` → `NoCli("curl")`; a cancelled/IO failure → `Other`; a failed run's stderr
/// containing "401"/"403" → `NotAuthed{Bitbucket, host}`; anything else → `Other`. `curl` has no
/// stable exit code for "unauthenticated" beyond the generic `--fail` 22, so this reads stderr.
fn classify(f: super::proc::RunFail, host: &str) -> ForgeError {
    match f {
        super::proc::RunFail::NotFound => ForgeError::NoCli("curl"),
        super::proc::RunFail::Cancelled => ForgeError::Other("request cancelled".to_string()),
        super::proc::RunFail::Io(message) => ForgeError::Other(message),
        super::proc::RunFail::Failed { stderr } => {
            let s = stderr.to_lowercase();
            if s.contains("401") || s.contains("403") {
                ForgeError::NotAuthed { forge: crate::git::Forge::Bitbucket, host: host.to_owned() }
            } else {
                ForgeError::Other(stderr.trim().to_string())
            }
        }
    }
}

/// The project/repo base every REST call shares: `owner` is the project key, `name` the repo
/// slug (`FetchTarget`'s contract for Bitbucket).
fn base(target: &FetchTarget<'_>) -> String {
    format!(
        "https://{}/rest/api/latest/projects/{}/repos/{}",
        target.host,
        enc(target.owner),
        enc(target.name)
    )
}

/// Whether a Bitbucket listing response's page was not the last — REST DC pages every listing
/// with `isLastPage`; `false` means a fuller surface exists past the requested `limit`.
fn is_truncated(v: &Value) -> bool {
    v["isLastPage"].as_bool() == Some(false)
}

/// Per-candidate `(id, sort_key)` lists — `sort_key` is `latestCommit` for the open resolve,
/// an ISO-8601 `createdDate` for the historical one — paired with whether any page was
/// truncated. Factored out so the resolve functions' signatures pass clippy's type-complexity
/// lint.
type CandidateHits = (Vec<Vec<(u64, String)>>, bool);

/// The open PRs for every candidate name, one REST call per candidate. Each returned entry is
/// `(id, latestCommit)`, exactly the shape [`super::select_open`] consumes.
fn resolve_open(
    target: &FetchTarget<'_>,
    token: &str,
    candidates: &[String],
) -> Result<CandidateHits, ForgeError> {
    let mut truncated = false;
    let mut out = Vec::with_capacity(candidates.len());
    for branch in candidates {
        let (entries, trunc) = resolve_one_open(target, token, branch)?;
        truncated |= trunc;
        out.push(entries);
    }
    Ok((out, truncated))
}

fn resolve_one_open(
    target: &FetchTarget<'_>,
    token: &str,
    branch: &str,
) -> Result<(Vec<(u64, String)>, bool), ForgeError> {
    let url = format!(
        "{}/pull-requests?state=OPEN&direction=OUTGOING&at=refs/heads/{}&limit=100",
        base(target),
        enc(branch)
    );
    let v = curl_get(target, token, &url)?;
    let entries = v["values"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some((
                p["id"].as_u64()?,
                p["fromRef"]["latestCommit"].as_str().unwrap_or_default().to_string(),
            ))
        })
        .collect();
    Ok((entries, is_truncated(&v)))
}

/// The newest-created non-open PR per candidate name, one REST call per candidate. Each
/// returned entry is `(id, iso_created_at)`, exactly the shape [`super::select_historical`]
/// consumes — `createdDate` is converted through [`epoch_ms_to_iso`] so the lexical comparison
/// `select_historical` uses stays chronological.
fn resolve_historical(
    target: &FetchTarget<'_>,
    token: &str,
    candidates: &[String],
) -> Result<CandidateHits, ForgeError> {
    let mut truncated = false;
    let mut out = Vec::with_capacity(candidates.len());
    for branch in candidates {
        let (entries, trunc) = resolve_one_historical(target, token, branch)?;
        truncated |= trunc;
        out.push(entries);
    }
    Ok((out, truncated))
}

fn resolve_one_historical(
    target: &FetchTarget<'_>,
    token: &str,
    branch: &str,
) -> Result<(Vec<(u64, String)>, bool), ForgeError> {
    let url = format!(
        "{}/pull-requests?state=ALL&direction=OUTGOING&at=refs/heads/{}&limit=20",
        base(target),
        enc(branch)
    );
    let v = curl_get(target, token, &url)?;
    let best = v["values"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["state"].as_str() != Some("OPEN"))
        .filter_map(|p| Some((p["id"].as_u64()?, p["createdDate"].as_i64()?)))
        .max_by_key(|&(_, created)| created);
    let entries = best.map(|(id, created)| (id, epoch_ms_to_iso(created))).into_iter().collect();
    Ok((entries, is_truncated(&v)))
}

/// The PR's full detail — identity, branches, head commit, and its own draft/state flags.
fn pr_detail(target: &FetchTarget<'_>, token: &str, id: u64) -> Result<Value, ForgeError> {
    let url = format!("{}/pull-requests/{id}", base(target));
    curl_get(target, token, &url)
}

/// The PR's mergeability, folded to a [`Merge`]. Only queried while the PR is open.
fn fetch_merge(target: &FetchTarget<'_>, token: &str, id: u64) -> Result<Merge, ForgeError> {
    let url = format!("{}/pull-requests/{id}/merge", base(target));
    let v = curl_get(target, token, &url)?;
    Ok(derive_merge(&v))
}

/// The head commit's build statuses, normalised to [`Check`]s. This endpoint lives outside the
/// project/repo path — it is keyed by commit hash alone, shared across every repo on the host.
fn fetch_checks(
    target: &FetchTarget<'_>,
    token: &str,
    sha: &str,
) -> Result<(Vec<Check>, bool), ForgeError> {
    let url = format!("https://{}/rest/build-status/latest/commits/{sha}?limit=100", target.host);
    let v = curl_get(target, token, &url)?;
    let checks = v["values"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c["name"].as_str().or_else(|| c["key"].as_str())?.to_string();
            Some(Check { name, status: check_status(c["state"].as_str().unwrap_or("")) })
        })
        .collect();
    Ok((checks, is_truncated(&v)))
}

/// The PR's activity feed, filtered to comments and normalised to [`Comment`]s.
fn fetch_comments(
    target: &FetchTarget<'_>,
    token: &str,
    id: u64,
) -> Result<(Vec<Comment>, bool), ForgeError> {
    let url = format!("{}/pull-requests/{id}/activities?limit=100", base(target));
    let v = curl_get(target, token, &url)?;
    Ok((map_activities(&v["values"]), is_truncated(&v)))
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// OPEN → Open, MERGED → Merged, DECLINED → Closed (default Open, like the other backends'
/// `parse_state`, so an unrecognised lifecycle still reads as live rather than vanishing).
fn parse_state(s: &str) -> PrState {
    match s {
        "MERGED" => PrState::Merged,
        "DECLINED" => PrState::Closed,
        _ => PrState::Open,
    }
}

/// Fold the `/merge` endpoint's `{canMerge, conflicted, vetoes[]}` into a [`Merge`]. A real
/// conflict always wins; a veto without `canMerge` is a blocking gate a reviewer can act on;
/// everything else (including a still-computing response) reads as `Clean`.
fn derive_merge(v: &Value) -> Merge {
    let conflicted = v["conflicted"].as_bool().unwrap_or(false);
    let can_merge = v["canMerge"].as_bool().unwrap_or(true);
    let vetoes_empty = v["vetoes"].as_array().is_none_or(Vec::is_empty);
    if conflicted {
        Merge::Conflicting
    } else if !can_merge && !vetoes_empty {
        Merge::Blocked
    } else {
        Merge::Clean
    }
}

/// Normalise a build-status `state` to a [`CheckStatus`]. `UNKNOWN` and any other value read as
/// still-pending, matching the siblings' "unrecognised = pending" default.
fn check_status(s: &str) -> CheckStatus {
    match s {
        "SUCCESSFUL" => CheckStatus::Success,
        "FAILED" => CheckStatus::Failure,
        "INPROGRESS" => CheckStatus::Running,
        "CANCELLED" => CheckStatus::Skipped,
        _ => CheckStatus::Pending,
    }
}

/// `createdDate` is epoch milliseconds → ISO-8601 `YYYY-MM-DDTHH:MM:SSZ`, so sorting and
/// `relative_age` work unchanged. Inverse of `mod.rs`'s `parse_iso` civil-date algorithm, via
/// Howard Hinnant's `civil_from_days`.
// The civil-from-days algorithm reads naturally with the conventional short field names.
#[allow(clippy::many_single_char_names, clippy::similar_names)]
fn epoch_ms_to_iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (h, mi, se) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    let y = y + i64::from(m <= 2);

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z")
}

/// An activity with a `commentAnchor{path, line}` → [`CommentKind::Finding`] (anchor
/// `path:line`; a missing `line` anchors to the path alone; `anchor.orphaned == true` marks it
/// outdated). `comment.state == "RESOLVED"` or `comment.threadResolved == true` → resolved.
/// `reply_count` is the total nested `comment.comments[]` entries, counted recursively (a reply
/// can itself carry replies). Without an anchor → [`CommentKind::Comment`]. Bitbucket has no
/// review-body surface, so there is no `Review` kind here. `author` is `comment.author.name`;
/// `author_is_bot` is `comment.author.type == "SERVICE"` when present, else the name
/// ending in `-bot`/`_bot`. Non-`COMMENTED` activities (opened, approved, rescoped, …) are
/// dropped.
fn map_activities(activities: &Value) -> Vec<Comment> {
    let mut out = Vec::new();
    for a in activities.as_array().into_iter().flatten() {
        if a["action"].as_str() != Some("COMMENTED") {
            continue;
        }
        let comment = &a["comment"];
        if comment.is_null() {
            continue;
        }
        let author = &comment["author"];
        let author_name = author["name"].as_str().unwrap_or("").to_string();
        let author_is_bot = match author["type"].as_str() {
            Some(t) => t == "SERVICE",
            None => author_name.ends_with("-bot") || author_name.ends_with("_bot"),
        };
        let body = comment["text"].as_str().unwrap_or("").trim().to_string();
        let created_at = comment["createdDate"].as_i64().map(epoch_ms_to_iso).unwrap_or_default();
        let is_resolved = comment["state"].as_str() == Some("RESOLVED")
            || comment["threadResolved"].as_bool().unwrap_or(false);
        let reply_count = count_replies(comment);

        let anchor = a.get("commentAnchor").filter(|v| !v.is_null());
        let c = if let Some(anchor) = anchor {
            let path = anchor["path"].as_str().unwrap_or("");
            let anchor_str = match anchor["line"].as_u64() {
                Some(line) => format!("{path}:{line}"),
                None => path.to_string(),
            };
            Comment {
                kind: CommentKind::Finding,
                author: author_name,
                author_is_bot,
                anchor: anchor_str,
                body,
                snippet: None,
                created_at,
                is_resolved,
                is_outdated: anchor["orphaned"].as_bool().unwrap_or(false),
                reply_count,
            }
        } else {
            Comment {
                kind: CommentKind::Comment,
                author: author_name,
                author_is_bot,
                anchor: "comment".to_string(),
                body,
                snippet: None,
                created_at,
                is_resolved,
                is_outdated: false,
                reply_count,
            }
        };
        out.push(c);
    }
    // Newest first, matching the other backends' merged-comment ordering.
    out.sort_by(|x, y| y.created_at.cmp(&x.created_at));
    out
}

/// Count every nested `comments[]` entry under `comment`, recursively — a reply can itself
/// carry replies, so this is the total thread size below the root, not just its direct replies.
fn count_replies(comment: &Value) -> u32 {
    let Some(arr) = comment["comments"].as_array() else {
        return 0;
    };
    let direct = u32::try_from(arr.len()).unwrap_or(u32::MAX);
    direct + arr.iter().map(count_replies).sum::<u32>()
}

/// Snapshot assembly from detail + merge + checks + comments + sync. `number` is `id`;
/// `head_ref`/`base_ref` are `fromRef`/`toRef`'s `displayId`. A fork is detected by comparing
/// the PR's own `fromRef.repository` to its `toRef.repository` (always the queried repo) —
/// case-insensitive on the project key, exact on the slug — so no external target is needed
/// here. `draft` defaults to `false` when absent (older DC versions predate the field).
fn build_snapshot(
    detail: &Value,
    merge: Merge,
    checks: Vec<Check>,
    comments: Vec<Comment>,
    sync: Sync,
    truncated: bool,
) -> PrSnapshot {
    let from_repo = &detail["fromRef"]["repository"];
    let to_repo = &detail["toRef"]["repository"];
    let from_key = from_repo["project"]["key"].as_str().unwrap_or("");
    let to_key = to_repo["project"]["key"].as_str().unwrap_or("");
    let from_slug = from_repo["slug"].as_str().unwrap_or("");
    let to_slug = to_repo["slug"].as_str().unwrap_or("");
    let head_is_fork = !from_key.eq_ignore_ascii_case(to_key) || from_slug != to_slug;

    PrSnapshot {
        number: detail["id"].as_u64().unwrap_or_default(),
        title: detail["title"].as_str().unwrap_or_default().to_string(),
        url: detail["links"]["self"][0]["href"].as_str().unwrap_or_default().to_string(),
        state: parse_state(detail["state"].as_str().unwrap_or("OPEN")),
        is_draft: detail["draft"].as_bool().unwrap_or(false),
        head_ref: detail["fromRef"]["displayId"].as_str().unwrap_or_default().to_string(),
        head_is_fork,
        base_ref: detail["toRef"]["displayId"].as_str().unwrap_or_default().to_string(),
        merge,
        sync,
        checks,
        comments,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_maps_the_three_bitbucket_lifecycles() {
        assert_eq!(parse_state("OPEN"), PrState::Open);
        assert_eq!(parse_state("MERGED"), PrState::Merged);
        assert_eq!(parse_state("DECLINED"), PrState::Closed);
        assert_eq!(parse_state("anything-else"), PrState::Open); // default is the live case
    }

    #[test]
    fn derive_merge_folds_conflicted_and_vetoed_but_not_a_clean_response() {
        assert_eq!(
            derive_merge(&serde_json::json!({"canMerge": false, "conflicted": true, "vetoes": []})),
            Merge::Conflicting
        );
        assert_eq!(
            derive_merge(
                &serde_json::json!({"canMerge": false, "conflicted": false, "vetoes": [{"summaryMessage": "blocked"}]})
            ),
            Merge::Blocked
        );
        assert_eq!(
            derive_merge(&serde_json::json!({"canMerge": true, "conflicted": false, "vetoes": []})),
            Merge::Clean
        );
        // canMerge:false with no vetoes (e.g. still computing) is not actionable → Clean.
        assert_eq!(
            derive_merge(
                &serde_json::json!({"canMerge": false, "conflicted": false, "vetoes": []})
            ),
            Merge::Clean
        );
    }

    #[test]
    fn check_status_maps_every_documented_arm() {
        assert_eq!(check_status("SUCCESSFUL"), CheckStatus::Success);
        assert_eq!(check_status("FAILED"), CheckStatus::Failure);
        assert_eq!(check_status("INPROGRESS"), CheckStatus::Running);
        assert_eq!(check_status("CANCELLED"), CheckStatus::Skipped);
        assert_eq!(check_status("UNKNOWN"), CheckStatus::Pending);
        assert_eq!(check_status("anything-else"), CheckStatus::Pending);
    }

    #[test]
    fn epoch_ms_round_trips_through_parse_iso() {
        for ms in [0i64, 1_752_192_000_000, 4_102_444_799_000] {
            let iso = epoch_ms_to_iso(ms);
            assert_eq!(super::super::parse_iso(&iso), Some(ms / 1000));
        }
    }

    #[test]
    fn map_activities_maps_anchored_findings_general_comments_and_bots() {
        let activities = serde_json::json!([
            {
                "action": "COMMENTED",
                "commentAnchor": {"path": "src/a.rs", "line": 12, "orphaned": false},
                "comment": {
                    "text": "fix this", "state": "RESOLVED", "createdDate": 1_752_192_000_000i64,
                    "author": {"name": "persijano"},
                    "comments": [
                        {"text": "on it", "createdDate": 1_752_192_100_000i64, "author": {"name": "reviewer2"}},
                        {"text": "done", "createdDate": 1_752_192_200_000i64, "author": {"name": "persijano"}}
                    ]
                }
            },
            {
                "action": "COMMENTED",
                "commentAnchor": {"path": "src/b.rs", "orphaned": true},
                "comment": {
                    "text": "stale line", "createdDate": 1_752_192_300_000i64,
                    "author": {"name": "persijano"}
                }
            },
            {
                "action": "COMMENTED",
                "comment": {
                    "text": "looks good overall", "createdDate": 1_752_192_400_000i64,
                    "author": {"name": "reviewer2"}
                }
            },
            {
                "action": "COMMENTED",
                "comment": {
                    "text": "automated note", "createdDate": 1_752_192_500_000i64,
                    "author": {"name": "ci-runner", "type": "SERVICE"}
                }
            },
            {
                "action": "APPROVED",
                "comment": {"text": "should be dropped", "createdDate": 1_752_192_600_000i64, "author": {"name": "x"}}
            }
        ]);
        let cs = map_activities(&activities);
        assert_eq!(cs.len(), 4); // the non-COMMENTED activity is dropped

        let finding = cs.iter().find(|c| c.anchor == "src/a.rs:12").unwrap();
        assert_eq!(finding.kind, CommentKind::Finding);
        assert!(finding.is_resolved);
        assert!(!finding.is_outdated);
        assert_eq!(finding.reply_count, 2);

        let orphaned = cs.iter().find(|c| c.anchor == "src/b.rs").unwrap();
        assert!(orphaned.is_outdated);
        assert!(!orphaned.is_resolved);
        assert_eq!(orphaned.reply_count, 0);

        let plain = cs.iter().find(|c| c.author == "reviewer2").unwrap();
        assert_eq!(plain.kind, CommentKind::Comment);
        assert_eq!(plain.anchor, "comment");

        let bot = cs.iter().find(|c| c.author == "ci-runner").unwrap();
        assert!(bot.author_is_bot);
    }

    #[test]
    fn map_activities_falls_back_to_a_name_suffix_when_no_user_type_is_present() {
        let activities = serde_json::json!([
            {"action": "COMMENTED", "comment": {"text": "hi", "createdDate": 0, "author": {"name": "deploy_bot"}}},
            {"action": "COMMENTED", "comment": {"text": "hi", "createdDate": 0, "author": {"name": "release-bot"}}},
            {"action": "COMMENTED", "comment": {"text": "hi", "createdDate": 0, "author": {"name": "persijano"}}}
        ]);
        let cs = map_activities(&activities);
        assert!(cs.iter().find(|c| c.author == "deploy_bot").unwrap().author_is_bot);
        assert!(cs.iter().find(|c| c.author == "release-bot").unwrap().author_is_bot);
        assert!(!cs.iter().find(|c| c.author == "persijano").unwrap().author_is_bot);
    }

    #[test]
    fn build_snapshot_detects_a_fork_by_differing_project_key_case_insensitively() {
        let detail = serde_json::json!({
            "id": 42, "title": "Add feature", "state": "OPEN", "draft": true,
            "links": {"self": [{"href": "https://bb.example.com/projects/TEAM/repos/svc/pull-requests/42"}]},
            "fromRef": {"displayId": "feat/x", "latestCommit": "abc123",
                        "repository": {"slug": "svc", "project": {"key": "team"}}},
            "toRef": {"displayId": "main", "repository": {"slug": "svc", "project": {"key": "TEAM"}}}
        });
        let s = build_snapshot(&detail, Merge::Clean, vec![], vec![], Sync::InSync, false);
        assert_eq!(s.number, 42);
        assert!(s.is_draft);
        assert!(!s.head_is_fork); // "team" vs "TEAM" — same key, case-insensitive
        assert_eq!(s.head_ref, "feat/x");
        assert_eq!(s.base_ref, "main");
        assert_eq!(s.url, "https://bb.example.com/projects/TEAM/repos/svc/pull-requests/42");

        let mut forked = detail.clone();
        forked["fromRef"]["repository"]["project"]["key"] = serde_json::json!("OTHER");
        assert!(
            build_snapshot(&forked, Merge::Clean, vec![], vec![], Sync::InSync, false).head_is_fork
        );

        // Absent fields default rather than fail — a mid-rollout API response degrades soft.
        let bare = serde_json::json!({"id": 7});
        let s = build_snapshot(&bare, Merge::Clean, vec![], vec![], Sync::InSync, false);
        assert!(!s.is_draft);
        assert_eq!(s.head_ref, "");
        assert!(!s.head_is_fork);
    }

    #[test]
    fn token_from_prefers_the_env_var_over_the_credential_helper() {
        assert_eq!(
            token_from(Some("env-token".to_string()), || Some("cred-pw".to_string())),
            Ok("env-token".to_string())
        );
        assert_eq!(token_from(None, || Some("cred-pw".to_string())), Ok("cred-pw".to_string()));
        assert_eq!(
            token_from(Some(String::new()), || Some("cred-pw".to_string())),
            Ok("cred-pw".to_string())
        );
        assert!(token_from(None, || None).is_err());
        assert!(token_from(Some(String::new()), || Some(String::new())).is_err());
    }

    #[test]
    fn token_from_never_calls_the_credential_helper_when_env_is_set() {
        let calls = std::cell::Cell::new(0);
        let result = token_from(Some("env-token".to_string()), || {
            calls.set(calls.get() + 1);
            Some("cred-pw".to_string())
        });
        assert_eq!(result, Ok("env-token".to_string()));
        assert_eq!(calls.get(), 0, "credential helper closure must not run when env wins");
    }

    #[test]
    fn classify_maps_curl_failures() {
        assert_eq!(
            classify(super::super::proc::RunFail::NotFound, "bb.example.com"),
            ForgeError::NoCli("curl")
        );
        assert_eq!(
            classify(
                super::super::proc::RunFail::Failed { stderr: "HTTP 401 Unauthorized".to_string() },
                "bb.example.com"
            ),
            ForgeError::NotAuthed {
                forge: crate::git::Forge::Bitbucket,
                host: "bb.example.com".to_string()
            }
        );
        assert_eq!(
            classify(
                super::super::proc::RunFail::Failed { stderr: "HTTP 403 Forbidden".to_string() },
                "bb.example.com"
            ),
            ForgeError::NotAuthed {
                forge: crate::git::Forge::Bitbucket,
                host: "bb.example.com".to_string()
            }
        );
        assert_eq!(
            classify(
                super::super::proc::RunFail::Failed { stderr: "HTTP 500".to_string() },
                "bb.example.com"
            ),
            ForgeError::Other("HTTP 500".to_string())
        );
    }
}
