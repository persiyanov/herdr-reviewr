//! The GitHub backend: association, detail, and normalization through `gh` GraphQL calls.
//!
//! See `specs/forge-host.md`. Everything GitHub-shaped lives here — the GraphQL queries,
//! the response JSON parsing, and `gh`'s stderr classification. The forge-agnostic
//! resolution logic (gates, picks, sync, the snapshot model) stays in `super`.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::{
    AssocPr, Association, Check, CheckStatus, CliError, Comment, CommentKind, FetchTarget, Merge,
    PrSnapshot, PrState, Sync,
};
use crate::git::Forge;

/// Run explicitly targeted `gh` arguments in `repo` and return stdout or a classified failure.
fn gh(repo: &Path, host: &str, args: &[&str], cancelled: &AtomicBool) -> Result<String, CliError> {
    super::run_cli("gh", repo, args, cancelled).map_err(|failure| match failure {
        super::CliFailure::Missing => CliError::Missing(Forge::GitHub),
        super::CliFailure::Other(message) => CliError::Other(message),
        super::CliFailure::Stderr(stderr) => classify_failure(&stderr, host),
    })
}

/// Map a failed `gh`'s stderr to a degraded state by its wording — `gh` has no stable exit
/// codes for these. An unrecognised failure is `Other` → a transient `Error` view.
fn classify_failure(stderr: &str, host: &str) -> CliError {
    let s = stderr.to_lowercase();
    if s.contains("not logged") || s.contains("authentication") || s.contains("gh auth login") {
        CliError::NotAuthed(Forge::GitHub, host.to_owned())
    } else {
        CliError::Other(stderr.trim().to_string())
    }
}

/// The closed-lookup aliases, one per `(point, tip name)` pair: `(alias, var, point index,
/// name)`. Build, vars, and parse all enumerate through this one owner, so the alias ↔
/// point pairing cannot drift between them. Capped at 8 pairs — a tip that many refs point
/// at (post-release coincidences, mirror refs) must not balloon the query past API limits.
fn closed_aliases(points: &[crate::git::PublicationPoint]) -> Vec<(String, String, usize, String)> {
    points
        .iter()
        .enumerate()
        .flat_map(|(i, point)| {
            point
                .names
                .iter()
                .enumerate()
                .map(move |(j, name)| (format!("c{i}_{j}"), format!("b{i}_{j}"), i, name.clone()))
        })
        .take(8)
        .collect()
}

/// Ask GitHub which PRs contain each publication point, in one aliased call against the
/// `source` repository, with the closed-unmerged name lookup against the target riding
/// along (`specs/forge-host.md`). Only PRs based on the target repository count.
pub(super) fn associate_points(
    target: &FetchTarget<'_>,
    source: &crate::git::RepoTarget,
    points: &[crate::git::PublicationPoint],
    absorbed: &[String],
    head: Option<&str>,
) -> Result<Association, CliError> {
    let closed = closed_aliases(points);
    let query =
        build_association_query(points.len() + absorbed.len() + head.iter().count(), &closed);
    let mut vars = vec![
        ("so".to_string(), source.owner().to_string()),
        ("sn".to_string(), source.name().to_string()),
        ("to".to_string(), target.owner.to_string()),
        ("tn".to_string(), target.name.to_string()),
    ];
    for (i, oid) in points
        .iter()
        .map(|p| p.oid.as_str())
        .chain(absorbed.iter().map(String::as_str))
        .chain(head)
        .enumerate()
    {
        vars.push((format!("p{i}"), oid.to_string()));
    }
    for (_, var, _, name) in &closed {
        vars.push((var.clone(), name.clone()));
    }
    let v = graphql(target.repo, target.host, &query, &vars, target.cancelled)?;
    Ok(parse_association(&v, points, absorbed, head, &closed))
}

