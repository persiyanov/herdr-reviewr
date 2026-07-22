//! Read-only GitHub access: the pull request's identity, state, checks, and comments.
//!
//! See `specs/forge-host.md`. A fetch first derives [`PrFetchInput`] from local Git and one
//! validated config snapshot, then reads its canonical target through explicitly hosted `gh`
//! GraphQL calls. It never posts, resolves, re-runs, merges, or otherwise writes to GitHub. The
//! `PR` tab renders the [`PrSnapshot`] this module produces; degradation is in-band as [`PrView`].

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// What the `PR` tab shows: the resolved snapshot, or a degraded state with its own remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrView {
    /// Work is pending but has not crossed the loading-indicator delay.
    Pending,
    /// Work crossed the loading-indicator delay without producing a snapshot.
    Loading,
    /// An open (or merged/closed) PR resolved through the worktree's publication points.
    Pr(Box<PrSnapshot>),
    /// No PR contains the worktree's published work.
    NoPr,
    /// `HEAD` is detached, so there is no branch identity to query.
    Detached,
    /// Two or more open PRs contain the published work and no tiebreak decides; the
    /// count, so the user knows to pick on GitHub.
    Ambiguous(usize),
    /// `gh` is not on `PATH`.
    NoGh,
    /// `gh` is installed but not authenticated for this canonical host.
    NotAuthed(String),
    /// Neither `upstream` nor `origin` names a supported hosted Git repository.
    NeedsGitHubRemote,
    /// The fallback `origin` names a hosted forge outside the supported GitHub hosts.
    UnsupportedHost(String),
    /// The fallback `origin` names a supported host but not an owner/repository path.
    MalformedOrigin(String),
    /// A local Git read failed before the GitHub fetch could start.
    GitError(String),
    /// Any other `gh` failure (rate limit, offline, …); the app freezes the last good view.
    Error(String),
}

impl PrView {
    /// A same-input failure that can be retried without discarding the visible snapshot.
    /// Both snapshot preservation and the empty-state renderer consume this projection so a
    /// newly added retryable failure cannot diverge between those surfaces. `refresh` is the
    /// active `refresh` binding's hint key, so the advertised retry key follows a rebind.
    pub fn retry_remedy(&self, refresh: crate::keymap::Key) -> Option<String> {
        match self {
            Self::NoGh => {
                Some(format!("GitHub CLI not found. Install `gh`, then press {refresh}."))
            }
            Self::NotAuthed(host) => Some(format!(
                "Not signed in to {host}. Run `gh auth login --hostname {host}`, then press {refresh}."
            )),
            Self::GitError(message) => {
                Some(format!("Git read failed: {message}. Press {refresh} to retry."))
            }
            Self::Error(message) => {
                Some(format!("GitHub unavailable: {message}. Press {refresh} to retry."))
            }
            _ => None,
        }
    }
}

/// One pull request's state, read fresh from GitHub each poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnapshot {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The PR description as GitHub returns it, empty when none (`specs/forge-host.md`).
    pub body: String,
    pub state: PrState,
    pub is_draft: bool,
    /// The PR's head branch name — the candidate that resolved, which may differ from the
    /// worktree's local branch name (`specs/forge-host.md`).
    pub head_ref: String,
    /// The head branch lives in another repository (GitHub's `isCrossRepository`); shown
    /// as a marker so a same-named fork PR is visible.
    pub head_is_fork: bool,
    pub base_ref: String,
    pub merge: Merge,
    pub sync: Sync,
    pub checks: Vec<Check>,
    pub comments: Vec<Comment>,
    /// A capped surface (reviews/comments/threads/checks) had more rows than the 100-row fetch
    /// returned — the lists shown are a prefix, not the whole set. Drives a "more on GitHub" marker.
    pub truncated: bool,
}

