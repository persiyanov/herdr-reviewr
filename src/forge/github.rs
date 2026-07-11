//! Read-only GitHub access via `gh`: the pull request's identity, state, checks, and comments.
//!
//! See `specs/forge-host.md`. Reads its canonical target through explicitly hosted `gh` GraphQL
//! calls. It never posts, resolves, re-runs, merges, or otherwise writes to GitHub.

use serde_json::Value;

use super::{FetchTarget, Merge, PrFetchInput, PrSnapshot, PrState, PrView, Sync};

/// Read GitHub for one already-derived input, dispatched from [`super::backend_fetch`]. The
/// snapshot-or-empty `PrView` variants only (`Pr` / `NoPr` / `Ambiguous`) — the core handles
/// origin and candidate pre-checks before dispatch.
pub(crate) fn fetch(
    target: &FetchTarget<'_>,
    input: &PrFetchInput,
) -> Result<PrView, super::ForgeError> {
    fetch_inner(target, input).map_err(Into::into)
}

fn fetch_inner(target: &FetchTarget<'_>, input: &PrFetchInput) -> Result<PrView, GhError> {
    // Resolve the open PR across all candidates in one aliased call, then read its detail
    // directly — `mergeable` only populates on direct access, never through the list
    // connection (`specs/forge-host.md`).
    let open = resolve_candidates(target, &input.candidates, OPEN, "headRefOid")?;
    let number = match super::select_open(&open, input.head_oid.as_deref()) {
        super::Pick::One(n) => n,
        super::Pick::Ambiguous(count) => return Ok(PrView::Ambiguous(count)),
        super::Pick::None => {
            // No open PR anywhere: fall back to the newest-created merged/closed PR.
            let hist = resolve_candidates(target, &input.candidates, HISTORICAL, "createdAt")?;
            match super::select_historical(&hist) {
                Some(n) => n,
                None => return Ok(PrView::NoPr(input.candidates.clone())),
            }
        }
    };
    let detail = pr_detail(target, number)?;
    let node = &detail["data"]["repository"]["pullRequest"];
    if node.is_null() {
        return Ok(PrView::NoPr(input.candidates.clone()));
    }
    // Sync compares the fetch's pinned HEAD to the PR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's PR with another branch's count.
    let pr_head = node["headRefOid"].as_str().unwrap_or_default();
    let sync = match input.head_oid.as_deref() {
        Some(pin) if !pr_head.is_empty() => super::derive_sync(
            crate::git::ahead_behind_oids(target.repo, pin, pr_head)
                .map_err(|e| GhError::Other(e.0))?,
        ),
        _ => Sync::Unknown,
    };
    Ok(PrView::Pr(Box::new(build_snapshot(node, sync))))
}