/// The aliased association query: `p{i}: object(oid:$p{i})` per nominated OID — points,
/// absorbed candidates, then the exact-identity `HEAD` — against the source repository,
/// plus one closed-PR lookup per `(point, tip name)` pair against the target. The target block always carries `id` — the rename-proof base filter, and
/// the reason the block is never an empty selection set. Values ride as variables, never
/// in the query text.
fn build_association_query(oids: usize, closed: &[(String, String, usize, String)]) -> String {
    use std::fmt::Write;
    let mut q = String::from("query($so:String!,$sn:String!,$to:String!,$tn:String!");
    for i in 0..oids {
        let _ = write!(q, ",$p{i}:GitObjectID!");
    }
    for (_, var, _, _) in closed {
        let _ = write!(q, ",${var}:String!");
    }
    q.push_str("){src:repository(owner:$so,name:$sn){");
    for i in 0..oids {
        let _ = write!(
            q,
            "p{i}:object(oid:$p{i}){{... on Commit{{associatedPullRequests(first:100){{nodes{{\
             number state headRefOid headRefName createdAt mergedAt \
             baseRepository{{id}}}}}}}}}} "
        );
    }
    q.push_str("} tgt:repository(owner:$to,name:$tn){id ");
    for (alias, var, _, _) in closed {
        let _ = write!(
            q,
            "{alias}:pullRequests(headRefName:${var}, states:[CLOSED], first:10, \
             orderBy:{{field:CREATED_AT, direction:DESC}}){{nodes{{\
             number headRefOid headRefName createdAt}}}} "
        );
    }
    q.push_str("}}");
    q
}

/// Split the association response by lifecycle. Association nodes keep only PRs whose base
/// repository `id` equals the target's — ids survive renames and transfers, names do not.
/// Nodes from `absorbed` aliases are admitted only as a merged PR whose head is exactly an
/// absorbed commit — the parked epilogue. Nodes from the `head` alias are admitted only
/// when their head is exactly the pinned `HEAD` — the exact-identity epilogue. Closed
/// nodes keep only an exact head match to their point — identity, never a name
/// (`specs/forge-host.md`). Duplicates collapse.
fn parse_association(
    v: &Value,
    points: &[crate::git::PublicationPoint],
    absorbed: &[String],
    head: Option<&str>,
    closed: &[(String, String, usize, String)],
) -> Association {
    let mut assoc = Association::default();
    let target_id = v["data"]["tgt"]["id"].as_str().unwrap_or_default();
    let pr_of = |node: &Value| -> Option<AssocPr> {
        Some(AssocPr {
            number: node["number"].as_u64()?,
            head_oid: node["headRefOid"].as_str().unwrap_or_default().to_string(),
            head_ref: node["headRefName"].as_str().unwrap_or_default().to_string(),
            merged_at: node["mergedAt"].as_str().unwrap_or_default().to_string(),
            created_at: node["createdAt"].as_str().unwrap_or_default().to_string(),
        })
    };
    for i in 0..points.len() + absorbed.len() + head.iter().count() {
        let from_absorbed = i >= points.len() && i < points.len() + absorbed.len();
        let from_head = i >= points.len() + absorbed.len();
        let nodes = &v["data"]["src"][format!("p{i}").as_str()]["associatedPullRequests"]["nodes"];
        for node in nodes.as_array().into_iter().flatten() {
            let base = node["baseRepository"]["id"].as_str().unwrap_or_default();
            if base.is_empty() || base != target_id {
                continue;
            }
            let Some(pr) = pr_of(node) else { continue };
            if from_absorbed {
                // An absorbed commit is base history, which proves nothing by containment.
                // Only the exact parked epilogue is admissible: a merged PR whose head is
                // an absorbed commit itself.
                let exact = absorbed.iter().any(|oid| oid == &pr.head_oid);
                if exact && node["state"].as_str() == Some("MERGED") {
                    super::push_unique(&mut assoc.merged, pr);
                }
                continue;
            }
            if from_head {
                // The pinned HEAD is not published, so containment proves nothing. Only
                // exact identity admits: the worktree is parked on the PR's own head.
                if Some(pr.head_oid.as_str()) != head {
                    continue;
                }
            }
            match node["state"].as_str() {
                Some("OPEN") => super::push_unique(&mut assoc.open, pr),
                Some("MERGED") => super::push_unique(&mut assoc.merged, pr),
                _ => {}
            }
        }
    }
    for (alias, _, i, _) in closed {
        let nodes = &v["data"]["tgt"][alias.as_str()]["nodes"];
        for node in nodes.as_array().into_iter().flatten() {
            let Some(pr) = pr_of(node) else { continue };
            if pr.head_oid != points[*i].oid {
                continue;
            }
            super::push_unique(&mut assoc.closed, pr);
        }
    }
    assoc
}