/// The PR lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// Whether the PR has a merge blocker worth surfacing, folded from GitHub's `mergeable` and
/// `mergeStateStatus`. Only the actionable blockers are modelled; GitHub's `behind` / `unstable`
/// / still-`checking` states carry nothing a reviewer acts on, so they fold into `Clean`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    Clean,
    Conflicting,
    Blocked,
}

/// The local branch's position relative to the PR head (`head_oid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    InSync,
    /// Local `HEAD` is ahead of the PR head by N commits — the PR lags your local tree.
    Unpushed(u32),
    /// The PR head is ahead of local `HEAD` by N commits.
    Behind(u32),
    /// The PR head object is not available locally, so its relation to `HEAD` is unknowable.
    Unknown,
}

/// One CI check, the latest run for its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
}

/// A check's outcome, normalised across check runs and commit statuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Success,
    Failure,
    Running,
    Pending,
    Skipped,
}

/// One incoming comment: a PR-level review, a plain comment, or an inline finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub kind: CommentKind,
    pub author: String,
    pub author_is_bot: bool,
    /// `path:line` for a finding, the literal `review`/`comment` for the unanchored kinds.
    pub anchor: String,
    pub body: String,
    /// The finding's diff hunk as GitHub returns it; `None` for a review or comment.
    pub snippet: Option<String>,
    /// The post time as GitHub's ISO-8601 string (`…Z`), the newest-first sort key.
    pub created_at: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub reply_count: u32,
}

/// What a comment is anchored to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    Review,
    Comment,
    Finding,
}

impl PrSnapshot {
    /// The overall check rollup: any failure fails, else any still-running is running, else success.
    /// `None` when the PR has no checks.
    #[must_use]
    pub fn checks_rollup(&self) -> Option<CheckStatus> {
        if self.checks.is_empty() {
            return None;
        }
        if self.checks.iter().any(|c| c.status == CheckStatus::Failure) {
            return Some(CheckStatus::Failure);
        }
        if self
            .checks
            .iter()
            .any(|c| matches!(c.status, CheckStatus::Running | CheckStatus::Pending))
        {
            return Some(CheckStatus::Running);
        }
        Some(CheckStatus::Success)
    }

    /// How many checks have failed — the count behind the `✗ N failing` rollup label.
    #[must_use]
    pub fn failing_checks(&self) -> usize {
        self.checks.iter().filter(|c| c.status == CheckStatus::Failure).count()
    }
}

/// Run explicitly targeted `gh` arguments in `repo` and return stdout or a classified failure.
fn gh(repo: &Path, host: &str, args: &[&str], cancelled: &AtomicBool) -> Result<String, GhError> {
    let child = Command::new("gh")
        .current_dir(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(GhError::NoGh);
        }
        Err(error) => return Err(GhError::Other(error.to_string())),
    };

    // Drain both pipes while polling so a large GraphQL response cannot fill a pipe and block
    // the child before it exits. A superseded config/fetch kills the process; the coordinator
    // keeps ownership until this worker reports completion, preserving one real fetch in flight.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let status = loop {
        if cancelled.load(Ordering::Acquire) {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(GhError::Other(error.to_string()));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if cancelled.load(Ordering::Acquire) {
        return Err(GhError::Other("request cancelled".to_string()));
    }
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    Err(classify_failure(&String::from_utf8_lossy(&stderr), host))
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

/// A classified `gh` failure, mapped to a [`PrView`] degraded state.
#[derive(Debug, PartialEq, Eq)]
enum GhError {
    NoGh,
    NotAuthed(String),
    LocalGit(String),
    Other(String),
}

impl From<GhError> for PrView {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NoGh => PrView::NoGh,
            GhError::NotAuthed(host) => PrView::NotAuthed(host),
            GhError::LocalGit(message) => PrView::GitError(message),
            GhError::Other(m) => PrView::Error(m),
        }
    }
}

/// The derived local state that determines one PR fetch.
pub use crate::git::PrFetchInput;

