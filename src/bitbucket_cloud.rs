//! The read-only Bitbucket Cloud provider behind `src/forge.rs`, through `bkt api`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::forge::{
    AssocPr, Association, Check, CheckStatus, Comment, CommentKind, Merge, PrFetchInput,
    PrSnapshot, PrState, PrView, Reply, Sync, finish_comments, prose_row, push_unique,
    upsert_latest,
};

/// Read Bitbucket Cloud for one already-derived input. Degradation stays in-band for the PR tab.
pub(crate) fn fetch(
    repo: &Path,
    input: &PrFetchInput,
    target: &crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> PrView {
    match fetch_inner(repo, input, target, cancelled) {
        Ok(view) => view,
        Err(error) => error.into_view(target),
    }
}

/// A classified `bkt` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug, PartialEq, Eq)]
enum BktError {
    NoBkt,
    NotAuthed,
    /// `bkt` is signed in to Bitbucket Cloud under this host key, but no context names it.
    NoContext(String),
    /// A 403 or 404: the addressed object is unknown or unreadable.
    Unavailable(String),
    LocalGit(String),
    Other(String),
}

impl BktError {
    fn into_view(self, target: &crate::git::RepoTarget) -> PrView {
        let forge = crate::git::Forge::BitbucketCloud;
        let (host, workspace) = (target.host(), target.owner());
        match self {
            Self::NoBkt => PrView::NoCli(forge),
            Self::NotAuthed => PrView::NotAuthed(forge, host.to_owned()),
            // `bkt` refuses a Cloud context without a workspace, so the remedy names this one.
            Self::NoContext(key) => PrView::Error(
                forge,
                format!(
                    "no bkt context for {host}; run \
                     `bkt context create {workspace} --host {key} --workspace {workspace}`"
                ),
            ),
            Self::LocalGit(message) => PrView::GitError(message),
            Self::Unavailable(message) | Self::Other(message) => PrView::Error(forge, message),
        }
    }
}

/// The retryable error a panicked reader degrades into (`crate::forge::join_read`).
fn died(surface: &str) -> BktError {
    BktError::Other(format!("{surface} read panicked"))
}

/// An unreadable optional surface reads as empty instead of failing the fetch.
fn optional_surface(result: Result<Value, BktError>) -> Result<Value, BktError> {
    match result {
        Err(BktError::Unavailable(_)) => Ok(Value::Null),
        other => other,
    }
}

/// One fetch's `bkt` runs: where they run, the context they pin, and their cancel flag.
#[derive(Clone, Copy)]
struct Bkt<'a> {
    repo: &'a Path,
    context: Option<&'a str>,
    cancelled: &'a AtomicBool,
}

/// One query parameter, passed as `--param` so `bkt` encodes the value.
type Param<'a> = (&'a str, String);

/// One API read's argv: `--context` beats the active context, which may name another host.
fn bkt_args(context: Option<&str>, path: &str, params: &[Param<'_>]) -> Vec<String> {
    let mut args = vec!["api".to_string(), path.to_owned()];
    for (key, value) in params {
        args.push("--param".to_string());
        args.push(format!("{key}={value}"));
    }
    if let Some(context) = context {
        args.push(format!("--context={context}"));
    }
    args
}

/// Run one `bkt` command and parse the JSON it prints.
fn bkt_json(repo: &Path, args: &[String], cancelled: &AtomicBool) -> Result<Value, BktError> {
    let mut cmd = crate::proc::command("bkt");
    cmd.current_dir(repo).args(args);
    let stdout = crate::forge::run_provider(
        cmd,
        cancelled,
        BktError::NoBkt,
        classify_failure,
        BktError::Other,
    )?;
    serde_json::from_str(stdout.trim()).map_err(|error| BktError::Other(error.to_string()))
}

/// Run one `bkt api` read under the fetch's context.
fn bkt_api(bkt: Bkt<'_>, path: &str, params: &[Param<'_>]) -> Result<Value, BktError> {
    bkt_json(bkt.repo, &bkt_args(bkt.context, path, params), bkt.cancelled)
}

/// A failed `bkt`'s state by its `Error: <status> <text>` line, else by its wording.
fn classify_failure(stderr: &str) -> BktError {
    let s = stderr.to_lowercase();
    let status = s.lines().find_map(|line| {
        let line = line.trim().strip_prefix("error: ").unwrap_or(line.trim());
        let (code, rest) = line.split_at_checked(3)?;
        (rest.is_empty() || rest.starts_with(' ')).then(|| code.parse::<u16>().ok())?
    });
    // A 403 naming scopes is an under-scoped token: only a new login unblocks it.
    let under_scoped = status == Some(403) && s.contains("scope");
    if status == Some(401)
        || under_scoped
        || s.contains("auth login")
        || s.contains("no active context")
        || s.contains("no hosts configured")
        || s.contains("bkt_token")
        || s.contains("bkt_username")
    {
        BktError::NotAuthed
    } else if matches!(status, Some(403 | 404)) {
        BktError::Unavailable(stderr.trim().to_string())
    } else {
        BktError::Other(stderr.trim().to_string())
    }
}