/// Run explicitly targeted `gh` arguments in `repo` and return stdout or a classified failure.
fn gh(
    repo: &std::path::Path,
    host: &str,
    args: &[&str],
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<String, GhError> {
    match super::proc::run_tool("gh", repo, args, None, cancelled) {
        Ok(stdout) => Ok(stdout),
        Err(super::proc::RunFail::NotFound) => Err(GhError::NoGh),
        Err(super::proc::RunFail::Cancelled) => {
            Err(GhError::Other("request cancelled".to_string()))
        }
        Err(super::proc::RunFail::Failed { stderr }) => Err(classify_failure(&stderr, host)),
        Err(super::proc::RunFail::Io(message)) => Err(GhError::Other(message)),
    }
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

/// A classified `gh` failure, mapped to a [`super::ForgeError`].
#[derive(Debug, PartialEq, Eq)]
enum GhError {
    NoGh,
    NotAuthed(String),
    Other(String),
}

impl From<GhError> for super::ForgeError {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NoGh => super::ForgeError::NoCli("gh"),
            GhError::NotAuthed(host) => {
                super::ForgeError::NotAuthed { forge: crate::git::Forge::GitHub, host }
            }
            GhError::Other(m) => super::ForgeError::Other(m),
        }
    }
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

/// The list filter for the open-PR resolve call. `first:100` keeps the surfaced ambiguity
/// count the real number of open PRs, not a cap.
const OPEN: &str = "states:OPEN, first:100";
/// The list filter for the historical fallback: the newest-created merged/closed PR per name.
const HISTORICAL: &str =
    "states:[MERGED,CLOSED], first:1, orderBy:{field:CREATED_AT, direction:DESC}";

/// All of one PR's state in a single direct GraphQL call — identity, mergeability, checks,
/// reviews, plain comments, and review threads. Each list caps at 100 rows — ample for any real
/// PR in a review sidebar — and its `pageInfo` flags a fuller surface so the UI can mark it,
/// rather than paging to exhaustion (`specs/forge-host.md`). `reviews` reads `last:100` to keep
/// the newest, so its "more exist" flag is `hasPreviousPage`; the `first:` lists use `hasNextPage`.
fn pr_detail(target: &FetchTarget<'_>, number: u64) -> Result<Value, GhError> {
    let q = format!(
        "query($o:String!,$n:String!){{repository(owner:$o,name:$n){{\
         pullRequest(number:{number}){{\
         number title url isDraft state mergeable mergeStateStatus baseRefName headRefName \
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
    repo: &std::path::Path,
    host: &str,
    query: &str,
    vars: &[(String, String)],
    cancelled: &std::sync::atomic::AtomicBool,
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
fn normalize_checks(rollup: &Value) -> Vec<super::Check> {
    let mut out: Vec<super::Check> = Vec::new();
    for node in rollup.as_array().into_iter().flatten() {
        let name =
            node["name"].as_str().or_else(|| node["context"].as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let status = check_status(node);
        // Latest wins: a later array entry for the same name (a re-run) replaces the earlier.
        if let Some(slot) = out.iter_mut().find(|c| c.name == name) {
            *slot = super::Check { name, status };
        } else {
            out.push(super::Check { name, status });
        }
    }
    out
}

/// Normalise one check node — a check run (`status`/`conclusion`) or a commit status (`state`)
/// — to a [`super::CheckStatus`].
fn check_status(node: &Value) -> super::CheckStatus {
    // Check runs carry `status`/`conclusion`; commit statuses carry `state`.
    if let Some(state) = node["state"].as_str() {
        return match state {
            "SUCCESS" => super::CheckStatus::Success,
            "FAILURE" | "ERROR" => super::CheckStatus::Failure,
            _ => super::CheckStatus::Pending,
        };
    }
    match node["status"].as_str() {
        Some("COMPLETED") => match node["conclusion"].as_str() {
            Some("SUCCESS") => super::CheckStatus::Success,
            Some("SKIPPED" | "NEUTRAL") => super::CheckStatus::Skipped,
            // FAILURE / TIMED_OUT / CANCELLED / ACTION_REQUIRED / a missing conclusion all read
            // as a failed check — something needs attention.
            _ => super::CheckStatus::Failure,
        },
        Some("IN_PROGRESS") => super::CheckStatus::Running,
        _ => super::CheckStatus::Pending,
    }
}

/// Merge the three comment surfaces (GraphQL `reviews`, `comments`, and `reviewThreads` node
/// arrays) into one newest-first list, keeping only a bot's latest PR-level post and each human's.
fn merge_comments(reviews: &Value, issues: &Value, threads: &Value) -> Vec<super::Comment> {
    let mut out: Vec<super::Comment> = Vec::new();

    // Submitted reviews with a non-empty body (the PR-level `review` cards).
    for r in reviews.as_array().into_iter().flatten() {
        let body = r["body"].as_str().unwrap_or("").trim().to_string();
        if body.is_empty() {
            continue;
        }
        out.push(prose_comment(
            super::CommentKind::Review,
            &r["author"],
            body,
            r["submittedAt"].as_str(),
        ));
    }

    // Plain conversation comments (the `comment` cards).
    for c in issues.as_array().into_iter().flatten() {
        let body = c["body"].as_str().unwrap_or("").trim().to_string();
        if body.is_empty() {
            continue;
        }
        out.push(prose_comment(
            super::CommentKind::Comment,
            &c["author"],
            body,
            c["createdAt"].as_str(),
        ));
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
        out.push(super::Comment {
            kind: super::CommentKind::Finding,
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
    kind: super::CommentKind,
    user: &Value,
    body: String,
    created_at: Option<&str>,
) -> super::Comment {
    let login = user["login"].as_str().unwrap_or("").to_string();
    let anchor = match kind {
        super::CommentKind::Review => "review",
        _ => "comment",
    };
    super::Comment {
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
fn dedup_bot_prose(out: &mut Vec<super::Comment>) {
    let mut keep_newest: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for c in out.iter() {
        if c.author_is_bot && c.kind != super::CommentKind::Finding {
            let e = keep_newest.entry(c.author.clone()).or_default();
            if c.created_at > *e {
                e.clone_from(&c.created_at);
            }
        }
    }
    out.retain(|c| {
        !(c.author_is_bot && c.kind != super::CommentKind::Finding)
            || keep_newest.get(&c.author) == Some(&c.created_at)
    });
}

/// Whether a GitHub login is an app/bot (`…[bot]`).
fn is_bot(login: &str) -> bool {
    login.ends_with("[bot]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{CheckStatus, CommentKind};

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