/// A local Git failure before a GitHub fetch starts.
#[derive(Debug, PartialEq, Eq)]
pub enum PrInputError {
    /// The repository target could not be proven, so no existing snapshot is attributable.
    TargetRead(String),
    /// Branch state failed after this repository target was proven.
    BranchState { target: crate::git::RepoTarget, message: String },
}

/// Derive one complete fetch input from local Git and one validated config snapshot.
pub fn fetch_input(
    repo: &Path,
    base: Option<&str>,
    config: &crate::config::PluginConfig,
) -> Result<PrFetchInput, PrInputError> {
    fetch_input_inner(repo, base, config, false)
}

/// Re-derive a completed fetch's input, confirming its repository again after the branch reads.
pub(crate) fn verify_input(
    repo: &Path,
    base: Option<&str>,
    config: &crate::config::PluginConfig,
) -> Result<PrFetchInput, PrInputError> {
    fetch_input_inner(repo, base, config, true)
}

fn fetch_input_inner(
    repo: &Path,
    base: Option<&str>,
    config: &crate::config::PluginConfig,
    verify_repository: bool,
) -> Result<PrFetchInput, PrInputError> {
    let (repository, origin_repository) = crate::git::remote_identities(repo, config.github_host())
        .map_err(|error| PrInputError::TargetRead(error.0))?;
    let crate::git::RepositoryIdentity::Repository(target) = &repository else {
        return Ok(PrFetchInput {
            repository,
            origin_repository: None,
            local: crate::git::PrLocalState::default(),
        });
    };
    let local = match crate::git::pr_local(repo, base, config.base_branches()) {
        Ok(local) => local,
        Err(error) => {
            let (current, _) = crate::git::remote_identities(repo, config.github_host())
                .map_err(|read_error| PrInputError::TargetRead(read_error.0))?;
            if current != repository {
                return Err(PrInputError::TargetRead(
                    "repository changed while reading branch state".to_string(),
                ));
            }
            return Err(PrInputError::BranchState { target: target.clone(), message: error.0 });
        }
    };
    let (repository, origin_repository) = if verify_repository {
        crate::git::remote_identities(repo, config.github_host())
            .map_err(|error| PrInputError::TargetRead(error.0))?
    } else {
        (repository, origin_repository)
    };
    Ok(PrFetchInput { repository, origin_repository, local })
}

/// Read GitHub for one already-derived input. Degradation stays in-band for the PR tab.
#[must_use]
pub fn fetch(repo: &Path, input: &PrFetchInput) -> PrView {
    fetch_cancellable(repo, input, &AtomicBool::new(false))
}