/// The context to pin, read from `bkt auth status`; `None` leaves the choice to `bkt`.
fn cloud_context(repo: &Path, cancelled: &AtomicBool) -> Result<Option<String>, BktError> {
    let args = ["auth", "status", "--json"].map(str::to_string);
    pick_context(&bkt_json(repo, &args, cancelled)?)
}

/// The active context on a Bitbucket Cloud host, else the first one by name.
fn pick_context(status: &Value) -> Result<Option<String>, BktError> {
    let hosts = status["hosts"].as_array().map(Vec::as_slice).unwrap_or_default();
    let contexts = status["contexts"].as_array().map(Vec::as_slice).unwrap_or_default();
    // Nothing saved is `BKT_TOKEN` mode, which only `bkt` itself can resolve.
    if hosts.is_empty() && contexts.is_empty() {
        return Ok(None);
    }
    let cloud: Vec<&str> = hosts
        .iter()
        .filter(|host| host["kind"].as_str() == Some("cloud"))
        .filter_map(|host| host["key"].as_str())
        .collect();
    let Some(key) = cloud.first() else {
        return Err(BktError::NotAuthed);
    };
    let on_cloud = |context: &&Value| context["host"].as_str().is_some_and(|h| cloud.contains(&h));
    let pick = contexts
        .iter()
        .filter(on_cloud)
        .find(|context| context["active"].as_bool() == Some(true))
        .or_else(|| contexts.iter().find(on_cloud));
    match pick.and_then(|context| context["name"].as_str()) {
        Some(name) => Ok(Some(name.to_string())),
        None => Err(BktError::NoContext((*key).to_string())),
    }
}

/// A repository's pull request collection, or one pull request under it.
fn pulls_path(repository: &crate::git::RepoTarget) -> String {
    format!("/repositories/{}/{}/pullrequests", repository.owner(), repository.name())
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    target: &crate::git::RepoTarget,
    cancelled: &AtomicBool,
) -> Result<PrView, BktError> {
    let context = cloud_context(repo, cancelled)?;
    let bkt = Bkt { repo, context: context.as_deref(), cancelled };
    let Some((id, home)) = associate_by_branch(bkt, input, target)? else {
        return Ok(PrView::NoPr);
    };
    let base = format!("{}/{id}", pulls_path(home));

    // The three surfaces read concurrently.
    let (pr, comments, statuses) = std::thread::scope(|scope| {
        let pr = scope.spawn(|| bkt_api(bkt, &base, &[]));
        let comments = scope.spawn(|| {
            // Newest first, so one page is the newest hundred.
            // The listing leaves a finding's quoted hunk out unless asked, and never says outdated.
            let page = [
                ("pagelen", "100".to_string()),
                ("sort", "-created_on".to_string()),
                ("fields", "+values.inline.context_lines".to_string()),
            ];
            bkt_api(bkt, &format!("{base}/comments"), &page)
        });
        let statuses = scope.spawn(|| {
            let page = [("pagelen", "100".to_string())];
            optional_surface(bkt_api(bkt, &format!("{base}/statuses"), &page))
        });
        (
            crate::forge::join_read(pr, || died("pull request")),
            crate::forge::join_read(comments, || died("comments")),
            crate::forge::join_read(statuses, || died("statuses")),
        )
    });
    let pr = pr?;
    let comments = comments?;
    let statuses = statuses?;
    if pr["id"].as_u64().is_none() {
        return Ok(PrView::NoPr);
    }
    let head_oid = source_oid(repo, &pr);
    // Against the pinned HEAD, so a mid-fetch checkout never mixes two branches.
    let sync = crate::forge::local_sync(repo, input.local.head_oid.as_deref(), &head_oid)
        .map_err(|error| BktError::LocalGit(error.0))?;
    Ok(PrView::Pr(Box::new(build_snapshot(&pr, head_oid, sync, &comments, &statuses))))
}

/// The source tip in full: Bitbucket abbreviates it, so an unfetched tip reads as unknown.
fn source_oid(repo: &Path, node: &Value) -> String {
    let short = node["source"]["commit"]["hash"].as_str().unwrap_or_default();
    crate::git::full_oid(repo, short).unwrap_or_default()
}