/// All of one PR's state in a single direct GraphQL call — identity, mergeability, checks,
/// reviews, plain comments, and review threads. Each list surface reads its newest 100 rows
/// (`last:100`, flagged by `hasPreviousPage`) — ample for any real PR in a review sidebar —
/// and flags a fuller surface so the UI can mark it, rather than paging to exhaustion
/// (`specs/forge-host.md`). Checks keep `first:100`/`hasNextPage`. Returns the PR node and
/// its head OID, or `None` when the PR vanished between the association and this read.
pub(super) fn pr_detail(
    target: &FetchTarget<'_>,
    number: u64,
) -> Result<Option<(Value, String)>, CliError> {
    let q = build_detail_query(number);
    let vars = vec![
        ("o".to_string(), target.owner.to_string()),
        ("n".to_string(), target.name.to_string()),
    ];
    let mut v = graphql(target.repo, target.host, &q, &vars, target.cancelled)?;
    let node = v["data"]["repository"]["pullRequest"].take();
    if node.is_null() {
        return Ok(None);
    }
    let head = node["headRefOid"].as_str().unwrap_or_default().to_string();
    Ok(Some((node, head)))
}

/// Project one PR directly, including fork identity and capped check/comment surfaces.
fn build_detail_query(number: u64) -> String {
    format!(
        "query($o:String!,$n:String!){{repository(owner:$o,name:$n){{\
         pullRequest(number:{number}){{\
         number title url body isDraft state mergeable mergeStateStatus baseRefName headRefName \
         headRefOid isCrossRepository \
         commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:100){{pageInfo{{hasNextPage}} nodes{{__typename \
         ... on CheckRun{{name status conclusion}} ... on StatusContext{{context state}}}}}}}}}}}}}} \
         reviews(last:100){{pageInfo{{hasPreviousPage}} nodes{{author{{login}} body submittedAt}}}} \
         comments(last:100){{pageInfo{{hasPreviousPage}} nodes{{author{{login}} body createdAt}}}} \
         reviewThreads(last:100){{pageInfo{{hasPreviousPage}} nodes{{isResolved isOutdated path line \
         comments(first:1){{totalCount nodes{{author{{login}} body createdAt diffHunk}}}}}}}}}}}}}}"
    )
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
) -> Result<Value, CliError> {
    let args = graphql_args(host, query, vars);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = gh(repo, host, &arg_refs, cancelled)?;
    serde_json::from_str(&out).map_err(|e| CliError::Other(e.to_string()))
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

/// Assemble the snapshot from the `gh pr view` JSON, the computed `sync`, and the merged comments.
pub(super) fn build_snapshot(node: &Value, sync: Sync) -> PrSnapshot {
    let contexts = &node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
    let rollup = &contexts["nodes"];
    // A surface whose page reports more in the direction it pages is a prefix, not the whole set.
    // Each query asks only for its own flag — `hasPreviousPage` for the `last:` lists,
    // `hasNextPage` for checks — so OR-ing both reads whichever applies; the absent one is false.
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

    super::dedup_bot_prose(&mut out);
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

/// Whether a GitHub login is an app/bot (`…[bot]`).
fn is_bot(login: &str) -> bool {
    login.ends_with("[bot]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(oid: &str, names: &[&str]) -> crate::git::PublicationPoint {
        crate::git::PublicationPoint {
            oid: oid.to_string(),
            names: names.iter().map(|n| (*n).to_string()).collect(),
        }
    }

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

        // Comments and threads read `last:100`, so their "more exist" flag pages backward.
        let mut comments_more = base.clone();
        comments_more["comments"]["pageInfo"]["hasPreviousPage"] = serde_json::json!(true);
        assert!(build_snapshot(&comments_more, Sync::InSync).truncated);

        let mut threads_more = base.clone();
        threads_more["reviewThreads"]["pageInfo"]["hasPreviousPage"] = serde_json::json!(true);
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
    fn absorbed_aliases_admit_only_an_exact_head_merged_pr() {
        // The parked epilogue: the worktree sits on base history, so containment proves
        // nothing — only a merged PR whose head IS the absorbed commit resolves.
        let v = serde_json::json!({"data": {
            "src": {
                "p0": {"associatedPullRequests": {"nodes": [
                    {"number": 82, "state": "MERGED", "headRefOid": "parked", "headRefName": "fix",
                     "createdAt": "2026-07-01T00:00:00Z", "mergedAt": "2026-07-02T00:00:00Z",
                     "baseRepository": {"id": "R1"}},
                    {"number": 90, "state": "MERGED", "headRefOid": "other", "headRefName": "else",
                     "createdAt": "2026-07-01T00:00:00Z", "mergedAt": "2026-07-03T00:00:00Z",
                     "baseRepository": {"id": "R1"}},
                    {"number": 91, "state": "OPEN", "headRefOid": "parked", "headRefName": "fix",
                     "createdAt": "2026-07-01T00:00:00Z", "mergedAt": null,
                     "baseRepository": {"id": "R1"}}
                ]}}
            },
            "tgt": {"id": "R1"}
        }});
        let absorbed = vec!["parked".to_string()];
        let a = parse_association(&v, &[], &absorbed, None, &[]);
        // #90 contains the commit but its head is a stranger's; #91 is open — neither admits.
        assert_eq!(a.merged.iter().map(|p| p.number).collect::<Vec<_>>(), [82]);
        assert!(a.open.is_empty());
    }

    fn assoc_node(number: u64, state: &str, head: &str) -> serde_json::Value {
        serde_json::json!({"number": number, "state": state, "headRefOid": head,
            "headRefName": "feat", "createdAt": "2026-07-01T00:00:00Z",
            "mergedAt": null, "baseRepository": {"id": "R1"}})
    }

    #[test]
    fn head_alias_admits_only_exact_head_matches_open_or_merged() {
        // The exact-identity epilogue: the pinned HEAD nominates, so a PR whose head IS
        // that commit admits — open or merged. A PR that merely contains it does not,
        // and closed-unmerged PRs never associate on the forge at all.
        let v = serde_json::json!({"data": {
            "src": {"p0": {"associatedPullRequests": {"nodes": [
                assoc_node(10, "MERGED", "tip"),
                assoc_node(11, "OPEN", "tip"),
                assoc_node(12, "CLOSED", "tip"),
                assoc_node(13, "MERGED", "other"),
                assoc_node(14, "OPEN", "other")
            ]}}},
            "tgt": {"id": "R1"}
        }});
        let a = parse_association(&v, &[], &[], Some("tip"), &[]);
        assert_eq!(a.open.iter().map(|p| p.number).collect::<Vec<_>>(), [11]);
        assert_eq!(a.merged.iter().map(|p| p.number).collect::<Vec<_>>(), [10]);
        assert!(a.closed.is_empty(), "the exact-identity path never fills closed");
    }

    #[test]
    fn alias_index_classes_stay_aligned_with_all_three_sources_present() {
        // One point, one absorbed candidate, and the head alias in one response: each
        // index must land in its own admission class, not a neighbor's.
        let v = serde_json::json!({"data": {
            "src": {
                "p0": {"associatedPullRequests": {"nodes": [assoc_node(20, "OPEN", "elsewhere")]}},
                "p1": {"associatedPullRequests": {"nodes": [assoc_node(21, "MERGED", "parked")]}},
                "p2": {"associatedPullRequests": {"nodes": [assoc_node(22, "MERGED", "tip")]}}
            },
            "tgt": {"id": "R1"}
        }});
        let points = vec![point("published", &[])];
        let absorbed = vec!["parked".to_string()];
        let a = parse_association(&v, &points, &absorbed, Some("tip"), &[]);
        assert_eq!(a.open.iter().map(|p| p.number).collect::<Vec<_>>(), [20], "containment");
        assert_eq!(a.merged.iter().map(|p| p.number).collect::<Vec<_>>(), [21, 22], "exact");
    }

    #[test]
    fn association_query_aliases_points_and_closed_names_and_never_inlines_values() {
        let points = vec![point("aaa", &[]), point("bbb", &["feat", "backup"])];
        let closed = closed_aliases(&points);
        assert_eq!(
            closed
                .iter()
                .map(|(alias, var, i, name)| (alias.as_str(), var.as_str(), *i, name.as_str()))
                .collect::<Vec<_>>(),
            [("c1_0", "b1_0", 1, "feat"), ("c1_1", "b1_1", 1, "backup")]
        );
        let q = build_association_query(points.len(), &closed);
        assert!(q.starts_with(
            "query($so:String!,$sn:String!,$to:String!,$tn:String!,\
             $p0:GitObjectID!,$p1:GitObjectID!,$b1_0:String!,$b1_1:String!)"
        ));
        assert!(q.contains("src:repository(owner:$so,name:$sn){p0:object(oid:$p0)"));
        assert!(q.contains("associatedPullRequests(first:100)"));
        assert!(q.contains("baseRepository{id}"));
        assert!(q.contains("tgt:repository(owner:$to,name:$tn){id "));
        assert!(q.contains("c1_0:pullRequests(headRefName:$b1_0, states:[CLOSED]"));
        assert!(q.contains("c1_1:pullRequests(headRefName:$b1_1, states:[CLOSED]"));
        // With no named point, the target block still carries `id` — never an empty
        // selection set, which GitHub rejects as a parse error.
        let bare = vec![point("aaa", &[])];
        let q = build_association_query(bare.len(), &closed_aliases(&bare));
        assert!(q.contains("tgt:repository(owner:$to,name:$tn){id }"));
    }

    #[test]
    fn parse_association_splits_lifecycles_filters_by_repo_id_and_dedups() {
        let v = serde_json::json!({"data": {
            "src": {
                "p0": {"associatedPullRequests": {"nodes": [
                    {"number": 7, "state": "OPEN", "headRefOid": "abc", "headRefName": "feat-x",
                     "createdAt": "2026-07-01T00:00:00Z", "mergedAt": null,
                     "baseRepository": {"id": "R1"}},
                    {"number": 8, "state": "MERGED", "headRefOid": "def", "headRefName": "feat-y",
                     "createdAt": "2026-06-01T00:00:00Z", "mergedAt": "2026-06-02T00:00:00Z",
                     "baseRepository": {"id": "R1"}},
                    {"number": 9, "state": "OPEN", "headRefOid": "zzz", "headRefName": "other",
                     "createdAt": "2026-07-01T00:00:00Z", "mergedAt": null,
                     "baseRepository": {"id": "R-other"}}
                ]}},
                "p1": {"associatedPullRequests": {"nodes": [
                    {"number": 7, "state": "OPEN", "headRefOid": "abc", "headRefName": "feat-x",
                     "createdAt": "2026-07-01T00:00:00Z", "mergedAt": null,
                     "baseRepository": {"id": "R1"}}
                ]}}
            },
            "tgt": {
                "id": "R1",
                "c1_0": {"nodes": [
                    {"number": 5, "headRefOid": "p1oid", "headRefName": "old-name",
                     "createdAt": "2026-05-01T00:00:00Z"},
                    {"number": 6, "headRefOid": "impostor", "headRefName": "old-name",
                     "createdAt": "2026-05-02T00:00:00Z"}
                ]}
            }
        }});
        let points = vec![point("p0oid", &[]), point("p1oid", &["old-name"])];
        let a = parse_association(&v, &points, &[], None, &closed_aliases(&points));
        // Open #7 appears under both points but lands once; #9 based elsewhere is dropped.
        assert_eq!(a.open.iter().map(|p| p.number).collect::<Vec<_>>(), [7]);
        assert_eq!(a.merged.iter().map(|p| p.number).collect::<Vec<_>>(), [8]);
        // The closed lookup admits only an exact head match to the point.
        assert_eq!(a.closed.iter().map(|p| p.number).collect::<Vec<_>>(), [5]);
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
            CliError::NotAuthed(Forge::GitHub, "github.example.com".to_string())
        );
        assert_eq!(
            classify_failure("You are not logged into any GitHub hosts", "github.com"),
            CliError::NotAuthed(Forge::GitHub, "github.com".to_string())
        );
        assert_eq!(
            classify_failure("HTTP 500 something", "github.com"),
            CliError::Other("HTTP 500 something".into())
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