/// Read GitHub with a cancellation signal owned by the event-loop coordinator.
#[must_use]
pub(crate) fn fetch_cancellable(
    repo: &Path,
    input: &PrFetchInput,
    cancelled: &AtomicBool,
) -> PrView {
    match fetch_inner(repo, input, cancelled) {
        Ok(view) => view,
        Err(error) => error.into(),
    }
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    cancelled: &AtomicBool,
) -> Result<PrView, GhError> {
    let repository = match &input.repository {
        crate::git::RepositoryIdentity::Repository(target) => target,
        crate::git::RepositoryIdentity::Missing | crate::git::RepositoryIdentity::Hostless => {
            return Ok(PrView::NeedsGitHubRemote);
        }
        crate::git::RepositoryIdentity::Unsupported(host) => {
            return Ok(PrView::UnsupportedHost(host.clone()));
        }
        crate::git::RepositoryIdentity::Malformed(host) => {
            return Ok(PrView::MalformedOrigin(host.clone()));
        }
    };
    if input.local.detached {
        // A detached HEAD (e.g. after `gh pr merge --delete-branch`) has no pin.
        return Ok(PrView::Detached);
    }
    if input.local.points.is_empty()
        && input.local.absorbed.is_empty()
        && nominated_head(&input.local).is_none()
    {
        // No published work beyond the base, no parked published tip, and no
        // exact-identity HEAD — nothing can prove a PR, so nothing is fetched
        // (`specs/forge-host.md`).
        return Ok(PrView::NoPr);
    }
    let target = FetchTarget {
        repo,
        host: repository.host(),
        owner: repository.owner(),
        name: repository.name(),
        cancelled,
    };
    let source = select_source(input.origin_repository.as_ref(), repository);
    let head = nominated_head(&input.local);
    let assoc =
        associate_points(&target, source, &input.local.points, &input.local.absorbed, head)?;
    let number = match pick_open(&assoc.open, input) {
        Pick::One(n) => n,
        Pick::Ambiguous(count) => {
            return Ok(PrView::Ambiguous(count));
        }
        Pick::None => match pick_merged(&assoc.merged).or_else(|| pick_closed(&assoc.closed)) {
            Some(n) => n,
            None => {
                return Ok(PrView::NoPr);
            }
        },
    };
    let detail = pr_detail(&target, number)?;
    let node = &detail["data"]["repository"]["pullRequest"];
    if node.is_null() {
        return Ok(PrView::NoPr);
    }
    // Sync compares the fetch's pinned HEAD to the PR head, so a checkout or commit landing
    // mid-fetch never pairs one branch's PR with another branch's count.
    let pr_head = node["headRefOid"].as_str().unwrap_or_default();
    let sync = match input.local.head_oid.as_deref() {
        Some(pin) if !pr_head.is_empty() => derive_sync(
            crate::git::ahead_behind_oids(repo, pin, pr_head)
                .map_err(|error| GhError::LocalGit(error.0))?,
        ),
        _ => Sync::Unknown,
    };
    Ok(PrView::Pr(Box::new(build_snapshot(node, sync))))
}

/// The local branch's position relative to the PR head, from `git`'s ahead/behind counts. A
/// diverged branch (both nonzero) leads with the unpushed count — the headline case. `None`
/// (the PR head isn't local yet) stays explicitly unknown rather than guessing.
fn derive_sync(ahead_behind: Option<(u32, u32)>) -> Sync {
    match ahead_behind {
        None => Sync::Unknown,
        Some((0, 0)) => Sync::InSync,
        Some((0, behind)) => Sync::Behind(behind),
        Some((ahead, _)) => Sync::Unpushed(ahead),
    }
}

struct FetchTarget<'a> {
    repo: &'a Path,
    host: &'a str,
    owner: &'a str,
    name: &'a str,
    cancelled: &'a AtomicBool,
}

/// One PR from the association query, reduced to the pick-relevant fields.
#[derive(Debug)]
struct AssocPr {
    number: u64,
    head_oid: String,
    head_ref: String,
    merged_at: String,
    created_at: String,
}

/// The association result, split by lifecycle: open and merged from the commit
/// association, closed-unmerged from the exact-identity name lookup.
#[derive(Debug, Default)]
struct Association {
    open: Vec<AssocPr>,
    merged: Vec<AssocPr>,
    closed: Vec<AssocPr>,
}

/// The repository the association query runs against: the origin repository, where the
/// published commits live — the fork case resolves through it (`specs/forge-host.md`). An
/// origin on another host cannot prove anything on the target's forge, so the target
/// stands in.
fn select_source<'a>(
    origin: Option<&'a crate::git::RepoTarget>,
    target: &'a crate::git::RepoTarget,
) -> &'a crate::git::RepoTarget {
    origin.filter(|origin| origin.host() == target.host()).unwrap_or(target)
}