/// The branch's pull request and its repository; a fork clone asks both, and upstream's wins.
fn associate_by_branch<'a>(
    bkt: Bkt<'_>,
    input: &'a PrFetchInput,
    target: &'a crate::git::RepoTarget,
) -> Result<Option<(u64, &'a crate::git::RepoTarget)>, BktError> {
    let names = input.local.head_names();
    if names.is_empty() {
        return Ok(None);
    }
    let head = input.local.head_oid.as_deref();
    let fork = crate::forge::fork_repository(input.origin_repository.as_ref(), target);
    for queried in std::iter::once(target).chain(fork) {
        let rows = match branch_lookup(bkt, queried, &names) {
            // An unreadable fork proves nothing; an unreadable target is the failure.
            Err(BktError::Unavailable(_)) if !queried.is(target) => continue,
            rows => rows?,
        };
        let mut assoc = collect_assoc(&rows, queried, &input.local.heads);
        // Only the history pick reads a tip, so only it pays for the full ids.
        if assoc.open.is_empty() {
            for pr in &mut assoc.history {
                pr.head_oid = crate::git::full_oid(bkt.repo, &pr.head_oid).unwrap_or_default();
            }
        }
        if let Some(id) = crate::forge::resolve_pick(bkt.repo, &assoc, head)
            .map_err(|error| BktError::LocalGit(error.0))?
        {
            return Ok(Some((id, queried)));
        }
    }
    Ok(None)
}

/// Every listing for `names` in one repository, read concurrently.
fn branch_lookup(
    bkt: Bkt<'_>,
    queried: &crate::git::RepoTarget,
    names: &[String],
) -> Result<Vec<Value>, BktError> {
    let path = pulls_path(queried);
    let listings = branch_listings(names);
    std::thread::scope(|scope| {
        let handles: Vec<_> =
            listings.iter().map(|params| scope.spawn(|| bkt_api(bkt, &path, params))).collect();
        handles.into_iter().map(|h| crate::forge::join_read(h, || died("listing"))).collect()
    })
}

/// Per name, an open page and a finished page, so history never buries an open pull request.
fn branch_listings(names: &[String]) -> Vec<Vec<Param<'static>>> {
    let fields = "values.id,values.state,values.created_on,values.updated_on,\
                  values.source.branch.name,values.source.commit.hash,\
                  values.source.repository.full_name,values.source.repository.uuid,\
                  values.destination.repository.uuid";
    names
        .iter()
        .flat_map(|name| {
            let branch = format!("source.branch.name = \"{}\"", bbql_escape(name));
            let listing = |states: &str, sort: &str, pagelen: &str| {
                vec![
                    ("q", format!("{branch} AND {states}")),
                    ("sort", sort.to_string()),
                    ("pagelen", pagelen.to_string()),
                    ("fields", fields.to_string()),
                ]
            };
            [
                listing("state = \"OPEN\"", "-created_on", "20"),
                // Ten is every finished row the pick consults.
                listing("state IN (\"MERGED\", \"DECLINED\", \"SUPERSEDED\")", "-updated_on", "10"),
            ]
        })
        .collect()
}

/// A branch name inside a quoted query string.
fn bbql_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The listings' admitted pull requests by lifecycle.
fn collect_assoc(
    listings: &[Value],
    queried: &crate::git::RepoTarget,
    heads: &[crate::git::Head],
) -> Association {
    let mut assoc = Association::default();
    for node in listings.iter().flat_map(|page| page["values"].as_array().into_iter().flatten()) {
        let Some(pr) = assoc_pr(node) else { continue };
        if !crate::forge::admits(heads, queried, &pr.head_ref, &head_repo(node, queried).as_ref()) {
            continue;
        }
        match node["state"].as_str().unwrap_or_default() {
            "OPEN" => push_unique(&mut assoc.open, pr),
            _ => push_unique(&mut assoc.history, pr),
        }
    }
    assoc
}

/// Where a node's source branch lives: the queried repository, or the fork it names.
enum SourceRepo {
    Queried,
    Other(Option<crate::git::RepoTarget>),
}

impl SourceRepo {
    fn as_ref(&self) -> crate::forge::HeadRepo<'_> {
        match self {
            Self::Queried => crate::forge::HeadRepo::Queried,
            Self::Other(fork) => crate::forge::HeadRepo::Other(fork.iter().collect()),
        }
    }
}

/// The source repository by uuid, which survives a rename; a deleted fork names none.
fn head_repo(node: &Value, queried: &crate::git::RepoTarget) -> SourceRepo {
    if !is_cross_repository(node) {
        return SourceRepo::Queried;
    }
    let full_name = node["source"]["repository"]["full_name"].as_str();
    SourceRepo::Other(full_name.and_then(|full| full.split_once('/')).and_then(|(owner, name)| {
        crate::git::RepoTarget::with_path(
            crate::git::Forge::BitbucketCloud,
            queried.host(),
            &[owner, name],
        )
    }))
}