/// The pinned `HEAD` as one more exact-identity nomination, unless a point already
/// carries the same OID. A nominating `HEAD` sits outside base history, so it can never
/// be an absorbed candidate (`specs/forge-host.md`).
fn nominated_head(local: &crate::git::PrLocalState) -> Option<&str> {
    local
        .head_oid
        .as_deref()
        .filter(|head| local.head_nominates && !local.points.iter().any(|point| point.oid == *head))
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

/// Ask the forge which PRs contain each publication point, in one aliased call against the
/// `source` repository, with the closed-unmerged name lookup against the target riding
/// along (`specs/forge-host.md`). Only PRs based on the target repository count.
fn associate_points(
    target: &FetchTarget<'_>,
    source: &crate::git::RepoTarget,
    points: &[crate::git::PublicationPoint],
    absorbed: &[String],
    head: Option<&str>,
) -> Result<Association, GhError> {
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

/// Push `pr` unless its number is already in `bucket` — a PR's identity is its number.
fn push_unique(bucket: &mut Vec<AssocPr>, pr: AssocPr) {
    if !bucket.iter().any(|have| have.number == pr.number) {
        bucket.push(pr);
    }
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
                    push_unique(&mut assoc.merged, pr);
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
                Some("OPEN") => push_unique(&mut assoc.open, pr),
                Some("MERGED") => push_unique(&mut assoc.merged, pr),
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
            push_unique(&mut assoc.closed, pr);
        }
    }
    assoc
}

/// The winner among the open PRs (`specs/forge-host.md` "Resolution").
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    One(u64),
    Ambiguous(usize),
    None,
}

/// Pick the open PR: a lone PR wins; several disambiguate by a head equal to the pinned
/// `HEAD`, then a head equal to a publication point, then the head named by the recorded
/// upstream — each only when exactly one matches. Failing all three, the count surfaces.
fn pick_open(open: &[AssocPr], input: &PrFetchInput) -> Pick {
    match open {
        [] => Pick::None,
        [only] => Pick::One(only.number),
        many => {
            let unique = |test: &dyn Fn(&AssocPr) -> bool| -> Option<u64> {
                let mut hits = many.iter().filter(|pr| test(pr));
                match (hits.next(), hits.next()) {
                    (Some(pr), None) => Some(pr.number),
                    _ => None,
                }
            };
            if let Some(pin) = input.local.head_oid.as_deref()
                && let Some(number) = unique(&|pr| pr.head_oid == pin)
            {
                return Pick::One(number);
            }
            if let Some(number) =
                unique(&|pr| input.local.points.iter().any(|point| point.oid == pr.head_oid))
            {
                return Pick::One(number);
            }
            if let Some(upstream) = input.local.upstream.as_deref()
                && let Some(number) = unique(&|pr| pr.head_ref == upstream)
            {
                return Pick::One(number);
            }
            Pick::Ambiguous(many.len())
        }
    }
}

/// The PR with the newest `key` timestamp. ISO-8601 `…Z` strings compare lexically; a
/// strict `>` keeps the earlier entry on a tie, so the pick is deterministic.
fn newest_by(prs: &[AssocPr], key: impl Fn(&AssocPr) -> &str) -> Option<u64> {
    let mut best: Option<&AssocPr> = None;
    for pr in prs {
        if best.is_none_or(|b| key(pr) > key(b)) {
            best = Some(pr);
        }
    }
    best.map(|pr| pr.number)
}

/// The newest-merged PR containing a publication point.
fn pick_merged(merged: &[AssocPr]) -> Option<u64> {
    newest_by(merged, |pr| &pr.merged_at)
}

/// The newest closed-unmerged PR whose head is exactly a publication point.
fn pick_closed(closed: &[AssocPr]) -> Option<u64> {
    newest_by(closed, |pr| &pr.created_at)
}

/// All of one PR's state in a single direct GraphQL call — identity, mergeability, checks,
/// reviews, plain comments, and review threads. Each list surface reads its newest 100 rows
/// (`last:100`, flagged by `hasPreviousPage`) — ample for any real PR in a review sidebar —
/// and flags a fuller surface so the UI can mark it, rather than paging to exhaustion
/// (`specs/forge-host.md`). Checks keep `first:100`/`hasNextPage`.
fn pr_detail(target: &FetchTarget<'_>, number: u64) -> Result<Value, GhError> {
    let q = build_detail_query(number);
    let vars = vec![
        ("o".to_string(), target.owner.to_string()),
        ("n".to_string(), target.name.to_string()),
    ];
    graphql(target.repo, target.host, &q, &vars, target.cancelled)
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

/// Assemble the snapshot from the `gh pr view` JSON, the computed `sync`, and the merged comments.
fn build_snapshot(node: &Value, sync: Sync) -> PrSnapshot {
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

    dedup_bot_prose(&mut out);
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

/// Keep only the latest PR-level (`review`/`comment`) post per bot author; humans keep all.
fn dedup_bot_prose(out: &mut Vec<Comment>) {
    let mut keep_newest: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for c in out.iter() {
        if c.author_is_bot && c.kind != CommentKind::Finding {
            let e = keep_newest.entry(c.author.clone()).or_default();
            if c.created_at > *e {
                e.clone_from(&c.created_at);
            }
        }
    }
    out.retain(|c| {
        !(c.author_is_bot && c.kind != CommentKind::Finding)
            || keep_newest.get(&c.author) == Some(&c.created_at)
    });
}

/// Whether a GitHub login is an app/bot (`…[bot]`).
fn is_bot(login: &str) -> bool {
    login.ends_with("[bot]")
}

/// A relative age label (`5m`, `2h`, `3d`, `2w`) from an ISO-8601 `…Z` timestamp, against `now`.
/// `now` is injected so the formatting is testable; the UI passes `SystemTime::now()`.
#[must_use]
pub fn relative_age(created_at: &str, now: SystemTime) -> String {
    let Some(then) = parse_iso(created_at) else {
        return String::new();
    };
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let secs = (now - then).max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 604_800 => format!("{}d", s / 86_400),
        s => format!("{}w", s / 604_800),
    }
}

/// Parse a fixed `YYYY-MM-DDTHH:MM:SSZ` timestamp to a Unix epoch second. `None` on any
/// deviation, so a malformed value yields an empty age rather than a wrong one.
// The civil-from-days algorithm reads naturally with the conventional short field names.
#[allow(clippy::many_single_char_names)]
fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, se) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // Days from the civil date (Howard Hinnant's algorithm), then to seconds.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let year_of_era = y - era * 400;
    let day_of_year = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rollup_fails_on_any_failure_else_running_else_success() {
        let snap = |statuses: &[CheckStatus]| PrSnapshot {
            number: 1,
            title: String::new(),
            url: String::new(),
            body: String::new(),
            state: PrState::Open,
            is_draft: false,
            head_ref: String::new(),
            head_is_fork: false,
            base_ref: String::new(),
            merge: Merge::Clean,
            sync: Sync::InSync,
            checks: statuses.iter().map(|&s| Check { name: "c".into(), status: s }).collect(),
            comments: Vec::new(),
            truncated: false,
        };
        assert_eq!(snap(&[]).checks_rollup(), None);
        assert_eq!(
            snap(&[CheckStatus::Success, CheckStatus::Success]).checks_rollup(),
            Some(CheckStatus::Success)
        );
        assert_eq!(
            snap(&[CheckStatus::Success, CheckStatus::Running]).checks_rollup(),
            Some(CheckStatus::Running)
        );
        assert_eq!(
            snap(&[CheckStatus::Running, CheckStatus::Failure]).checks_rollup(),
            Some(CheckStatus::Failure)
        );
    }

    fn point(oid: &str, names: &[&str]) -> crate::git::PublicationPoint {
        crate::git::PublicationPoint {
            oid: oid.to_string(),
            names: names.iter().map(|n| (*n).to_string()).collect(),
        }
    }

    fn input(
        head: &str,
        points: Vec<crate::git::PublicationPoint>,
        up: Option<&str>,
    ) -> PrFetchInput {
        PrFetchInput {
            repository: crate::git::RepositoryIdentity::Missing,
            origin_repository: None,
            local: crate::git::PrLocalState {
                head_oid: Some(head.to_string()),
                base_oid: Some("base".to_string()),
                points,
                absorbed: Vec::new(),
                head_nominates: false,
                upstream: up.map(str::to_string),
                detached: false,
            },
        }
    }

    fn assoc(number: u64, head_oid: &str, head_ref: &str) -> AssocPr {
        AssocPr {
            number,
            head_oid: head_oid.to_string(),
            head_ref: head_ref.to_string(),
            merged_at: String::new(),
            created_at: String::new(),
        }
    }

    #[test]
    fn fetch_gates_resolve_without_touching_the_forge() {
        // Each early gate returns before any `gh` spawn: identity failures, a detached
        // HEAD, and a worktree with no publication points (`specs/forge-host.md`).
        let gated = |input: &PrFetchInput| fetch(Path::new("."), input);
        let mut missing = input("head", vec![], None);
        missing.repository = crate::git::RepositoryIdentity::Missing;
        assert_eq!(gated(&missing), PrView::NeedsGitHubRemote);

        let mut unsupported = input("head", vec![], None);
        unsupported.repository = crate::git::RepositoryIdentity::Unsupported("gitlab.com".into());
        assert_eq!(gated(&unsupported), PrView::UnsupportedHost("gitlab.com".into()));

        let repo = crate::git::RepositoryIdentity::Repository(
            crate::git::RepoTarget::new("github.com", "owner", "repo").unwrap(),
        );
        let mut detached = input("head", vec![], None);
        detached.repository = repo.clone();
        detached.local.detached = true;
        assert_eq!(gated(&detached), PrView::Detached);

        let mut zero_work = input("head", vec![], None);
        zero_work.repository = repo;
        assert_eq!(gated(&zero_work), PrView::NoPr);
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
    fn the_head_nominates_only_when_flagged_and_not_already_a_point() {
        // The gate and the alias both route through this one filter, so a regression
        // that drops the nomination re-tightens the NoPr gate and fails here.
        let mut local = input("tip", vec![], None).local;
        assert_eq!(nominated_head(&local), None, "an unflagged HEAD never nominates");
        local.head_nominates = true;
        assert_eq!(nominated_head(&local), Some("tip"), "a flagged HEAD nominates alone");
        local.points = vec![point("tip", &["feat"])];
        assert_eq!(nominated_head(&local), None, "a point already carries the OID");
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
    fn pick_open_prefers_head_then_point_then_upstream_else_surfaces_the_count() {
        let one = [assoc(1, "aaa", "feat")];
        assert_eq!(pick_open(&one, &input("zzz", vec![], None)), Pick::One(1));
        assert_eq!(pick_open(&[], &input("zzz", vec![], None)), Pick::None);

        let two = [assoc(1, "aaa", "feat"), assoc(2, "bbb", "cont")];
        assert_eq!(pick_open(&two, &input("bbb", vec![], None)), Pick::One(2));
        assert_eq!(pick_open(&two, &input("zzz", vec![point("aaa", &[])], None)), Pick::One(1));
        assert_eq!(pick_open(&two, &input("zzz", vec![], Some("cont"))), Pick::One(2));
        assert_eq!(pick_open(&two, &input("zzz", vec![], None)), Pick::Ambiguous(2));
        // A tiebreak matching several PRs decides nothing.
        let dup = [assoc(1, "aaa", "feat"), assoc(2, "aaa", "feat")];
        assert_eq!(pick_open(&dup, &input("aaa", vec![], Some("feat"))), Pick::Ambiguous(2));
        // Tiers outrank, not merely win in isolation: with the pinned HEAD on one PR and
        // both lower tiers pointing at the other, the HEAD tier decides. Same one rung
        // down — a point identity beats an upstream name, per "names never prove identity".
        let crossed = [assoc(1, "aaa", "feat"), assoc(2, "bbb", "cont")];
        assert_eq!(
            pick_open(&crossed, &input("aaa", vec![point("bbb", &[])], Some("cont"))),
            Pick::One(1)
        );
        assert_eq!(
            pick_open(&crossed, &input("zzz", vec![point("aaa", &[])], Some("cont"))),
            Pick::One(1)
        );
    }

    #[test]
    fn source_selection_prefers_a_same_host_origin_else_the_target() {
        let target = crate::git::RepoTarget::new("github.com", "acme", "widgets").unwrap();
        let fork = crate::git::RepoTarget::new("github.com", "contributor", "widgets").unwrap();
        let foreign = crate::git::RepoTarget::new("ghe.corp.test", "me", "widgets").unwrap();
        assert_eq!(select_source(Some(&fork), &target), &fork);
        assert_eq!(select_source(Some(&foreign), &target), &target);
        assert_eq!(select_source(None, &target), &target);
    }

    #[test]
    fn merged_pick_takes_the_newest_merge_and_closed_pick_the_newest_created() {
        let merged = [
            AssocPr { merged_at: "2026-06-01T00:00:00Z".into(), ..assoc(1, "a", "x") },
            AssocPr { merged_at: "2026-06-03T00:00:00Z".into(), ..assoc(2, "b", "y") },
            AssocPr { merged_at: "2026-06-03T00:00:00Z".into(), ..assoc(3, "c", "z") }, // tie → earlier
        ];
        assert_eq!(pick_merged(&merged), Some(2));
        assert_eq!(pick_merged(&[]), None);
        let closed = [
            AssocPr { created_at: "2026-05-01T00:00:00Z".into(), ..assoc(4, "d", "x") },
            AssocPr { created_at: "2026-05-09T00:00:00Z".into(), ..assoc(5, "e", "y") },
        ];
        assert_eq!(pick_closed(&closed), Some(5));
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
    fn relative_age_buckets_by_magnitude() {
        // now = 2026-06-27T12:00:00Z
        let now = UNIX_EPOCH
            + std::time::Duration::from_secs(parse_iso("2026-06-27T12:00:00Z").unwrap() as u64);
        assert_eq!(relative_age("2026-06-27T11:55:00Z", now), "5m");
        assert_eq!(relative_age("2026-06-27T10:00:00Z", now), "2h");
        assert_eq!(relative_age("2026-06-24T12:00:00Z", now), "3d");
        assert_eq!(relative_age("2026-06-13T12:00:00Z", now), "2w");
        assert_eq!(relative_age("garbage", now), "");
    }

    #[test]
    fn parse_iso_anchors_the_epoch_and_the_feb_year_branch() {
        // The epoch anchors the civil-from-days math; a Jan/Feb date exercises the `mo <= 2`
        // year-adjust branch that the June fixtures above never hit.
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2000-02-29T00:00:00Z"), Some(951_782_400)); // a leap-day boundary
        assert_eq!(parse_iso("not-a-date"), None);
    }

    #[test]
    fn sync_leads_with_unpushed_and_tolerates_a_missing_head() {
        assert_eq!(derive_sync(None), Sync::Unknown);
        assert_eq!(derive_sync(Some((0, 0))), Sync::InSync);
        assert_eq!(derive_sync(Some((2, 0))), Sync::Unpushed(2));
        assert_eq!(derive_sync(Some((0, 3))), Sync::Behind(3));
        assert_eq!(derive_sync(Some((2, 3))), Sync::Unpushed(2)); // diverged → unpushed leads
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
        assert_eq!(
            PrView::from(GhError::LocalGit("rev-list failed".into())),
            PrView::GitError("rev-list failed".into())
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