/// Whether the source lives outside the destination repository; a deleted fork does.
fn is_cross_repository(node: &Value) -> bool {
    let uuid = |side: &str| node[side]["repository"]["uuid"].as_str();
    match (uuid("source"), uuid("destination")) {
        (Some(source), Some(destination)) => source != destination,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// One listing node reduced to the pick-relevant fields shared with the other providers.
fn assoc_pr(node: &Value) -> Option<AssocPr> {
    let finished = node["state"].as_str() != Some("OPEN");
    Some(AssocPr {
        number: node["id"].as_u64()?,
        // Abbreviated here; the history pick expands it.
        head_oid: node["source"]["commit"]["hash"].as_str().unwrap_or_default().to_string(),
        head_ref: node["source"]["branch"]["name"].as_str().unwrap_or_default().to_string(),
        created_at: node["created_on"].as_str().unwrap_or_default().to_string(),
        // Bitbucket reports no close time; a finished pull request's last update stands in.
        closed_at: match node["updated_on"].as_str() {
            Some(updated) if finished => updated.to_string(),
            _ => String::new(),
        },
        // A listing node is a reduced row, never the full pull request.
        raw: None,
    })
}

// ---- Pure normalization (unit-tested) --------------------------------------------------

/// Assemble the snapshot from the pull request, its comments page, and its statuses page.
fn build_snapshot(
    pr: &Value,
    head_oid: String,
    sync: Sync,
    comments: &Value,
    statuses: &Value,
) -> PrSnapshot {
    PrSnapshot {
        number: pr["id"].as_u64().unwrap_or_default(),
        title: pr["title"].as_str().unwrap_or_default().to_string(),
        url: pr["links"]["html"]["href"].as_str().unwrap_or_default().to_string(),
        body: pr["description"].as_str().unwrap_or_default().to_string(),
        // A missing state reads as closed, never reviewable.
        state: parse_state(pr["state"].as_str().unwrap_or_default()),
        is_draft: pr["draft"].as_bool().unwrap_or(false),
        head_ref: pr["source"]["branch"]["name"].as_str().unwrap_or_default().to_string(),
        head_is_fork: is_cross_repository(pr),
        head_oid,
        base_ref: pr["destination"]["branch"]["name"].as_str().unwrap_or_default().to_string(),
        // Bitbucket's API reports no conflict or merge-check state.
        merge: Merge::Clean,
        sync,
        checks: build_checks(statuses),
        comments: merge_comments(comments, pr),
        comments_truncated: page_capped(comments),
        checks_truncated: page_capped(statuses),
    }
}

/// `DECLINED` and `SUPERSEDED` both read as closed.
fn parse_state(state: &str) -> PrState {
    match state {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        _ => PrState::Closed,
    }
}

/// Whether a one-page read hides rows: a next page, or a size past the page.
fn page_capped(page: &Value) -> bool {
    let rows = page["values"].as_array().map_or(0, Vec::len) as u64;
    page["next"].is_string() || page["size"].as_u64().is_some_and(|size| size > rows)
}

/// The commit's build statuses as checks, the latest report per name.
fn build_checks(statuses: &Value) -> Vec<Check> {
    let mut rows: Vec<&Value> = statuses["values"].as_array().into_iter().flatten().collect();
    // Oldest first, so a re-run replaces its earlier report.
    rows.sort_by_key(|row| row["updated_on"].as_str().unwrap_or_default());
    let mut checks: Vec<Check> = Vec::new();
    for row in rows {
        let name = row["name"].as_str().filter(|name| !name.is_empty()).or(row["key"].as_str());
        let Some(name) = name else { continue };
        let status = build_status(row["state"].as_str().unwrap_or_default());
        upsert_latest(&mut checks, Check { name: name.to_string(), status });
    }
    checks
}

/// Normalise one build status state to a [`CheckStatus`].
fn build_status(state: &str) -> CheckStatus {
    match state {
        "SUCCESSFUL" => CheckStatus::Success,
        // A stopped build never passed, so it needs attention like a failed one.
        "FAILED" | "STOPPED" => CheckStatus::Failure,
        "INPROGRESS" => CheckStatus::Running,
        _ => CheckStatus::Pending,
    }
}

/// Whether a comment renders: published, not deleted, with content.
fn is_comment(comment: &Value) -> bool {
    !comment["deleted"].as_bool().unwrap_or(false)
        && !comment["pending"].as_bool().unwrap_or(false)
        && !body_of(comment).is_empty()
}

fn body_of(comment: &Value) -> &str {
    comment["content"]["raw"].as_str().unwrap_or("").trim()
}

/// The author's display name, and whether the account is an app or reads as a bot.
fn author_of(user: &Value) -> (String, bool) {
    let name = user["display_name"].as_str().or(user["nickname"].as_str()).unwrap_or("");
    let bot = user["type"].as_str() == Some("app_user") || crate::forge::is_named_bot(name);
    (name.to_string(), bot)
}

/// The id of the comment a thread hangs from, following `parent` as far as the page reaches.
fn thread_key(comment: &Value, by_id: &HashMap<u64, &Value>) -> Option<u64> {
    let mut key = comment["id"].as_u64()?;
    let mut at = comment;
    // Bounded by the page, so a parent cycle cannot spin.
    for _ in 0..by_id.len() {
        let Some(parent) = at["parent"]["id"].as_u64() else { break };
        key = parent;
        let Some(next) = by_id.get(&parent) else { break };
        at = next;
    }
    Some(key)
}

/// An inline comment's place: the new side by `to`, else the old side by `from`.
fn inline_place(inline: &Value) -> crate::forge::FindingPlace {
    let path = inline["path"].as_str().unwrap_or("");
    let (to, from) = (inline["to"].as_u64(), inline["from"].as_u64());
    let (start, end) = if to.is_some() {
        (inline["start_to"].as_u64().or(to), to)
    } else {
        (inline["start_from"].as_u64().or(from), from)
    };
    let side = crate::forge::finding_side(to.is_some(), from.is_some());
    crate::forge::FindingPlace::from_lines(path, start, end, side)
}

/// A finding's quoted hunk, from its header on: `context_lines` leads with the file's `---`/`+++` pair.
fn quoted_hunk(inline: &Value) -> Option<String> {
    let lines = inline["context_lines"].as_str()?;
    let at = if lines.starts_with("@@ ") { 0 } else { lines.find("\n@@ ")? + 1 };
    Some(lines[at..].to_string())
}

/// Comment threads and participant verdicts as one newest-first list.
fn merge_comments(comments: &Value, pr: &Value) -> Vec<Comment> {
    let rows = comments["values"].as_array().map(Vec::as_slice).unwrap_or_default();
    let by_id: HashMap<u64, &Value> =
        rows.iter().filter_map(|row| Some((row["id"].as_u64()?, row))).collect();
    let mut keys: Vec<u64> = Vec::new();
    let mut threads: HashMap<u64, Vec<&Value>> = HashMap::new();
    for row in rows {
        let Some(key) = thread_key(row, &by_id) else { continue };
        if !threads.contains_key(&key) {
            keys.push(key);
        }
        threads.entry(key).or_default().push(row);
    }

    let mut out: Vec<Comment> = Vec::new();
    for key in keys {
        let mut notes: Vec<&Value> = threads.remove(&key).unwrap_or_default();
        notes.retain(|note| is_comment(note));
        notes.sort_by_key(|note| note["created_on"].as_str().unwrap_or_default());
        let Some((root, replies)) = notes.split_first() else { continue };
        // The thread's own head carries its place and resolution, even deleted or off the page.
        let head = by_id.get(&key).copied().unwrap_or(root);
        let inline = if head["inline"].is_object() { &head["inline"] } else { &root["inline"] };
        let (author, author_is_bot) = author_of(&root["user"]);
        let place = inline.is_object().then(|| inline_place(inline));
        out.push(Comment {
            kind: if place.is_some() { CommentKind::Finding } else { CommentKind::Comment },
            author_is_bot,
            author,
            anchor: place
                .as_ref()
                .map_or_else(|| "comment".to_string(), crate::forge::FindingPlace::anchor),
            place,
            body: body_of(root).to_string(),
            snippet: quoted_hunk(inline),
            created_at: root["created_on"].as_str().unwrap_or("").to_string(),
            is_resolved: head["resolution"].is_object(),
            is_outdated: false,
            replies: replies
                .iter()
                .map(|note| {
                    let (author, author_is_bot) = author_of(&note["user"]);
                    Reply {
                        author,
                        author_is_bot,
                        body: body_of(note).to_string(),
                        created_at: note["created_on"].as_str().unwrap_or("").to_string(),
                    }
                })
                .collect(),
        });
    }
    for participant in pr["participants"].as_array().into_iter().flatten() {
        let body = match participant["state"].as_str() {
            Some("approved") => "Approved this pull request.",
            Some("changes_requested") => "Requested changes.",
            _ => continue,
        };
        let (author, bot) = author_of(&participant["user"]);
        if author.is_empty() {
            continue;
        }
        let at = participant["participated_on"].as_str().unwrap_or("").to_string();
        out.push(prose_row(CommentKind::Review, author, bot, body.to_string(), at));
    }
    finish_comments(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bb(path: &[&str]) -> crate::git::RepoTarget {
        let forge = crate::git::Forge::BitbucketCloud;
        crate::git::RepoTarget::with_path(forge, "bitbucket.org", path).unwrap()
    }

    fn head(repo: &crate::git::RepoTarget, name: &str) -> crate::git::Head {
        crate::git::Head { repo: repo.clone(), name: name.to_string() }
    }

    fn listed(id: u64, state: &str, branch: &str, source: &str, destination: &str) -> Value {
        json!({
            "id": id, "state": state,
            "created_on": "2026-09-13T02:10:33.813593+00:00",
            "updated_on": "2026-10-05T02:55:43.275809+00:00",
            "source": {
                "branch": {"name": branch}, "commit": {"hash": "fe55684f21d5"},
                "repository": {"full_name": "contributor/app", "uuid": source},
            },
            "destination": {"repository": {"uuid": destination}},
        })
    }

    fn note(id: u64, at: &str, body: &str) -> Value {
        json!({
            "id": id, "created_on": at, "deleted": false, "pending": false,
            "content": {"raw": body},
            "user": {"display_name": "Mario", "nickname": "mario", "type": "user"},
        })
    }

    #[test]
    fn bkt_failures_classify_by_status_line_and_login_wording() {
        assert_eq!(classify_failure("Error: 401 Unauthorized\n"), BktError::NotAuthed);
        let no_context = "Error: no active context; run `bkt context use <name>`\n";
        assert_eq!(classify_failure(no_context), BktError::NotAuthed);
        let no_login = "Error: credentials for host \"x\" not found; run `bkt auth login x`";
        assert_eq!(classify_failure(no_login), BktError::NotAuthed);
        let scopes = "Error: 403 Forbidden: Your credentials lack one or more required scopes";
        assert_eq!(classify_failure(scopes), BktError::NotAuthed);
        assert!(matches!(classify_failure("Error: 404 Not Found"), BktError::Unavailable(_)));
        assert!(matches!(classify_failure("Error: 403 Forbidden"), BktError::Unavailable(_)));
        // A status inside a path or an id is no status.
        let other = classify_failure("Error: parse request path /pullrequests/401");
        assert!(matches!(other, BktError::Other(_)));
    }

    #[test]
    fn every_read_pins_the_cloud_context_and_passes_values_as_params() {
        let page = [("pagelen", "100".to_string())];
        assert_eq!(
            bkt_args(Some("cloud"), "/repositories/team/app/pullrequests", &page),
            [
                "api",
                "/repositories/team/app/pullrequests",
                "--param",
                "pagelen=100",
                "--context=cloud"
            ]
        );
        assert_eq!(bkt_args(None, "/user", &[]), ["api", "/user"]);
    }

    #[test]
    fn the_pinned_context_is_the_active_cloud_one_else_the_first_cloud_one() {
        let status = |active: &str| {
            json!({
                "hosts": [
                    {"key": "api.bitbucket.org", "kind": "cloud"},
                    {"key": "bitbucket.corp.example", "kind": "dc"},
                ],
                "contexts": [
                    {"name": "a-work", "host": "bitbucket.corp.example", "active": active == "a-work"},
                    {"name": "b-cloud", "host": "api.bitbucket.org", "active": false},
                    {"name": "c-cloud", "host": "api.bitbucket.org", "active": active == "c-cloud"},
                ],
            })
        };
        assert_eq!(pick_context(&status("c-cloud")), Ok(Some("c-cloud".to_string())));
        // A Data Center context being active never sends a Cloud read to that server.
        assert_eq!(pick_context(&status("a-work")), Ok(Some("b-cloud".to_string())));
    }

    #[test]
    fn a_missing_cloud_login_or_context_is_reported_and_an_empty_config_is_left_to_bkt() {
        let dc_only = json!({
            "hosts": [{"key": "bitbucket.corp.example", "kind": "dc"}],
            "contexts": [{"name": "work", "host": "bitbucket.corp.example", "active": true}],
        });
        assert_eq!(pick_context(&dc_only), Err(BktError::NotAuthed));
        let no_context = json!({
            "hosts": [{"key": "api.bitbucket.org", "kind": "cloud"}],
            "contexts": null,
        });
        assert_eq!(
            pick_context(&no_context),
            Err(BktError::NoContext("api.bitbucket.org".to_string()))
        );
        assert_eq!(pick_context(&json!({"hosts": null, "contexts": null})), Ok(None));
    }

    #[test]
    fn branch_listings_pair_an_open_page_with_a_finished_history_page() {
        let listings = branch_listings(&["feat/\"x\"".to_string()]);
        let q = |listing: &[Param<'_>]| listing[0].1.clone();
        assert_eq!(listings.len(), 2);
        assert_eq!(q(&listings[0]), r#"source.branch.name = "feat/\"x\"" AND state = "OPEN""#);
        assert!(q(&listings[1]).ends_with(r#"state IN ("MERGED", "DECLINED", "SUPERSEDED")"#));
        assert!(listings.iter().all(|listing| listing[0].0 == "q"));
    }

    #[test]
    fn a_pull_request_attaches_only_when_its_source_is_one_of_the_branchs_heads() {
        let (target, fork) = (bb(&["team", "app"]), bb(&["contributor", "app"]));
        let page = |nodes: Vec<Value>| vec![json!({"values": nodes})];
        let numbers = |assoc: &Association| {
            (
                assoc.open.iter().map(|pr| pr.number).collect::<Vec<_>>(),
                assoc.history.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            )
        };
        // A same-repository pull request attaches to a head published on the target.
        let own = page(vec![
            listed(1, "OPEN", "feat", "{t}", "{t}"),
            listed(2, "MERGED", "feat", "{t}", "{t}"),
            listed(3, "DECLINED", "other", "{t}", "{t}"),
        ]);
        let assoc = collect_assoc(&own, &target, &[head(&target, "feat")]);
        assert_eq!(numbers(&assoc), (vec![1], vec![2]));
        assert_eq!(assoc.history[0].closed_at, "2026-10-05T02:55:43.275809+00:00");
        assert_eq!(assoc.open[0].closed_at, "");

        // A fork's pull request attaches only to a head published on that fork.
        let forked = page(vec![listed(4, "OPEN", "feat", "{f}", "{t}")]);
        assert_eq!(numbers(&collect_assoc(&forked, &target, &[head(&fork, "feat")])).0, [4]);
        assert!(collect_assoc(&forked, &target, &[head(&target, "feat")]).open.is_empty());

        // A deleted fork names no repository, so its pull request attaches nowhere.
        let mut orphan = listed(5, "OPEN", "feat", "{f}", "{t}");
        orphan["source"]["repository"] = Value::Null;
        let orphaned = page(vec![orphan]);
        assert!(collect_assoc(&orphaned, &target, &[head(&fork, "feat")]).open.is_empty());
    }

    #[test]
    fn snapshot_maps_the_pull_request_fields() {
        let mut pr = listed(5455, "OPEN", "feat", "{t}", "{t}");
        pr["title"] = json!("Add Bitbucket");
        pr["description"] = json!("Body");
        pr["draft"] = json!(true);
        pr["destination"]["branch"] = json!({"name": "main"});
        pr["links"] =
            json!({"html": {"href": "https://bitbucket.org/team/app/pull-requests/5455"}});
        let snapshot =
            build_snapshot(&pr, "full".to_string(), Sync::InSync, &Value::Null, &Value::Null);
        assert_eq!((snapshot.number, snapshot.title.as_str()), (5455, "Add Bitbucket"));
        assert_eq!(snapshot.url, "https://bitbucket.org/team/app/pull-requests/5455");
        assert_eq!((snapshot.head_ref.as_str(), snapshot.base_ref.as_str()), ("feat", "main"));
        assert_eq!((snapshot.state, snapshot.is_draft), (PrState::Open, true));
        assert_eq!((snapshot.head_oid.as_str(), snapshot.head_is_fork), ("full", false));
        assert_eq!(snapshot.merge, Merge::Clean);
        assert!(!snapshot.comments_truncated && !snapshot.checks_truncated);
    }

    #[test]
    fn state_and_fork_mappings_follow_the_provider_contract() {
        assert_eq!(parse_state("OPEN"), PrState::Open);
        assert_eq!(parse_state("MERGED"), PrState::Merged);
        assert_eq!(parse_state("DECLINED"), PrState::Closed);
        assert_eq!(parse_state("SUPERSEDED"), PrState::Closed);
        assert_eq!(parse_state(""), PrState::Closed);
        assert!(is_cross_repository(&listed(1, "OPEN", "feat", "{f}", "{t}")));
        assert!(!is_cross_repository(&listed(1, "OPEN", "feat", "{t}", "{t}")));
    }

    #[test]
    fn a_page_is_capped_by_a_next_link_or_a_size_past_its_rows() {
        assert!(!page_capped(&json!({"values": [1, 2], "size": 2})));
        assert!(page_capped(&json!({"values": [1, 2], "size": 3})));
        assert!(page_capped(&json!({"values": [1], "next": "https://api.bitbucket.org/…"})));
        assert!(!page_capped(&Value::Null));
    }

    #[test]
    fn build_statuses_normalise_to_checks_and_a_rerun_replaces_its_earlier_report() {
        let statuses = json!({"values": [
            {"key": "k1", "name": "build", "state": "SUCCESSFUL", "updated_on": "2026-09-14T07:00:00+00:00"},
            {"key": "k0", "name": "build", "state": "FAILED", "updated_on": "2026-09-14T06:00:00+00:00"},
            {"key": "lint", "name": "", "state": "INPROGRESS", "updated_on": "2026-09-14T06:30:00+00:00"},
        ]});
        let checks = build_checks(&statuses);
        let rows: Vec<_> = checks.iter().map(|check| (check.name.as_str(), check.status)).collect();
        assert_eq!(rows, [("build", CheckStatus::Success), ("lint", CheckStatus::Running)]);
        assert_eq!(build_status("STOPPED"), CheckStatus::Failure);
        assert_eq!(build_status("anything else"), CheckStatus::Pending);
    }

    #[test]
    fn comments_group_into_threads_with_findings_sides_and_verdicts() {
        let mut finding = note(10, "2026-03-19T10:00:00+00:00", "rename this");
        finding["inline"] = json!({"path": "src/a.rs", "from": null, "to": 235});
        finding["resolution"] = json!({"type": "comment_resolution"});
        let mut reply = note(11, "2026-03-19T11:00:00+00:00", "done");
        reply["parent"] = json!({"id": 10});
        reply["inline"] = finding["inline"].clone();
        let mut nested = note(12, "2026-03-19T12:00:00+00:00", "thanks");
        nested["parent"] = json!({"id": 11});
        let mut old_side = note(20, "2026-03-20T10:00:00+00:00", "why removed?");
        old_side["inline"] =
            json!({"path": "src/b.rs", "from": 9, "to": null, "start_from": 7, "start_to": null});
        let mut deleted = note(30, "2026-03-21T10:00:00+00:00", "");
        deleted["deleted"] = json!(true);
        let mut pending = note(31, "2026-03-21T11:00:00+00:00", "draft");
        pending["pending"] = json!(true);
        let plain = note(40, "2026-03-22T10:00:00+00:00", "LGTM overall");
        // Newest first, as the comments page arrives.
        let page = json!({"values": [plain, pending, deleted, old_side, nested, reply, finding]});
        let pr = json!({"participants": [
            {"state": "approved", "participated_on": "2026-03-23T10:00:00+00:00",
             "user": {"display_name": "Derek", "type": "user"}},
            {"state": null, "user": {"display_name": "Lurker", "type": "user"}},
            {"state": "changes_requested", "participated_on": "2026-03-18T10:00:00+00:00",
             "user": {"display_name": "review-bot", "type": "app_user"}},
        ]});

        let out = merge_comments(&page, &pr);
        let anchors: Vec<_> = out.iter().map(|c| (c.kind, c.anchor.as_str())).collect();
        assert_eq!(
            anchors,
            [
                (CommentKind::Review, "review"),
                (CommentKind::Comment, "comment"),
                (CommentKind::Finding, "src/b.rs:7-9"),
                (CommentKind::Finding, "src/a.rs:235"),
                (CommentKind::Review, "review"),
            ]
        );
        assert_eq!(out[0].body, "Approved this pull request.");
        assert!(out[4].author_is_bot && out[4].body == "Requested changes.");
        assert_eq!(out[2].place.as_ref().unwrap().side, Some(crate::model::Side::Old));
        let thread = &out[3];
        assert_eq!(thread.place.as_ref().unwrap().side, Some(crate::model::Side::New));
        assert!(thread.is_resolved && thread.author == "Mario");
        let replies: Vec<_> = thread.replies.iter().map(|reply| reply.body.as_str()).collect();
        assert_eq!(replies, ["done", "thanks"], "a nested reply joins its thread, oldest first");
    }

    #[test]
    fn a_finding_quotes_its_hunk_without_the_file_header_pair() {
        let hunk = "@@ -4,2 +4,2 @@\n-old\n+new\n context\n";
        let with_pair = json!({"context_lines": format!("--- a/greet.py\n+++ b/greet.py\n{hunk}")});
        assert_eq!(quoted_hunk(&with_pair).as_deref(), Some(hunk));
        assert_eq!(quoted_hunk(&json!({"context_lines": hunk})).as_deref(), Some(hunk));
        assert_eq!(
            quoted_hunk(&json!({"context_lines": ""})),
            None,
            "a file comment quotes nothing"
        );
        assert_eq!(quoted_hunk(&json!({"path": "greet.py"})), None);
    }

    #[test]
    fn replies_whose_thread_head_is_off_the_page_or_deleted_still_form_one_thread() {
        let mut first = note(51, "2026-03-19T11:00:00+00:00", "older reply");
        first["parent"] = json!({"id": 50});
        first["inline"] = json!({"path": "src/a.rs", "to": 3});
        let mut second = note(52, "2026-03-19T12:00:00+00:00", "newer reply");
        second["parent"] = json!({"id": 50});
        let off_page = merge_comments(&json!({"values": [second, first]}), &Value::Null);
        assert_eq!(off_page.len(), 1);
        assert_eq!(
            (off_page[0].body.as_str(), off_page[0].anchor.as_str()),
            ("older reply", "src/a.rs:3")
        );
        assert_eq!(off_page[0].replies.len(), 1);

        let mut head = note(60, "2026-03-19T10:00:00+00:00", "");
        head["deleted"] = json!(true);
        head["inline"] = json!({"path": "src/c.rs", "to": 8});
        let mut survivor = note(61, "2026-03-19T11:00:00+00:00", "still here");
        survivor["parent"] = json!({"id": 60});
        let kept = merge_comments(&json!({"values": [survivor, head]}), &Value::Null);
        assert_eq!((kept.len(), kept[0].anchor.as_str()), (1, "src/c.rs:8"));
        assert!(kept[0].replies.is_empty(), "a deleted head is neither shown nor counted");
    }
}
