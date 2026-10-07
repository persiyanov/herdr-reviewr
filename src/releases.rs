//! The `Releases` tab: `origin`'s versions and releases read from GitHub, never local refs.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::app::RefreshKind;
use crate::git::{Forge, ForgeHosts, RepoTarget, RepositoryIdentity};

/// What the `Releases` tab shows: the resolved list, or a degraded state with its own remedy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReleasesView {
    /// Work is pending but has not crossed the loading-indicator delay.
    #[default]
    Pending,
    /// Work crossed the loading-indicator delay without producing a list.
    Loading,
    Ready(Box<ReleasesSnapshot>),
    /// The resolved repository lives on a forge whose releases this tab does not read.
    NotGitHub(Forge),
    /// `origin` names no recognized forge repository.
    NeedsForgeRemote,
    /// The fallback `origin` names a hosted forge outside the supported forge hosts.
    UnsupportedHost(String),
    /// The fallback `origin` names a supported host but not a valid repository path.
    MalformedOrigin(String),
    /// GitHub reports no default branch: the repository has no commits yet.
    NoDefaultBranch,
    /// `gh` is not on `PATH`.
    NoCli,
    /// `gh` is installed but not authenticated for this host.
    NotAuthed(String),
    /// A local Git read failed before GitHub could be asked.
    GitError(String),
    /// Any other `gh` failure (rate limit, offline, …); a painted list stays.
    Error(String),
}

impl ReleasesView {
    /// The remedy for a retryable failure, which keeps a painted list; `None` otherwise.
    #[must_use]
    pub fn retry_remedy(&self, refresh: crate::keymap::Key) -> Option<String> {
        let refresh = refresh.label();
        match self {
            Self::NoCli => {
                Some(format!("GitHub CLI not found. Install `gh`, then press {refresh}."))
            }
            Self::NotAuthed(host) => Some(format!(
                "Not signed in to {host}. Run {}, then press {refresh}.",
                crate::forge::login_hint(Forge::GitHub, host)
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

    /// The read pane's message when there is no list to show; `None` for a list or a quiet wait.
    #[must_use]
    pub fn message(&self, refresh: crate::keymap::Key) -> Option<String> {
        if let Some(remedy) = self.retry_remedy(refresh) {
            return Some(remedy);
        }
        Some(match self {
            Self::Pending | Self::Ready(_) => return None,
            Self::Loading => "loading…".into(),
            Self::NotGitHub(forge) => format!(
                "The Releases tab reads GitHub only. This repository is on {}.",
                forge.display_name()
            ),
            Self::NeedsForgeRemote => "The Releases tab needs a GitHub remote named origin.".into(),
            Self::UnsupportedHost(host) => {
                format!("Unsupported host: {host}. Self-hosted GitHub? Set `github_host`.")
            }
            Self::MalformedOrigin(host) => {
                format!("The origin remote must point to a repository path on {host}.")
            }
            Self::NoDefaultBranch => "The repository has no default branch yet.".into(),
            Self::NoCli | Self::NotAuthed(_) | Self::GitError(_) | Self::Error(_) => {
                unreachable!("retry failures returned above")
            }
        })
    }
}

/// `origin`'s version tags, unreleased commits, and releases, read in one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasesSnapshot {
    /// The repository the list was read from, part of its identity.
    pub repository: RepoTarget,
    /// The default branch's name, the other part of its identity.
    pub branch: String,
    /// The commits newer than the newest version tag, newest first.
    pub root: Vec<ReleaseCommit>,
    /// Every version tag, highest version first.
    pub versions: Vec<VersionTag>,
    /// Every tag by the commit it points at, annotated tags peeled.
    pub tags: HashMap<String, Vec<String>>,
    /// Every published or draft release the list read, newest first.
    pub releases: Vec<Release>,
    /// The `upstream` remote's GitHub repository, when it names another one.
    pub upstream: Option<Upstream>,
}

/// One version tag: a node in the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionTag {
    pub tag: String,
    /// The commit it points at.
    pub oid: String,
    /// That commit, which the node row stands for.
    pub commit: ReleaseCommit,
}

/// A node's commit range, by both ends: a moved or deleted tag is another key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeKey {
    pub tag: String,
    pub oid: String,
    /// The next older version tag's commit, where the range stops; none for the oldest.
    pub older: Option<String>,
}

/// A node's commits, loaded only once it is unfolded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeLoad {
    Loading,
    /// Newest first; `more` when the range ran past what one load reads.
    Loaded {
        commits: Vec<ReleaseCommit>,
        more: bool,
    },
    Failed(String),
}

/// The `upstream` remote's repository, read only to link it and flag a newer release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    pub repository: RepoTarget,
    /// Its latest release's tag, when that is a newer version than `origin`'s highest.
    pub newer: Option<String>,
}

/// One row of the release tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeRow {
    /// A draft GitHub release, which has no tag until it is published.
    Draft { release: usize },
    /// The node of `versions[node]`.
    Release { node: usize, open: bool },
    /// Commit `index` of an unfolded node's loaded commits, or of the root.
    Commit { node: Option<usize>, index: usize },
    /// An unfolded node's load state: loading, failed, or more commits than shown.
    Note { node: usize },
}

/// What a row stands for: the identity a refresh reconciles the selection by, never its index.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pick {
    /// A draft by its GitHub id; its tag finds its node once it is published.
    Draft {
        id: u64,
        tag: String,
    },
    Release(String),
    Commit {
        oid: String,
        node: Option<String>,
    },
    Note(String),
}

/// One commit on the default branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleaseCommit {
    pub oid: String,
    pub subject: String,
    /// The full message, subject and body.
    pub message: String,
    pub author: String,
    /// When it was committed, as GitHub spells it.
    pub date: String,
    /// The tags pointing at this commit, annotated tags peeled.
    pub tags: Vec<String>,
}

/// One GitHub release.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Release {
    /// GitHub's id: a draft's identity, since its tag may be shared or empty.
    pub id: u64,
    /// Empty only for a draft saved without one.
    pub tag: String,
    pub name: String,
    /// The release notes as markdown, empty when none.
    pub notes: String,
    pub draft: bool,
    pub prerelease: bool,
}

/// What the read pane shows for a version's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notes<'a> {
    /// The version has a GitHub release.
    Release(&'a Release),
    /// The tag has no GitHub release; its tagged commit's message shows instead.
    TagOnly(&'a VersionTag),
}

impl ReleasesSnapshot {
    /// The GitHub release of `tag`.
    #[must_use]
    pub fn release(&self, tag: &str) -> Option<&Release> {
        self.releases.iter().find(|release| release.tag == tag)
    }

    /// A node's notes: its release, else its tag with none.
    #[must_use]
    pub fn node_notes(&self, node: usize) -> Notes<'_> {
        let version = &self.versions[node];
        self.release(&version.tag).map_or(Notes::TagOnly(version), Notes::Release)
    }

    /// The range node `node` covers.
    #[must_use]
    pub fn node_key(&self, node: usize) -> NodeKey {
        let version = &self.versions[node];
        NodeKey {
            tag: version.tag.clone(),
            oid: version.oid.clone(),
            older: self.versions.get(node + 1).map(|older| older.oid.clone()),
        }
    }

    /// The notice a newer `upstream` release earns, if any.
    #[must_use]
    pub fn upstream_notice(&self) -> Option<String> {
        let tag = self.upstream.as_ref()?.newer.as_ref()?;
        Some(format!("{tag} is newer"))
    }

    /// Every draft release, newest first, by index into `releases`.
    #[must_use]
    pub fn drafts(&self) -> Vec<usize> {
        (0..self.releases.len()).filter(|&i| self.releases[i].draft).collect()
    }

    /// Every tag a release names, drafts included: a tag a new release cannot take.
    pub fn release_tags(&self) -> impl Iterator<Item = &str> {
        self.releases.iter().map(|release| release.tag.as_str()).filter(|tag| !tag.is_empty())
    }

    /// Whether commits wait above the newest version tag: what the next release would ship.
    #[must_use]
    pub fn has_unreleased(&self) -> bool {
        !self.root.is_empty()
    }
}

/// A repository's web page; GitHub and GitLab both serve it at the host and path.
#[must_use]
pub fn web_url(repository: &RepoTarget) -> String {
    format!("https://{}/{}", repository.host(), repository.full_path())
}

/// History, tags with their peeled commit, and releases in one call, each its newest 100.
const QUERY: &str = "query($o:String!,$n:String!){repository(owner:$o,name:$n){\
     defaultBranchRef{name target{... on Commit{history(first:100){\
     nodes{oid message committedDate author{name}}}}}} \
     refs(refPrefix:\"refs/tags/\",first:100,orderBy:{field:TAG_COMMIT_DATE,direction:DESC}){\
     nodes{name target{oid ... on Commit{message committedDate author{name}} \
     ... on Tag{target{oid ... on Commit{message committedDate author{name}}}}}}} \
     releases(first:100,orderBy:{field:CREATED_AT,direction:DESC}){\
     nodes{databaseId tagName name description isDraft isPrerelease}} latestRelease{tagName}}}";

/// One page of a node's history, from its tagged commit back.
const NODE_QUERY: &str = "query($o:String!,$n:String!,$oid:GitObjectID!,$after:String){\
     repository(owner:$o,name:$n){object(oid:$oid){... on Commit{history(first:100,after:$after){\
     pageInfo{hasNextPage endCursor} nodes{oid message committedDate author{name}}}}}}}";

/// The most commits one node load reads, in pages of `PAGE`.
const NODE_CAP: usize = 500;
const PAGE: usize = 100;

/// The latest release of the `upstream` repository, for the newer-version notice.
const UPSTREAM_QUERY: &str =
    "query($o:String!,$n:String!){repository(owner:$o,name:$n){latestRelease{tagName}}}";

/// Read `origin`'s releases and `upstream`'s latest beside them; runs on a worker, never the loop.
#[must_use]
pub fn fetch(repo: &Path, hosts: &ForgeHosts<'_>, cancelled: &AtomicBool) -> ReleasesView {
    let origin = match crate::git::remote_identity(repo, "origin", hosts) {
        Ok(origin) => origin,
        Err(error) => return ReleasesView::GitError(error.0),
    };
    let target = match origin {
        RepositoryIdentity::Repository(target) => target,
        RepositoryIdentity::Missing | RepositoryIdentity::Hostless => {
            return ReleasesView::NeedsForgeRemote;
        }
        RepositoryIdentity::Unsupported(host) => return ReleasesView::UnsupportedHost(host),
        RepositoryIdentity::Malformed(host) => return ReleasesView::MalformedOrigin(host),
    };
    if target.forge() != Forge::GitHub {
        return ReleasesView::NotGitHub(target.forge());
    }
    // Both reads run at once, so the notice never delays the list.
    let (response, upstream) = std::thread::scope(|scope| {
        let upstream = scope.spawn(|| upstream_latest(repo, hosts, &target, cancelled));
        let response = crate::forge::graphql(repo, target.host(), QUERY, &vars(&target), cancelled);
        (response, upstream.join().ok().flatten())
    });
    match response {
        Ok(response) => {
            let mut view = parse(&response, target);
            if let ReleasesView::Ready(snapshot) = &mut view {
                let origin_highest = origin_highest(&response);
                snapshot.upstream = upstream.map(|(repository, latest)| {
                    Upstream::flagged(repository, latest, origin_highest)
                });
            }
            view
        }
        Err(error) => error.into(),
    }
}

fn vars(target: &RepoTarget) -> [(String, String); 2] {
    [("o".to_string(), target.owner().to_string()), ("n".to_string(), target.name().to_string())]
}

/// `upstream`'s GitHub repository and latest tag; a failed read is no tag, never an error.
fn upstream_latest(
    repo: &Path,
    hosts: &ForgeHosts<'_>,
    origin: &RepoTarget,
    cancelled: &AtomicBool,
) -> Option<(RepoTarget, Option<String>)> {
    let Ok(RepositoryIdentity::Repository(upstream)) =
        crate::git::remote_identity(repo, "upstream", hosts)
    else {
        return None;
    };
    if upstream.forge() != Forge::GitHub || upstream.is(origin) {
        return None;
    }
    let latest =
        crate::forge::graphql(repo, upstream.host(), UPSTREAM_QUERY, &vars(&upstream), cancelled)
            .ok()
            .and_then(|response| {
                response["data"]["repository"]["latestRelease"]["tagName"]
                    .as_str()
                    .map(String::from)
            });
    Some((upstream, latest))
}

impl Upstream {
    /// Upstream with its latest release kept only when it is newer than `origin`'s highest.
    fn flagged(repository: RepoTarget, latest: Option<String>, origin: Option<&str>) -> Self {
        Self { repository, newer: latest.filter(|tag| is_newer(tag, origin)) }
    }
}

/// A tag's semver version, with or without a leading `v`; `None` makes it a plain label.
pub(crate) fn version(tag: &str) -> Option<semver::Version> {
    let tag = tag.trim();
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// A commit as a GraphQL `Commit` node reads: its oid, full message, author, and date.
fn graphql_commit(node: &Value) -> Option<ReleaseCommit> {
    let message = node["message"].as_str().unwrap_or_default().trim_end();
    Some(ReleaseCommit {
        oid: node["oid"].as_str()?.to_string(),
        subject: message.lines().next().unwrap_or_default().to_string(),
        message: message.to_string(),
        author: node["author"]["name"].as_str().unwrap_or_default().to_string(),
        date: node["committedDate"].as_str().unwrap_or_default().to_string(),
        tags: Vec::new(),
    })
}

/// The highest version among `tags`.
fn highest_version<'a>(tags: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    tags.filter_map(|tag| Some((version(tag)?, tag))).max_by(|a, b| a.0.cmp(&b.0)).map(|(_, t)| t)
}

/// `origin`'s highest version across its tags and its latest release.
fn origin_highest(response: &Value) -> Option<&str> {
    let node = &response["data"]["repository"];
    let tags = nodes(&node["refs"]).filter_map(|tag| tag["name"].as_str());
    highest_version(tags.chain(node["latestRelease"]["tagName"].as_str()))
}

/// Whether `upstream` outranks `origin` (absent: any does); an unparsable version never does.
fn is_newer(upstream: &str, origin: Option<&str>) -> bool {
    match (version(upstream), origin.map(version)) {
        (Some(upstream), Some(Some(origin))) => upstream > origin,
        (Some(_), None) => true,
        _ => false,
    }
}

impl From<crate::forge::GhError> for ReleasesView {
    fn from(error: crate::forge::GhError) -> Self {
        use crate::forge::GhError;
        match error {
            GhError::NoGh => Self::NoCli,
            GhError::NotAuthed(host) => Self::NotAuthed(host),
            GhError::LocalGit(message) => Self::GitError(message),
            GhError::NotFound(message) | GhError::Other(message) => Self::Error(message),
        }
    }
}

/// Build the view from the GraphQL response; a missing repository is an error, never an empty list.
fn parse(response: &Value, repository: RepoTarget) -> ReleasesView {
    let node = &response["data"]["repository"];
    if node.is_null() {
        return ReleasesView::Error(format!("{} not found", repository.full_path()));
    }
    let branch = &node["defaultBranchRef"];
    let Some(name) = branch["name"].as_str() else {
        return ReleasesView::NoDefaultBranch;
    };
    let mut tags: HashMap<String, Vec<String>> = HashMap::new();
    let mut versions = Vec::new();
    for tag in nodes(&node["refs"]) {
        // An annotated tag points at its tag object; the commit is one step further.
        let target = match &tag["target"]["target"] {
            Value::Object(_) => &tag["target"]["target"],
            _ => &tag["target"],
        };
        let oid = target["oid"].as_str();
        if let (Some(name), Some(oid)) = (tag["name"].as_str(), oid) {
            tags.entry(oid.to_string()).or_default().push(name.to_string());
            if version(name).is_some() {
                let commit = graphql_commit(target).unwrap_or_default();
                versions.push(VersionTag { tag: name.to_string(), oid: oid.to_string(), commit });
            }
        }
    }
    versions.sort_by_key(|v| std::cmp::Reverse(version(&v.tag)));
    let root = nodes(&branch["target"]["history"])
        .map_while(|commit| {
            let oid = commit["oid"].as_str()?;
            // The newest version tag's commit ends the unreleased run.
            if versions.iter().any(|version| version.oid == oid) {
                return None;
            }
            let tags = tags.get(oid).cloned().unwrap_or_default();
            graphql_commit(commit).map(|commit| ReleaseCommit { tags, ..commit })
        })
        .collect();
    let releases = nodes(&node["releases"])
        .filter_map(|release| {
            let draft = release["isDraft"].as_bool().unwrap_or(false);
            // A draft may hold no tag yet; a published release always has one.
            let tag = release["tagName"].as_str().filter(|tag| draft || !tag.is_empty())?;
            Some(Release {
                id: release["databaseId"].as_u64().unwrap_or_default(),
                tag: tag.to_string(),
                name: release["name"].as_str().unwrap_or_default().to_string(),
                notes: release["description"].as_str().unwrap_or_default().to_string(),
                draft,
                prerelease: release["isPrerelease"].as_bool().unwrap_or(false),
            })
        })
        .collect();
    ReleasesView::Ready(Box::new(ReleasesSnapshot {
        repository,
        branch: name.to_string(),
        root,
        versions,
        tags,
        releases,
        upstream: None,
    }))
}

/// A node's commits, newest back to the next older tag; a worker runs it as the node unfolds.
#[must_use]
pub fn fetch_node(repo: &Path, repository: &RepoTarget, key: &NodeKey) -> NodeLoad {
    let cancelled = AtomicBool::new(false);
    let read = match &key.older {
        Some(older) => compare_range(repo, repository, older, &key.oid, &cancelled),
        None => history_to_start(repo, repository, &key.oid, &cancelled),
    };
    match read {
        Ok((commits, more)) => NodeLoad::Loaded { commits, more },
        Err(error) => NodeLoad::Failed(error),
    }
}

/// `older..newer` as `git log` reads it, through GitHub's compare, newest first and capped.
fn compare_range(
    repo: &Path,
    repository: &RepoTarget,
    older: &str,
    newer: &str,
    cancelled: &AtomicBool,
) -> Result<(Vec<ReleaseCommit>, bool), String> {
    let page = |number: usize| -> Result<Value, String> {
        let path = format!(
            "repos/{}/{}/compare/{older}...{newer}?per_page={PAGE}&page={number}",
            repository.owner(),
            repository.name()
        );
        let args = ["api", "--hostname", repository.host(), path.as_str()];
        let out =
            crate::forge::gh(repo, repository.host(), &args, cancelled).map_err(failure_text)?;
        serde_json::from_str(&out).map_err(|error| error.to_string())
    };
    // Compare pages run oldest first, so the newest commits are the last pages.
    let first = page(1)?;
    let total = usize::try_from(first["total_commits"].as_u64().unwrap_or(0)).unwrap_or(0);
    let pages = newest_pages(total);
    let more = *pages.start() > 1;
    let mut commits = Vec::new();
    for number in pages {
        let body = if number == 1 { first.clone() } else { page(number)? };
        compare_page(&body, &mut commits);
    }
    commits.reverse();
    Ok((commits, more))
}

/// The compare pages holding a range's newest `NODE_CAP` commits, for `total` in all.
fn newest_pages(total: usize) -> std::ops::RangeInclusive<usize> {
    let pages = total.div_ceil(PAGE).max(1);
    pages.saturating_sub(NODE_CAP / PAGE) + 1..=pages
}

/// A compare page's commits, oldest first, each by its oid and subject line.
fn compare_page(body: &Value, commits: &mut Vec<ReleaseCommit>) {
    let rows = body["commits"].as_array().into_iter().flatten();
    commits.extend(rows.filter_map(|commit| {
        let message = commit["commit"]["message"].as_str()?.trim_end();
        Some(ReleaseCommit {
            oid: commit["sha"].as_str()?.to_string(),
            subject: message.lines().next().unwrap_or_default().to_string(),
            message: message.to_string(),
            author: commit["commit"]["author"]["name"].as_str().unwrap_or_default().to_string(),
            date: commit["commit"]["author"]["date"].as_str().unwrap_or_default().to_string(),
            tags: Vec::new(),
        })
    }));
}

/// The oldest version's history from its commit back to the start, newest first and capped.
fn history_to_start(
    repo: &Path,
    repository: &RepoTarget,
    oid: &str,
    cancelled: &AtomicBool,
) -> Result<(Vec<ReleaseCommit>, bool), String> {
    let mut commits = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut vars = vars(repository).to_vec();
        vars.push(("oid".to_string(), oid.to_string()));
        if let Some(cursor) = &after {
            vars.push(("after".to_string(), cursor.clone()));
        }
        let page = crate::forge::graphql(repo, repository.host(), NODE_QUERY, &vars, cancelled)
            .map_err(failure_text)?;
        let history = &page["data"]["repository"]["object"]["history"];
        if history.is_null() {
            return Err(format!("{oid} not found"));
        }
        match node_page(history, &mut commits) {
            Some(cursor) if commits.len() < NODE_CAP => after = Some(cursor),
            Some(_) => return Ok((commits, true)),
            None => return Ok((commits, false)),
        }
    }
}

/// A failed node load's reason, short enough for its row.
fn failure_text(error: crate::forge::GhError) -> String {
    use crate::forge::GhError;
    match error {
        GhError::NoGh => "GitHub CLI not found".to_string(),
        GhError::NotAuthed(host) => format!("not signed in to {host}"),
        GhError::LocalGit(message) | GhError::NotFound(message) | GhError::Other(message) => {
            message
        }
    }
}

/// Add one history page's commits; the next page's cursor while history runs on.
fn node_page(history: &Value, commits: &mut Vec<ReleaseCommit>) -> Option<String> {
    commits.extend(nodes(history).filter_map(graphql_commit));
    let info = &history["pageInfo"];
    if info["hasNextPage"].as_bool() != Some(true) {
        return None;
    }
    info["endCursor"].as_str().map(String::from)
}

/// A connection's `nodes`, empty when the field is absent.
fn nodes(connection: &Value) -> impl Iterator<Item = &Value> {
    connection["nodes"].as_array().into_iter().flatten()
}

/// The `Releases` tab's view and its place state.
#[derive(Debug, Default)]
pub struct ReleasesTab {
    pub view: ReleasesView,
    /// A retryable failure's remedy, shown above a list that stays.
    notice: Option<String>,
    /// A same-input refresh that crossed the loading-indicator delay.
    refreshing: bool,
    /// The selected row of the tree.
    cursor: usize,
    /// The version nodes unfolded, by tag, so a fold outlives the rows around it.
    open: HashSet<String>,
    /// Each node's commits once unfolded, by range, so a refresh keeps them.
    loaded: HashMap<NodeKey, NodeLoad>,
    /// Node loads owed to a worker, taken after the frame paints.
    node_requests: Vec<NodeKey>,
    /// Top visible line of the notes.
    read_scroll: usize,
    /// The notes' maximum useful scroll, noted by the renderer each frame.
    read_max: Cell<usize>,
    /// Top visible row of the tree, independent of its selection.
    nav_scroll: Cell<usize>,
    /// The tree's maximum useful scroll, noted by the renderer each frame.
    nav_max: Cell<usize>,
    /// A cursor move asks the next frame to reveal the selected row.
    reveal: Cell<bool>,
    /// Open `<details>` in the selected release's notes.
    expanded: HashSet<String>,
    /// The refresh to dispatch after the frame paints.
    pending: Option<RefreshKind>,
    /// A new release being written, shown in place of the notes.
    pub draft: Option<crate::release_create::ReleaseDraft>,
    /// The last draft's id, so each draft's worker results stay its own.
    drafts: u64,
    /// Generate and publish requests owed to a worker.
    draft_requests: Vec<crate::release_create::Request>,
    /// The draft's notes wait for the editor, run between frames.
    notes_edit: bool,
    /// The release published here, by tag and URL, shown on its node.
    pub published: Option<(String, String)>,
    /// The tag to select once a refresh lists it.
    select_after: Option<String>,
    /// Each form field's painted screen row, for clicks.
    form_rows: std::cell::RefCell<Vec<(u16, crate::release_create::Field)>>,
    form_scroll: Cell<usize>,
    /// The review popup's scroll and its maximum, noted by the renderer.
    review_scroll: Cell<usize>,
    review_max: Cell<usize>,
}

impl ReleasesTab {
    /// Open the release form over the unreleased commits; nothing to release opens nothing.
    pub fn start_draft(&mut self) {
        if self.draft.is_some() {
            return;
        }
        let Some(target) = self.release_target().map(|commit| commit.oid.clone()) else { return };
        self.drafts += 1;
        self.draft = self
            .snapshot()
            .and_then(|s| crate::release_create::ReleaseDraft::new(s, self.drafts, &target));
        let request =
            self.draft.as_ref().map(crate::release_create::ReleaseDraft::categories_request);
        self.send(request);
    }

    /// Land the discussion categories on the form that asked.
    pub fn land_categories(&mut self, id: u64, categories: crate::release_create::Categories) {
        if let Some(draft) = self.draft.as_mut() {
            draft.categories_read(id, categories);
        }
    }

    /// The form field painted on screen row `y`, for a click.
    #[must_use]
    pub fn form_field_at(&self, y: u16) -> Option<crate::release_create::Field> {
        self.form_rows.borrow().iter().find(|(row, _)| *row == y).map(|(_, field)| *field)
    }

    /// Note where the form painted each field; the renderer calls this each frame.
    pub fn note_form_rows(&self, rows: Vec<(u16, crate::release_create::Field)>) {
        *self.form_rows.borrow_mut() = rows;
    }

    /// Scroll the review by `delta`; the review opens at its top.
    pub fn scroll_review(&mut self, delta: isize) {
        let next = crate::app::clamp_scroll(self.review_scroll.get(), delta, self.review_max.get());
        self.review_scroll.set(next);
    }

    /// Start a review at its top.
    pub fn reset_review_scroll(&mut self) {
        self.review_scroll.set(0);
    }

    /// Clamp the review's scroll to `max`, noting it; the renderer calls this each frame.
    pub fn settle_review(&self, max: usize) -> usize {
        self.review_max.set(max);
        let scroll = self.review_scroll.get().min(max);
        self.review_scroll.set(scroll);
        scroll
    }

    /// The form's scroll, so the focused field stays in view; the renderer settles it.
    #[must_use]
    pub fn form_scroll(&self) -> &Cell<usize> {
        &self.form_scroll
    }

    /// Queue a draft's request for a worker.
    pub fn send(&mut self, request: Option<crate::release_create::Request>) {
        self.draft_requests.extend(request);
    }

    /// The draft requests owed, taken for dispatch.
    pub fn take_draft_requests(&mut self) -> Vec<crate::release_create::Request> {
        std::mem::take(&mut self.draft_requests)
    }

    /// Ask for the draft's notes in the editor, unless generated notes are on their way.
    pub fn edit_notes(&mut self) {
        let Some(draft) = self.draft.as_mut() else { return };
        if draft.generating {
            draft.error = Some("Wait for the generated notes before editing them.".to_string());
            return;
        }
        self.notes_edit = true;
    }

    /// Whether the editor is owed the draft's notes, taken.
    pub fn take_notes_edit(&mut self) -> bool {
        std::mem::take(&mut self.notes_edit)
    }

    /// Land generated notes on the draft and request that asked.
    pub fn land_generated(&mut self, id: u64, seq: u64, notes: Result<String, String>) {
        if let Some(draft) = self.draft.as_mut() {
            draft.generated(id, seq, notes);
        }
    }

    /// Land a publish: success refreshes onto the new version, a failure stays on the draft.
    pub fn land_published(&mut self, id: u64, tag: String, url: Result<String, String>) {
        let Some(draft) = self.draft.as_mut().filter(|draft| draft.id == id) else { return };
        match url {
            Ok(url) => {
                self.draft = None;
                self.published = Some((tag.clone(), url));
                self.select_after = Some(tag);
                self.request(RefreshKind::Forced);
            }
            Err(error) => draft.failed(error),
        }
    }

    /// The resolved list, or `None` in a loading or degraded view.
    #[must_use]
    pub fn snapshot(&self) -> Option<&ReleasesSnapshot> {
        match &self.view {
            ReleasesView::Ready(snapshot) => Some(snapshot),
            _ => None,
        }
    }

    /// Node `node`'s load, if it ever unfolded.
    #[must_use]
    pub fn load(&self, node: usize) -> Option<&NodeLoad> {
        self.loaded.get(&self.snapshot()?.node_key(node))
    }

    /// The tree's rows under the current folds and loads.
    #[must_use]
    pub fn rows(&self) -> Vec<TreeRow> {
        let Some(s) = self.snapshot() else { return Vec::new() };
        let mut rows: Vec<TreeRow> =
            s.drafts().into_iter().map(|release| TreeRow::Draft { release }).collect();
        rows.extend((0..s.root.len()).map(|index| TreeRow::Commit { node: None, index }));
        for (node, version) in s.versions.iter().enumerate() {
            let open = self.open.contains(&version.tag);
            rows.push(TreeRow::Release { node, open });
            if !open {
                continue;
            }
            match self.load(node) {
                Some(NodeLoad::Loaded { commits, more }) => {
                    // The node row stands for its tagged commit, so no child repeats it.
                    let children = (0..commits.len()).filter(|&i| commits[i].oid != version.oid);
                    rows.extend(children.map(|index| TreeRow::Commit { node: Some(node), index }));
                    if *more {
                        rows.push(TreeRow::Note { node });
                    }
                }
                _ => rows.push(TreeRow::Note { node }),
            }
        }
        rows
    }

    /// The commit a commit row shows.
    #[must_use]
    pub fn commit(&self, node: Option<usize>, index: usize) -> Option<&ReleaseCommit> {
        match node {
            None => self.snapshot()?.root.get(index),
            Some(node) => match self.load(node)? {
                NodeLoad::Loaded { commits, .. } => commits.get(index),
                NodeLoad::Loading | NodeLoad::Failed(_) => None,
            },
        }
    }

    /// The selected row.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The commit the selected row shows, when it is a commit row.
    #[must_use]
    pub fn selected(&self) -> Option<&ReleaseCommit> {
        match *self.rows().get(self.cursor)? {
            TreeRow::Commit { node, index } => self.commit(node, index),
            TreeRow::Draft { .. } | TreeRow::Release { .. } | TreeRow::Note { .. } => None,
        }
    }

    /// The notes: a node's release or bare tag, for its rows too, or a root commit's state.
    #[must_use]
    pub fn notes(&self) -> Option<Notes<'_>> {
        let s = self.snapshot()?;
        match *self.rows().get(self.cursor)? {
            TreeRow::Release { node, .. } | TreeRow::Note { node } => Some(s.node_notes(node)),
            TreeRow::Draft { release } => Some(Notes::Release(&s.releases[release])),
            TreeRow::Commit { .. } => None,
        }
    }

    fn pick_at(&self, row: usize) -> Option<Pick> {
        let s = self.snapshot()?;
        let tag = |node: usize| s.versions[node].tag.clone();
        Some(match *self.rows().get(row)? {
            TreeRow::Draft { release } => {
                let release = &s.releases[release];
                Pick::Draft { id: release.id, tag: release.tag.clone() }
            }
            TreeRow::Release { node, .. } => Pick::Release(tag(node)),
            TreeRow::Note { node } => Pick::Note(tag(node)),
            TreeRow::Commit { node, index } => {
                Pick::Commit { oid: self.commit(node, index)?.oid.clone(), node: node.map(tag) }
            }
        })
    }

    /// The row `pick` stands on now; a commit that moved lands on its node, a gone node clamps.
    fn row_of(&self, pick: &Pick) -> Option<usize> {
        let s = self.snapshot()?;
        let rows = self.rows();
        let node_of = |tag: &str| s.versions.iter().position(|version| version.tag == tag);
        let node_row = |node: usize| {
            rows.iter()
                .position(|row| matches!(row, TreeRow::Release { node: n, .. } if *n == node))
        };
        match pick {
            // A draft published since stands on its version's node.
            Pick::Draft { id, tag } => rows
                .iter()
                .position(|row| {
                    matches!(row, TreeRow::Draft { release } if s.releases[*release].id == *id)
                })
                .or_else(|| node_row(node_of(tag)?)),
            Pick::Release(tag) => node_row(node_of(tag)?),
            Pick::Note(tag) => {
                let node = node_of(tag)?;
                rows.iter()
                    .position(|row| *row == TreeRow::Note { node })
                    .or_else(|| node_row(node))
            }
            Pick::Commit { oid, node } => rows
                .iter()
                .position(|row| match *row {
                    TreeRow::Commit { node, index } => {
                        self.commit(node, index).is_some_and(|c| c.oid == *oid)
                    }
                    _ => false,
                })
                .or_else(|| node_row(s.versions.iter().position(|v| v.oid == *oid)?))
                .or_else(|| node_row(node_of(node.as_deref()?)?)),
        }
    }

    /// Run `change`, then put the cursor back on the row it stood for, the list holding still.
    fn keep_place(&mut self, change: impl FnOnce(&mut Self)) {
        let pick = self.pick_at(self.cursor);
        change(self);
        let last = self.rows().len().saturating_sub(1);
        if let Some(row) = pick.as_ref().and_then(|pick| self.row_of(pick)) {
            let scroll = self.nav_scroll.get();
            self.nav_scroll.set((scroll + row).saturating_sub(self.cursor));
            self.cursor = row;
            // A commit that moved into a folded node lands on that node's notes.
            if self.pick_at(row) != pick {
                self.read_scroll = 0;
                self.expanded.clear();
            }
        } else {
            self.cursor = self.cursor.min(last);
            self.read_scroll = 0;
            self.expanded.clear();
        }
    }

    /// Whether the selected row is a version node, which folds.
    #[must_use]
    pub fn on_release(&self) -> bool {
        matches!(self.rows().get(self.cursor), Some(TreeRow::Release { .. }))
    }

    /// Unfold or fold the selected node; unfolding loads its commits unless they are loaded.
    pub fn set_open(&mut self, open: bool) {
        let Some(TreeRow::Release { node, .. }) = self.rows().get(self.cursor).copied() else {
            return;
        };
        let Some(s) = self.snapshot() else { return };
        let (tag, key) = (s.versions[node].tag.clone(), s.node_key(node));
        if !open {
            self.open.remove(&tag);
            return;
        }
        self.open.insert(tag);
        self.request_load(key);
    }

    /// Queue `key`'s load unless it is loaded or loading; a failed one tries again.
    fn request_load(&mut self, key: NodeKey) {
        if matches!(self.loaded.get(&key), Some(NodeLoad::Loaded { .. } | NodeLoad::Loading)) {
            return;
        }
        self.loaded.insert(key.clone(), NodeLoad::Loading);
        self.node_requests.push(key);
    }

    /// The node loads owed, taken for dispatch.
    pub fn take_node_requests(&mut self) -> Vec<NodeKey> {
        std::mem::take(&mut self.node_requests)
    }

    /// Land a node's load, its commits labeled with their tags; a range no longer asked is dropped.
    pub fn land_node(&mut self, key: &NodeKey, mut load: NodeLoad) {
        if self.loaded.get(key) != Some(&NodeLoad::Loading) {
            return;
        }
        if let (NodeLoad::Loaded { commits, .. }, Some(s)) = (&mut load, self.snapshot()) {
            for commit in commits {
                commit.tags = s.tags.get(&commit.oid).cloned().unwrap_or_default();
            }
        }
        self.keep_place(|tab| {
            tab.loaded.insert(key.clone(), load);
        });
    }

    /// Select row `row`, toggling its fold when it is a version node, as a folder click does.
    pub fn click(&mut self, row: usize) {
        self.select(row);
        if let Some(TreeRow::Release { open, .. }) = self.rows().get(row).copied()
            && self.cursor == row
        {
            self.set_open(!open);
        }
    }

    /// Whether commits wait above the newest version tag.
    #[must_use]
    pub fn has_unreleased(&self) -> bool {
        self.snapshot().is_some_and(ReleasesSnapshot::has_unreleased)
    }

    /// The unreleased commit under the cursor: the only row a release can be cut at.
    #[must_use]
    pub fn release_target(&self) -> Option<&ReleaseCommit> {
        match *self.rows().get(self.cursor)? {
            TreeRow::Commit { node: None, index } => self.snapshot()?.root.get(index),
            TreeRow::Commit { .. }
            | TreeRow::Draft { .. }
            | TreeRow::Release { .. }
            | TreeRow::Note { .. } => None,
        }
    }

    /// The remedy of a failed refresh, painted above the list it kept.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    #[must_use]
    pub fn refreshing(&self) -> bool {
        self.refreshing
    }

    /// Past the indicator delay: a first load says `loading…`, a refresh lights the glyph.
    pub fn set_refreshing(&mut self, refreshing: bool) {
        if refreshing && self.view == ReleasesView::Pending {
            self.view = ReleasesView::Loading;
            self.refreshing = false;
        } else {
            self.refreshing = refreshing;
        }
    }

    /// Queue a refresh; the stronger pending kind wins.
    pub fn request(&mut self, kind: RefreshKind) {
        self.pending = self.pending.max(Some(kind));
    }

    /// The queued refresh, taken for dispatch.
    pub fn take_pending(&mut self) -> Option<RefreshKind> {
        self.pending.take()
    }

    /// Apply a fetch: a failure keeps the list; place, folds, and loads follow tag and oid.
    pub fn apply(&mut self, view: ReleasesView, refresh: crate::keymap::Key) {
        self.refreshing = false;
        // A published tag is looked for in the first refresh that lands, and only there.
        if self.snapshot().is_some()
            && let Some(message) = view.retry_remedy(refresh)
        {
            self.notice = Some(message);
            self.select_after = None;
            return;
        }
        self.notice = None;
        let same = matches!((self.snapshot(), &view), (Some(old), ReleasesView::Ready(new))
            if old.repository.is(&new.repository) && old.branch == new.branch);
        if !same {
            self.view = view;
            self.reset();
            return;
        }
        self.keep_place(|tab| {
            tab.view = view;
            let Some(s) = tab.snapshot() else { return };
            // A range whose bounding tags still stand keeps its commits; any other is dropped.
            let keys: Vec<NodeKey> = (0..s.versions.len()).map(|node| s.node_key(node)).collect();
            tab.loaded.retain(|key, _| keys.contains(key));
            // An unfolded node whose range moved loads again; a failed one waits for an unfold.
            let reopen: Vec<NodeKey> = keys
                .into_iter()
                .filter(|key| tab.open.contains(&key.tag) && !tab.loaded.contains_key(key))
                .collect();
            for key in reopen {
                tab.request_load(key);
            }
        });
        self.select_published();
        self.select_after = None;
    }

    /// Select the version just published, once the list holds it.
    fn select_published(&mut self) {
        let Some(tag) = self.select_after.clone() else { return };
        let Some(s) = self.snapshot() else { return };
        // A published release's node, else the newest draft holding the tag.
        let rows = self.rows();
        let node = s.versions.iter().position(|v| v.tag == tag);
        let found = rows.iter().position(|row| match *row {
            TreeRow::Release { node: n, .. } => Some(n) == node,
            TreeRow::Draft { release } => node.is_none() && s.releases[release].tag == tag,
            TreeRow::Commit { .. } | TreeRow::Note { .. } => false,
        });
        if let Some(row) = found {
            self.cursor = row;
            self.read_scroll = 0;
            self.expanded.clear();
            self.reveal.set(true);
        }
    }

    /// Back to the first row, as a new repository or branch starts.
    fn reset(&mut self) {
        self.cursor = 0;
        self.select_after = None;
        self.open.clear();
        self.loaded.clear();
        self.node_requests.clear();
        self.read_scroll = 0;
        self.nav_scroll.set(0);
        self.reveal.set(true);
        self.expanded.clear();
    }

    /// Move the selection by `delta` rows.
    pub fn move_by(&mut self, delta: isize) {
        let n = self.rows().len();
        if n > 0 {
            self.select(crate::app::step(self.cursor, delta, n));
        }
    }

    /// Select row `row`, its notes from the top.
    pub fn select(&mut self, row: usize) {
        // The reader moved: a published tag no longer takes the cursor.
        self.select_after = None;
        let n = self.rows().len();
        if row >= n || row == self.cursor {
            return;
        }
        self.cursor = row;
        self.read_scroll = 0;
        self.reveal.set(true);
        self.expanded.clear();
    }

    /// Scroll the tree by `delta`, leaving the selection.
    pub fn scroll_nav(&mut self, delta: isize) {
        self.reveal.set(false);
        self.nav_scroll.set(crate::app::clamp_scroll(
            self.nav_scroll.get(),
            delta,
            self.nav_max.get(),
        ));
    }

    /// Scroll the notes by `delta`, clamping first so a stale scroll never eats input.
    pub fn scroll_read(&mut self, delta: isize) {
        self.read_scroll = crate::app::clamp_scroll(self.read_scroll, delta, self.read_max.get());
    }

    /// Bring notes line `line` to the top, as far as the notes reach.
    pub fn scroll_read_to(&mut self, line: usize) {
        self.read_scroll = line.min(self.read_max.get());
    }

    #[must_use]
    pub fn read_scroll(&self) -> usize {
        self.read_scroll
    }

    /// Note the notes' maximum useful scroll; the renderer calls this each frame.
    pub fn note_read_max(&self, max: usize) {
        self.read_max.set(max);
    }

    #[must_use]
    pub fn nav_scroll(&self) -> usize {
        self.nav_scroll.get()
    }

    /// Settle the tree's scroll for a `viewport` of rows, revealing the selection when asked.
    pub fn settle_nav(&self, rows: usize, viewport: usize) -> usize {
        let max = rows.saturating_sub(viewport);
        let mut scroll = self.nav_scroll.get().min(max);
        if viewport > 0 && self.cursor < rows && self.reveal.replace(false) {
            if self.cursor < scroll {
                scroll = self.cursor;
            } else if self.cursor >= scroll + viewport {
                scroll = self.cursor + 1 - viewport;
            }
        }
        self.nav_max.set(max);
        self.nav_scroll.set(scroll);
        scroll
    }

    /// The `<details>` opened in the selected notes.
    #[must_use]
    pub fn expanded(&self) -> &HashSet<String> {
        &self.expanded
    }

    pub fn toggle_details(&mut self, key: &str) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
    }

    /// Open every disclosure in `keys`.
    pub fn expand_details(&mut self, keys: impl IntoIterator<Item = String>) {
        self.expanded.extend(keys);
    }

    pub fn collapse_details(&mut self) {
        self.expanded.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Key;
    use serde_json::json;

    fn target(owner: &str) -> RepoTarget {
        RepoTarget::new("github.com", owner, "repo").unwrap()
    }

    fn commit(oid: &str, tags: &[&str]) -> ReleaseCommit {
        ReleaseCommit {
            oid: oid.into(),
            subject: format!("subject {oid}"),
            message: format!("subject {oid}\n\nbody of {oid}"),
            author: "Ann".into(),
            date: "2026-10-01T12:00:00Z".into(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
        }
    }

    fn version_tag(tag: &str, oid: &str) -> VersionTag {
        VersionTag { tag: tag.into(), oid: oid.into(), commit: commit(oid, &[tag]) }
    }

    fn release(tag: &str, notes: &str) -> Release {
        Release { tag: tag.into(), name: tag.into(), notes: notes.into(), ..Release::default() }
    }

    fn snapshot(root: Vec<ReleaseCommit>) -> ReleasesSnapshot {
        ReleasesSnapshot {
            repository: target("o"),
            branch: "main".into(),
            root,
            versions: vec![],
            tags: HashMap::new(),
            releases: vec![release("v2", "notes two")],
            upstream: None,
        }
    }

    fn ready(owner: &str, branch: &str, root: Vec<ReleaseCommit>) -> ReleasesView {
        ReleasesView::Ready(Box::new(ReleasesSnapshot {
            repository: target(owner),
            branch: branch.into(),
            ..snapshot(root)
        }))
    }

    fn oids(n: usize) -> Vec<ReleaseCommit> {
        (0..n).map(|i| commit(&format!("c{i}"), &[])).collect()
    }

    const R: Key = Key::plain('r');

    #[test]
    fn parse_lists_every_version_tag_highest_first_and_only_newer_commits_at_the_root() {
        let response = json!({"data": {"repository": {
            "defaultBranchRef": {"name": "main", "target": {"oid": "c4", "history": {"nodes": [
                {"oid": "c4", "message": "fourth\n\nWhy it changed.\n", "committedDate": "2026-10-02",
                 "author": {"name": "Ann"}},
                {"oid": "c3", "message": "third"},
                {"oid": "c2", "message": "second"},
            ]}}},
            "refs": {"nodes": [
                {"name": "nightly", "target": {"oid": "c4"}},
                // Annotated: the ref's target is the tag object, the commit one step further.
                {"name": "v0.10.0", "target": {"oid": "tagobject",
                 "target": {"oid": "c3", "message": "third\n\nThe tagged one."}}},
                // Lightweight, and older than the history read: still a node.
                {"name": "0.9.0", "target": {"oid": "c0"}},
                {"name": "v0.9.1", "target": {"oid": "c1"}},
            ]},
            "releases": {"nodes": [
                {"databaseId": 41, "tagName": "v0.10.0", "name": "Ten",
                 "description": "## Notes", "isDraft": false, "isPrerelease": true},
                {"databaseId": 42, "tagName": "", "name": "untagged draft", "description": "",
                 "isDraft": true, "isPrerelease": false},
                {"databaseId": 43, "tagName": "", "name": "no tag", "description": "",
                 "isDraft": false, "isPrerelease": false},
            ]},
        }}});
        let ReleasesView::Ready(s) = parse(&response, target("o")) else { panic!("a list") };
        assert_eq!(s.branch, "main");
        let versions: Vec<(&str, &str)> =
            s.versions.iter().map(|v| (v.tag.as_str(), v.oid.as_str())).collect();
        assert_eq!(
            versions,
            vec![("v0.10.0", "c3"), ("v0.9.1", "c1"), ("0.9.0", "c0")],
            "by version, not by text, and never a plain tag"
        );
        assert_eq!(
            s.root,
            vec![ReleaseCommit {
                oid: "c4".into(),
                subject: "fourth".into(),
                message: "fourth\n\nWhy it changed.".into(),
                author: "Ann".into(),
                date: "2026-10-02".into(),
                tags: vec!["nightly".into()],
            }],
            "the root stops at the newest version, each commit with its whole message"
        );
        assert_eq!(
            s.versions[0].commit.message, "third\n\nThe tagged one.",
            "the tag's commit, peeled"
        );
        assert_eq!(s.tags["c3"], vec!["v0.10.0".to_string()]);
        assert_eq!(
            s.releases,
            vec![
                Release {
                    id: 41,
                    tag: "v0.10.0".into(),
                    name: "Ten".into(),
                    notes: "## Notes".into(),
                    draft: false,
                    prerelease: true,
                },
                Release {
                    id: 42,
                    name: "untagged draft".into(),
                    draft: true,
                    ..Release::default()
                },
            ],
            "a draft may hold no tag yet; a published release without one is dropped"
        );
        assert_eq!(
            s.node_key(0),
            NodeKey { tag: "v0.10.0".into(), oid: "c3".into(), older: Some("c1".into()) }
        );
        assert_eq!(s.node_key(2).older, None, "the oldest version runs to the start");
    }

    #[test]
    fn a_history_page_says_when_more_remain() {
        let page = |oids: &[&str], next: bool| {
            let nodes: Vec<_> =
                oids.iter().map(|oid| json!({"oid": oid, "message": oid})).collect();
            json!({"pageInfo": {"hasNextPage": next, "endCursor": "cursor"}, "nodes": nodes})
        };
        let mut commits = Vec::new();
        assert_eq!(node_page(&page(&["c2", "c1"], true), &mut commits).as_deref(), Some("cursor"));
        assert_eq!(node_page(&page(&["c0"], false), &mut commits), None, "the start");
        let oids: Vec<&str> = commits.iter().map(|c| c.oid.as_str()).collect();
        assert_eq!(oids, ["c2", "c1", "c0"]);
    }

    #[test]
    fn a_long_range_reads_only_its_newest_pages() {
        assert_eq!(newest_pages(0), 1..=1);
        assert_eq!(newest_pages(167), 1..=2);
        assert_eq!(newest_pages(500), 1..=5);
        assert_eq!(newest_pages(501), 2..=6, "past the cap, the oldest page is left out");
        assert_eq!(newest_pages(1000), 6..=10);
    }

    #[test]
    fn a_compare_page_reads_each_commits_oid_and_subject_line() {
        let body = json!({"total_commits": 2, "commits": [
            {"sha": "c1", "commit": {"message": "First line\n\nBody text",
             "author": {"name": "Ann", "date": "2026-10-01T12:00:00Z"}}},
            {"sha": "c2", "commit": {"message": "Second"}},
            {"commit": {"message": "no sha"}},
        ]});
        let mut commits = Vec::new();
        compare_page(&body, &mut commits);
        assert_eq!(
            commits,
            [
                ReleaseCommit {
                    oid: "c1".into(),
                    subject: "First line".into(),
                    message: "First line\n\nBody text".into(),
                    author: "Ann".into(),
                    date: "2026-10-01T12:00:00Z".into(),
                    tags: vec![],
                },
                ReleaseCommit {
                    oid: "c2".into(),
                    subject: "Second".into(),
                    message: "Second".into(),
                    ..ReleaseCommit::default()
                },
            ]
        );
    }

    #[test]
    fn parse_reports_an_empty_repository_and_a_missing_one_distinctly() {
        let empty = json!({"data": {"repository": {
            "defaultBranchRef": null, "refs": {"nodes": []}, "releases": {"nodes": []},
        }}});
        assert_eq!(parse(&empty, target("o")), ReleasesView::NoDefaultBranch);
        let missing = json!({"data": {"repository": null}});
        assert_eq!(parse(&missing, target("o")), ReleasesView::Error("o/repo not found".into()));
    }

    #[test]
    fn a_version_shows_its_release_or_its_bare_tag() {
        let mut s = snapshot(vec![]);
        s.versions = vec![version_tag("v2", "a")];
        assert_eq!(s.node_notes(0), Notes::Release(&s.releases[0]));
        s.releases.clear();
        assert_eq!(s.node_notes(0), Notes::TagOnly(&s.versions[0]), "a version with no release");
    }

    #[test]
    fn every_degraded_state_says_something_distinct() {
        let states = [
            ReleasesView::Loading,
            ReleasesView::NotGitHub(Forge::GitLab),
            ReleasesView::NotGitHub(Forge::AzureDevOps),
            ReleasesView::NeedsForgeRemote,
            ReleasesView::UnsupportedHost("code.corp".into()),
            ReleasesView::MalformedOrigin("github.com".into()),
            ReleasesView::NoDefaultBranch,
            ReleasesView::NoCli,
            ReleasesView::NotAuthed("github.com".into()),
            ReleasesView::GitError("boom".into()),
            ReleasesView::Error("rate limited".into()),
        ];
        let messages: Vec<String> =
            states.iter().map(|s| s.message(R).expect("a message")).collect();
        let unique: HashSet<&String> = messages.iter().collect();
        assert_eq!(unique.len(), messages.len(), "{messages:#?}");
        assert!(messages[1].contains("GitLab") && messages[2].contains("Azure DevOps"));
        assert!(messages[7].contains("Install `gh`"), "{}", messages[7]);
        assert!(messages[8].contains("gh auth login --hostname github.com"), "{}", messages[8]);
        assert_eq!(ReleasesView::Pending.message(R), None, "a quiet wait paints nothing");
        assert_eq!(ready("o", "main", vec![]).message(R), None);
    }

    #[test]
    fn a_retryable_failure_keeps_the_list_and_its_place_with_a_notice() {
        let mut tab = ReleasesTab::default();
        tab.apply(ready("o", "main", oids(5)), R);
        tab.select(3);
        tab.apply(ReleasesView::Error("offline".into()), R);
        assert_eq!(tab.selected().map(|c| c.oid.as_str()), Some("c3"));
        assert!(tab.notice().is_some_and(|n| n.contains("offline")));
        tab.apply(ready("o", "main", oids(5)), R);
        assert_eq!(tab.notice(), None, "a landed list clears the notice");
        // Without a list, the failure is the view itself.
        let mut fresh = ReleasesTab::default();
        fresh.apply(ReleasesView::NoCli, R);
        assert_eq!(fresh.view, ReleasesView::NoCli);
        assert_eq!(fresh.notice(), None);
    }

    #[test]
    fn a_refresh_follows_the_selected_commit_by_oid_not_by_row() {
        let mut tab = ReleasesTab::default();
        tab.apply(ready("o", "main", oids(30)), R);
        tab.select(10);
        tab.scroll_read(0);
        tab.read_max.set(50);
        tab.scroll_read(7);
        tab.nav_max.set(30);
        tab.scroll_nav(5);
        // Two commits land on top: the selection is the same commit two rows down.
        let mut grown = vec![commit("n1", &[]), commit("n0", &[])];
        grown.extend(oids(30));
        tab.apply(ready("o", "main", grown), R);
        assert_eq!(tab.cursor(), 12);
        assert_eq!(tab.selected().map(|c| c.oid.as_str()), Some("c10"));
        assert_eq!(tab.read_scroll(), 7, "the same commit keeps its notes' scroll");
        assert_eq!(tab.nav_scroll(), 7, "the list scrolls with the row, so it holds still");
    }

    #[test]
    fn a_vanished_commit_falls_back_to_the_nearest_row_and_a_new_identity_resets() {
        let mut tab = ReleasesTab::default();
        tab.apply(ready("o", "main", oids(10)), R);
        tab.select(8);
        tab.read_max.set(50);
        tab.scroll_read(4);
        // A force-push rewrote history: c8 is gone, and the list is shorter.
        tab.apply(ready("o", "main", (0..5).map(|i| commit(&format!("d{i}"), &[])).collect()), R);
        assert_eq!(tab.cursor(), 4, "clamped to the last surviving row");
        assert_eq!(tab.read_scroll(), 0, "new notes start at the top");

        tab.select(2);
        tab.apply(
            ready("other", "main", (0..5).map(|i| commit(&format!("d{i}"), &[])).collect()),
            R,
        );
        assert_eq!(tab.cursor(), 0, "another repository is a new list, even with equal oids");
        tab.select(2);
        tab.apply(
            ready("other", "trunk", (0..5).map(|i| commit(&format!("d{i}"), &[])).collect()),
            R,
        );
        assert_eq!(tab.cursor(), 0, "another default branch is a new list");
    }

    #[test]
    fn loading_shows_only_for_a_first_load_and_the_glyph_for_a_refresh() {
        let mut tab = ReleasesTab::default();
        tab.set_refreshing(true);
        assert_eq!(tab.view, ReleasesView::Loading);
        assert!(!tab.refreshing());
        tab.apply(ready("o", "main", oids(2)), R);
        tab.set_refreshing(true);
        assert!(tab.refreshing(), "a painted list stays, the glyph signals");
        assert!(matches!(tab.view, ReleasesView::Ready(_)));
        tab.apply(ready("o", "main", oids(2)), R);
        assert!(!tab.refreshing(), "a landing ends the signal");
    }

    #[test]
    fn moving_selects_within_the_list_and_reveals_it() {
        let mut tab = ReleasesTab::default();
        tab.move_by(1);
        assert_eq!(tab.cursor(), 0, "no list, no move");
        tab.apply(ready("o", "main", oids(3)), R);
        tab.reveal.set(false);
        tab.move_by(5);
        assert_eq!(tab.cursor(), 2);
        assert!(tab.reveal.get());
        tab.move_by(-9);
        assert_eq!(tab.cursor(), 0);
        tab.select(7);
        assert_eq!(tab.cursor(), 0, "an out-of-range row selects nothing");
    }

    #[test]
    fn settle_nav_reveals_the_selection_only_when_asked() {
        let mut tab = ReleasesTab::default();
        tab.apply(ready("o", "main", oids(30)), R);
        tab.select(20);
        assert_eq!(tab.settle_nav(30, 10), 11, "revealed at the bottom edge");
        tab.scroll_nav(-11);
        assert_eq!(tab.settle_nav(30, 10), 0, "a wheel scroll is never pulled back");
        tab.move_by(-1);
        assert_eq!(tab.settle_nav(30, 10), 10);
        assert_eq!(tab.settle_nav(5, 10), 0, "a short list never scrolls");
    }

    #[test]
    fn the_stronger_pending_refresh_wins() {
        let mut tab = ReleasesTab::default();
        tab.request(RefreshKind::Ambient);
        tab.request(RefreshKind::Forced);
        tab.request(RefreshKind::Ambient);
        assert_eq!(tab.take_pending(), Some(RefreshKind::Forced));
        assert_eq!(tab.take_pending(), None);
    }

    #[test]
    fn fetch_reads_origin_never_the_pr_tabs_upstream() {
        let (dir, git) = crate::test_support::test_repo();
        let hosts = ForgeHosts::default();
        let cancelled = AtomicBool::new(false);
        assert_eq!(fetch(dir.path(), &hosts, &cancelled), ReleasesView::NeedsForgeRemote);
        // An upstream alone is not where releases are cut.
        git(&["remote", "add", "upstream", "git@github.com:upstream/repo.git"]);
        assert_eq!(fetch(dir.path(), &hosts, &cancelled), ReleasesView::NeedsForgeRemote);
        // The PR tab would read the GitHub upstream; this tab reads origin.
        git(&["remote", "add", "origin", "git@gitlab.com:group/project.git"]);
        assert_eq!(fetch(dir.path(), &hosts, &cancelled), ReleasesView::NotGitHub(Forge::GitLab));
        git(&["remote", "set-url", "origin", "https://dev.azure.com/org/project/_git/repo"]);
        assert_eq!(
            fetch(dir.path(), &hosts, &cancelled),
            ReleasesView::NotGitHub(Forge::AzureDevOps)
        );
        git(&["remote", "set-url", "origin", "https://code.corp.example/o/r.git"]);
        assert_eq!(
            fetch(dir.path(), &hosts, &cancelled),
            ReleasesView::UnsupportedHost("code.corp.example".into())
        );
    }

    #[test]
    fn upstream_keeps_its_latest_only_when_it_is_newer() {
        let flagged = |latest: Option<&str>, origin: Option<&str>| {
            Upstream::flagged(target("up"), latest.map(String::from), origin).newer
        };
        assert_eq!(flagged(Some("v0.47.0"), Some("v0.46.0")).as_deref(), Some("v0.47.0"));
        assert_eq!(flagged(Some("v0.46.0"), Some("v0.46.0")), None, "equal");
        assert_eq!(flagged(Some("v0.45.0"), Some("v0.46.0")), None, "older");
        assert_eq!(flagged(None, Some("v0.46.0")), None, "unreadable or releaseless upstream");
    }

    #[test]
    fn two_leading_vs_are_not_a_version_tag() {
        assert!(version("vv1.0.0").is_none(), "one `v` is a tag style, two are not");
        assert!(version(" v1.0.0 ").is_some());
    }

    #[test]
    fn a_version_tag_is_semver_with_or_without_its_v() {
        let highest = |tags: &[&'static str]| highest_version(tags.iter().copied());
        assert_eq!(highest(&["v1.2.3"]), Some("v1.2.3"));
        assert_eq!(highest(&["1.2.3"]), Some("1.2.3"));
        assert_eq!(highest(&["v1.2.3-rc.1"]), Some("v1.2.3-rc.1"));
        assert_eq!(highest(&["nightly", "v1.2", "release-1"]), None);
        assert_eq!(highest(&["v1.9.0", "v1.10.0", "nightly"]), Some("v1.10.0"));
    }

    #[test]
    fn origin_highest_reads_every_version_tag_and_the_latest_release() {
        let response = |tags: &[&str], latest: Option<&str>| {
            let refs: Vec<_> =
                tags.iter().map(|t| json!({"name": t, "target": {"oid": "c"}})).collect();
            json!({"data": {"repository": {
                "refs": {"nodes": refs},
                "latestRelease": latest.map(|t| json!({"tagName": t})),
            }}})
        };
        let tags_only = response(&["v0.46.0", "v0.45.0", "nightly"], None);
        assert_eq!(origin_highest(&tags_only), Some("v0.46.0"), "tags count without a release");
        let flagged = |latest: &str, origin: &Value| {
            Upstream::flagged(target("up"), Some(latest.into()), origin_highest(origin)).newer
        };
        assert_eq!(flagged("v0.46.0", &tags_only), None, "a fork at the same tag sees nothing");
        assert_eq!(flagged("v0.47.0", &tags_only).as_deref(), Some("v0.47.0"));
        let released = response(&["v0.1.0"], Some("v0.50.0"));
        assert_eq!(origin_highest(&released), Some("v0.50.0"), "or its latest release");
        assert_eq!(origin_highest(&response(&["nightly"], None)), None);
    }

    #[test]
    fn only_a_higher_upstream_version_is_newer() {
        assert!(is_newer("v0.47.0", Some("v0.46.0")));
        assert!(is_newer("0.47.0", Some("v0.46.9")), "the `v` is optional on either side");
        assert!(is_newer("v1.0.0", Some("v1.0.0-rc.1")), "a release outranks its candidate");
        assert!(is_newer("v0.10.0", Some("v0.9.0")), "compared as versions, never as text");
        assert!(!is_newer("v0.46.0", Some("v0.46.0")), "equal");
        assert!(!is_newer("v0.45.0", Some("v0.46.0")), "older");
        assert!(!is_newer("nightly", Some("v0.46.0")), "an unparsable upstream");
        assert!(!is_newer("v0.47.0", Some("latest")), "an unparsable origin");
        assert!(is_newer("v0.1.0", None), "origin has no release yet");
    }

    #[test]
    fn the_notice_names_only_a_newer_upstream_version() {
        let mut snapshot = snapshot(vec![]);
        assert_eq!(snapshot.upstream_notice(), None);
        snapshot.upstream = Some(Upstream { repository: target("up"), newer: None });
        assert_eq!(snapshot.upstream_notice(), None, "an upstream with nothing newer is quiet");
        snapshot.upstream =
            Some(Upstream { repository: target("up"), newer: Some("v9.0.0".into()) });
        assert_eq!(snapshot.upstream_notice().as_deref(), Some("v9.0.0 is newer"));
        assert_eq!(web_url(&target("up")), "https://github.com/up/repo");
    }

    /// Root: f and n (plain tag); nodes: v2.0.0 (a, b), v1.0.0 (c, tag only), v0.9.0 (d, e).
    fn tree_sample() -> ReleasesSnapshot {
        let mut second = release("v2.0.0", "## Highlights\n\n- a **bold** change");
        second.name = "Second".into();
        let mut first = release("v0.9.0", "");
        first.name = String::new();
        let version = version_tag;
        let tags = [("aaaaaaa111", "v2.0.0"), ("ccccccc333", "v1.0.0"), ("ddddddd444", "v0.9.0")];
        let mut tags: HashMap<String, Vec<String>> =
            tags.iter().map(|(oid, tag)| ((*oid).to_string(), vec![(*tag).to_string()])).collect();
        tags.insert("nnnnnnn999".into(), vec!["nightly".into()]);
        ReleasesSnapshot {
            repository: target("o"),
            branch: "main".into(),
            root: vec![commit("fffffff000", &[]), commit("nnnnnnn999", &["nightly"])],
            versions: vec![
                version("v2.0.0", "aaaaaaa111"),
                version("v1.0.0", "ccccccc333"),
                version("v0.9.0", "ddddddd444"),
            ],
            tags,
            releases: vec![second, first],
            upstream: None,
        }
    }

    /// The commits each sample node holds, as a node load returns them, untagged.
    fn node_commits(tag: &str) -> Vec<ReleaseCommit> {
        let oids: &[&str] = match tag {
            "v2.0.0" => &["aaaaaaa111", "bbbbbbb222"],
            "v1.0.0" => &["ccccccc333"],
            _ => &["ddddddd444", "eeeeeee555"],
        };
        oids.iter().map(|oid| commit(oid, &[])).collect()
    }

    /// Land every owed node load as the worker would, and say which loaded.
    fn land_all(tab: &mut ReleasesTab) -> Vec<String> {
        let keys = tab.take_node_requests();
        for key in &keys {
            let load = NodeLoad::Loaded { commits: node_commits(&key.tag), more: false };
            tab.land_node(key, load);
        }
        keys.into_iter().map(|key| key.tag).collect()
    }

    #[test]
    fn every_version_is_a_folded_node_and_a_folded_node_loads_nothing() {
        use TreeRow::{Commit, Release};
        let mut tab = ReleasesTab::default();
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        assert_eq!(
            tab.rows(),
            vec![
                Commit { node: None, index: 0 },
                Commit { node: None, index: 1 },
                Release { node: 0, open: false },
                Release { node: 1, open: false },
                Release { node: 2, open: false },
            ]
        );
        tab.move_by(4);
        assert!(tab.take_node_requests().is_empty(), "moving over nodes fetches nothing");
        assert!(tab.has_unreleased());
        let released = ReleasesSnapshot { root: vec![], ..tree_sample() };
        assert!(!released.has_unreleased(), "nothing newer than the newest version");
    }

    /// The row of version `tag`'s node.
    fn node_row(tab: &ReleasesTab, tag: &str) -> usize {
        let node = tab.snapshot().unwrap().versions.iter().position(|v| v.tag == tag).unwrap();
        tab.rows()
            .iter()
            .position(|row| matches!(row, TreeRow::Release { node: n, .. } if *n == node))
            .unwrap()
    }

    /// Select version `tag`'s node and unfold it.
    fn unfold(tab: &mut ReleasesTab, tag: &str) {
        tab.select(node_row(tab, tag));
        tab.set_open(true);
    }

    /// The tree as names: `[tag]` for a node, a commit's first letter, `note` for a note.
    fn row_names(tab: &ReleasesTab) -> Vec<String> {
        tab.rows()
            .iter()
            .map(|row| match *row {
                TreeRow::Release { node, .. } => {
                    format!("[{}]", tab.snapshot().unwrap().versions[node].tag)
                }
                TreeRow::Commit { node, index } => {
                    tab.commit(node, index).unwrap().oid[..1].to_string()
                }
                TreeRow::Note { .. } => "note".into(),
                TreeRow::Draft { release } => {
                    format!("draft {}", tab.snapshot().unwrap().releases[release].tag)
                }
            })
            .collect()
    }

    #[test]
    fn unfolding_loads_exactly_that_node_once_and_a_failure_retries_on_the_next_unfold() {
        use TreeRow::{Note, Release};
        let mut tab = ReleasesTab::default();
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        unfold(&mut tab, "v2.0.0");
        assert_eq!(tab.load(0), Some(&NodeLoad::Loading));
        assert_eq!(tab.rows()[3], Note { node: 0 }, "a loading node says so under itself");
        assert_eq!(land_all(&mut tab), ["v2.0.0"], "only the unfolded node");
        assert_eq!(row_names(&tab), ["f", "n", "[v2.0.0]", "b", "[v1.0.0]", "[v0.9.0]"]);
        assert_eq!(tab.commit(Some(0), 0).unwrap().tags, ["v2.0.0"], "labeled from the tag list");
        tab.set_open(false);
        tab.set_open(true);
        assert!(tab.take_node_requests().is_empty(), "a loaded node unfolds from its cache");

        unfold(&mut tab, "v1.0.0");
        assert_eq!(tab.rows()[4], Release { node: 1, open: true });
        let keys = tab.take_node_requests();
        tab.land_node(&keys[0], NodeLoad::Failed("rate limited".into()));
        assert_eq!(tab.rows()[5], Note { node: 1 }, "the failure shows on its node");
        assert_eq!(tab.load(1), Some(&NodeLoad::Failed("rate limited".into())));
        // A refresh, the minute refetch included, leaves a failed node alone.
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        assert!(tab.take_node_requests().is_empty(), "a failed node waits for an unfold");
        tab.set_open(false);
        tab.set_open(true);
        assert_eq!(land_all(&mut tab), ["v1.0.0"], "unfolding again retries");
    }

    #[test]
    fn a_refresh_fetches_no_node_and_drops_a_cache_only_when_a_bounding_tag_moved() {
        let mut tab = ReleasesTab::default();
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        for tag in ["v2.0.0", "v0.9.0"] {
            unfold(&mut tab, tag);
            land_all(&mut tab);
        }
        tab.select(3);
        assert_eq!(tab.selected().map(|c| c.oid.as_str()), Some("bbbbbbb222"));
        // A commit lands at the root: no node fetches, the loads and the place hold.
        let mut grown = tree_sample();
        grown.root.insert(0, commit("ggggggg777", &[]));
        tab.apply(ReleasesView::Ready(Box::new(grown.clone())), R);
        assert!(tab.take_node_requests().is_empty(), "a refresh issues no node request");
        assert_eq!(tab.selected().map(|c| c.oid.as_str()), Some("bbbbbbb222"));
        assert!(matches!(tab.load(2), Some(NodeLoad::Loaded { .. })));
        // v1.0.0 moves: v2.0.0's range ends there, so only its cache goes, and it reloads.
        grown.versions[1].oid = "ccccccc000".into();
        tab.apply(ReleasesView::Ready(Box::new(grown)), R);
        assert_eq!(
            tab.take_node_requests().iter().map(|k| k.tag.as_str()).collect::<Vec<_>>(),
            ["v2.0.0"]
        );
        assert_eq!(tab.load(0), Some(&NodeLoad::Loading));
        assert!(matches!(tab.load(2), Some(NodeLoad::Loaded { .. })), "v0.9.0's range stands");
        assert_eq!(tab.loaded.len(), 2, "the old v2.0.0 range is gone from the cache");
        assert!(tab.on_release(), "a commit whose node reloads lands on the node");
        // The old range's late result is dropped; the new one lands.
        let stale = NodeKey {
            tag: "v1.0.0".into(),
            oid: "ccccccc333".into(),
            older: Some("ddddddd444".into()),
        };
        tab.land_node(&stale, NodeLoad::Loaded { commits: vec![], more: false });
        assert!(!tab.loaded.contains_key(&stale), "a range nobody asked for never lands");
    }

    #[test]
    fn a_deleted_or_new_older_tag_changes_the_range_it_bounds() {
        let mut tab = ReleasesTab::default();
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        unfold(&mut tab, "v2.0.0");
        land_all(&mut tab);
        // v1.0.0 is deleted: v2.0.0 now runs down to v0.9.0, a new range.
        let mut deleted = tree_sample();
        deleted.versions.remove(1);
        tab.apply(ReleasesView::Ready(Box::new(deleted)), R);
        let keys = tab.take_node_requests();
        assert_eq!(keys.len(), 1, "{keys:?}");
        assert_eq!(keys[0].older.as_deref(), Some("ddddddd444"), "bounded by the next older tag");
        tab.land_node(&keys[0], NodeLoad::Loaded { commits: node_commits("v2.0.0"), more: false });
        // A new tag lands between v2.0.0 and v0.9.0: v2.0.0's range ends there now.
        let mut inserted = tree_sample();
        inserted.versions[1] = version_tag("v1.5.0", "bbbbbbb222");
        tab.apply(ReleasesView::Ready(Box::new(inserted)), R);
        let keys = tab.take_node_requests();
        assert_eq!(
            keys.iter().map(|k| k.older.as_deref()).collect::<Vec<_>>(),
            [Some("bbbbbbb222")]
        );
        assert_eq!(tab.loaded.len(), 1, "only the range in use is cached");
        // The node's own tag deleted: the node is gone, and so is its cache.
        let mut gone = tree_sample();
        gone.versions.remove(0);
        tab.apply(ReleasesView::Ready(Box::new(gone)), R);
        assert!(tab.loaded.keys().all(|key| key.tag != "v2.0.0"), "{:?}", tab.loaded.keys());
    }

    #[test]
    fn a_node_landing_above_the_cursor_keeps_the_cursor_on_its_row() {
        let mut tab = ReleasesTab::default();
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        unfold(&mut tab, "v2.0.0");
        tab.select(node_row(&tab, "v0.9.0"));
        let before = tab.pick_at(tab.cursor());
        let keys = tab.take_node_requests();
        let three = ["aaaaaaa111", "bbbbbbb222", "bbbbbbb333", "bbbbbbb444"];
        let load = NodeLoad::Loaded {
            commits: three.iter().map(|oid| commit(oid, &[])).collect(),
            more: false,
        };
        tab.land_node(&keys[0], load);
        assert_eq!(tab.pick_at(tab.cursor()), before, "three children replaced one note above it");
        assert_eq!(tab.cursor(), node_row(&tab, "v0.9.0"));
        assert_eq!(tab.cursor(), 7);
    }

    #[test]
    fn each_version_shows_once_its_node_row_standing_for_its_tagged_commit() {
        let mut tab = ReleasesTab::default();
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        for tag in ["v0.9.0", "v1.0.0", "v2.0.0"] {
            unfold(&mut tab, tag);
        }
        land_all(&mut tab);
        assert_eq!(row_names(&tab), ["f", "n", "[v2.0.0]", "b", "[v1.0.0]", "[v0.9.0]", "e"]);
        // A selection on a tagged commit lands on its node.
        let mut cut = tree_sample();
        cut.root.remove(0);
        cut.versions.insert(0, version_tag("v3.0.0", "fffffff000"));
        tab.select(0);
        tab.apply(ReleasesView::Ready(Box::new(cut)), R);
        assert_eq!(tab.cursor(), node_row(&tab, "v3.0.0"));
    }

    #[test]
    fn every_draft_shows_and_keeps_its_own_selection_by_id() {
        let draft = |id: u64, tag: &str, name: &str| Release {
            id,
            draft: true,
            name: name.into(),
            ..release(tag, "")
        };
        let mut s = tree_sample();
        // Two drafts on one tag, one untagged, and one on a tag that already exists.
        s.releases.splice(
            0..0,
            [
                draft(7, "v3.0.0", "first"),
                draft(8, "v3.0.0", "second"),
                draft(9, "", "web"),
                draft(10, "v2.0.0", "redo"),
            ],
        );
        let mut tab = ReleasesTab::default();
        tab.apply(ReleasesView::Ready(Box::new(s.clone())), R);
        assert_eq!(
            &row_names(&tab)[..4],
            ["draft v3.0.0", "draft v3.0.0", "draft ", "draft v2.0.0"]
        );
        tab.select(1);
        assert_eq!(tab.notes(), Some(Notes::Release(&s.releases[1])));
        // The same list again: the second of two drafts on one tag stays selected.
        tab.apply(ReleasesView::Ready(Box::new(s.clone())), R);
        assert_eq!(tab.cursor(), 1, "a draft is its id, never its tag");
        // A refresh that reorders the drafts keeps the same one by its id.
        s.releases.swap(0, 1);
        tab.apply(ReleasesView::Ready(Box::new(s.clone())), R);
        assert_eq!(tab.cursor(), 0);
        let Some(Notes::Release(kept)) = tab.notes() else { panic!("a draft") };
        assert_eq!((kept.id, kept.name.as_str()), (8, "second"));
    }

    #[test]
    fn a_published_tag_takes_the_cursor_only_in_the_next_landed_refresh() {
        let published = || {
            let mut cut = tree_sample();
            cut.versions.insert(0, version_tag("v3.0.0", "ggggggg777"));
            ReleasesView::Ready(Box::new(cut))
        };
        let land_publish = |tab: &mut ReleasesTab| {
            tab.draft = None;
            tab.select_after = Some("v3.0.0".into());
        };
        // The reader moves before the refresh lands: the cursor stays where they put it.
        let mut tab = ReleasesTab::default();
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        land_publish(&mut tab);
        tab.move_by(1);
        tab.apply(published(), R);
        assert_eq!(tab.selected().map(|c| c.oid.as_str()), Some("nnnnnnn999"));
        // A failed refresh spends it, so a later one moves nothing.
        land_publish(&mut tab);
        tab.apply(ReleasesView::Error("offline".into()), R);
        tab.apply(published(), R);
        assert_eq!(tab.selected().map(|c| c.oid.as_str()), Some("nnnnnnn999"));
        // A refresh without the tag yet spends it too.
        land_publish(&mut tab);
        tab.apply(ReleasesView::Ready(Box::new(tree_sample())), R);
        assert_eq!(tab.select_after, None);
        // Otherwise the first landed refresh selects it.
        land_publish(&mut tab);
        tab.apply(published(), R);
        assert_eq!(tab.cursor(), node_row(&tab, "v3.0.0"));
        // A new repository clears it.
        land_publish(&mut tab);
        tab.apply(ready("other", "main", vec![]), R);
        assert_eq!(tab.select_after, None);
    }

    /// The tab on screen, as the event loop drives it.
    mod on_screen {
        use super::*;
        use crate::app::{App, Focus, Tab};
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::layout::Rect;

        const AREA: Rect = Rect { x: 0, y: 0, width: 140, height: 30 };

        fn app_with(view: ReleasesView) -> App {
            let mut app =
                App::new(std::path::PathBuf::from("."), crate::model::Scope::Uncommitted, None);
            app.set_tab(Tab::Releases).unwrap();
            app.apply_releases(view);
            app.focus = Focus::Files;
            app
        }

        fn sample() -> ReleasesView {
            ReleasesView::Ready(Box::new(tree_sample()))
        }

        fn screen(app: &App) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(AREA.width, AREA.height))
                    .unwrap();
            terminal.draw(|f| crate::ui::render(f, app)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..AREA.height)
                .map(|y| (0..AREA.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn press(app: &mut App, code: KeyCode) {
            let keymap = crate::keymap::Keymap::default();
            crate::handle_key(app, KeyEvent::from(code), AREA, &keymap).unwrap();
        }

        fn click(app: &mut App, column: u16, row: u16) {
            let event = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            };
            let keymap = crate::keymap::Keymap::default();
            crate::handle_mouse(app, event, AREA, &[], &keymap, &crate::export::Clipboard).unwrap();
        }

        /// The screen cell of tree row `row`.
        fn cell_of(app: &App, row: usize) -> (u16, u16) {
            (0..AREA.width)
                .flat_map(|x| (0..AREA.height).map(move |y| (x, y)))
                .find(|&(x, y)| crate::ui::releases_row_at(AREA, app, x, y) == Some(row))
                .expect("the row is on screen")
        }

        fn selected_oid(app: &App) -> Option<&str> {
            app.releases.selected().map(|c| c.oid.as_str())
        }

        /// The read pane's title, from the frame's top border.
        fn read_title(out: &str) -> String {
            out.lines().nth(1).unwrap().split('┐').next().unwrap().to_string()
        }

        #[test]
        fn the_list_shows_unreleased_commits_then_every_version_folded_newest_first() {
            let out = screen(&app_with(sample()));
            let row = |needle: &str| out.lines().position(|l| l.contains(needle)).unwrap();
            assert!(row("fffffff subject fffffff000") < row("nightly subject nnnnnnn999"));
            assert!(row("nightly subject nnnnnnn999") < row("▸ v2.0.0 Second"));
            assert!(row("▸ v2.0.0 Second") < row("▸ v1.0.0"), "a tag-only version is a node");
            assert!(row("▸ v1.0.0") < row("▸ v0.9.0"));
            assert!(!out.contains("bbbbbbb222"), "a folded node shows no commits:\n{out}");
        }

        #[test]
        fn a_commit_shows_its_message_and_a_version_its_notes_or_its_bare_tag() {
            let mut app = app_with(sample());
            let out = screen(&app);
            assert!(read_title(&out).contains(" Commit fffffff "), "{out}");
            assert!(
                out.contains("subject fffffff000") && out.contains("body of fffffff000"),
                "{out}"
            );
            assert!(
                out.contains("Ann · 2026-10-01T12:00:00Z") && out.contains("fffffff000"),
                "{out}"
            );
            press(&mut app, KeyCode::Char('j'));
            assert!(read_title(&screen(&app)).contains(" Commit nnnnnnn "));
            press(&mut app, KeyCode::Char('j'));
            let out = screen(&app);
            assert!(read_title(&out).contains(" Second "), "the release's title:\n{out}");
            assert!(out.contains("Highlights") && !out.contains("## Highlights"), "{out}");
            assert!(out.contains("a bold change") && !out.contains("**bold**"), "{out}");
            press(&mut app, KeyCode::Char('j'));
            let out = screen(&app);
            assert!(read_title(&out).contains(" v1.0.0 "), "a tag-only node shows its tag");
            assert!(out.contains("Tag v1.0.0 has no GitHub release."), "{out}");
            assert!(out.contains("body of ccccccc333"), "and its tagged commit's message:\n{out}");
            press(&mut app, KeyCode::Char('j'));
            let out = screen(&app);
            assert!(read_title(&out).contains(" v0.9.0 "), "an untitled release shows its tag");
            assert!(out.contains("v0.9.0 has no release notes."));
            let empty = ReleasesSnapshot { root: vec![], versions: vec![], ..tree_sample() };
            assert!(
                screen(&app_with(ReleasesView::Ready(Box::new(empty))))
                    .contains("main has no commits.")
            );
        }

        #[test]
        fn a_node_unfolds_by_key_and_by_click_and_its_commits_show_their_messages() {
            let mut app = app_with(sample());
            press(&mut app, KeyCode::Char('j'));
            press(&mut app, KeyCode::Char('j'));
            press(&mut app, KeyCode::Right);
            let out = screen(&app);
            assert!(out.contains("▾ v2.0.0") && out.contains("  loading…"), "{out}");
            assert_eq!(land_all(&mut app.releases), ["v2.0.0"]);
            let out = screen(&app);
            assert!(
                out.contains("▾ v2.0.0 Second  2") && out.contains("bbbbbbb subject bbbbbbb222"),
                "{out}"
            );
            assert!(!out.contains("aaaaaaa subject"), "the tagged commit is the node row:\n{out}");
            press(&mut app, KeyCode::Char('j'));
            assert_eq!(selected_oid(&app), Some("bbbbbbb222"));
            let out = screen(&app);
            assert!(
                read_title(&out).contains(" Commit bbbbbbb ") && out.contains("body of bbbbbbb222")
            );
            press(&mut app, KeyCode::Left);
            assert!(screen(&app).contains("bbbbbbb222"), "`←` on a commit folds nothing");
            press(&mut app, KeyCode::Char('k'));
            press(&mut app, KeyCode::Left);
            assert!(!screen(&app).contains("bbbbbbb222"), "`←` folds the node");
            let (x, y) = cell_of(&app, 4);
            click(&mut app, x, y);
            assert!(screen(&app).contains("▾ v0.9.0"), "a click unfolds a node");
            land_all(&mut app.releases);
            assert!(screen(&app).contains("eeeeeee subject eeeeeee555"));
            let (x, y) = cell_of(&app, 4);
            click(&mut app, x, y);
            assert!(
                screen(&app).contains("▸ v0.9.0 subject ddddddd444  2"),
                "a second click folds it; an untitled release names its tagged commit"
            );
            press(&mut app, KeyCode::Char('k'));
            press(&mut app, KeyCode::Right);
            app.releases.take_node_requests().into_iter().for_each(|key| {
                app.releases.land_node(&key, NodeLoad::Failed("rate limited".into()));
            });
            assert!(screen(&app).contains("couldn't load: rate limited"));
        }

        #[test]
        fn a_tag_only_node_that_gains_a_release_keeps_its_fold_load_and_selection() {
            let mut app = app_with(sample());
            unfold(&mut app.releases, "v1.0.0");
            land_all(&mut app.releases);
            assert!(screen(&app).contains("Tag v1.0.0 has no GitHub release."));
            let mut released = tree_sample();
            let mut first = release("v1.0.0", "Stable at last.");
            first.name = "One".into();
            released.releases.push(first);
            app.apply_releases(ReleasesView::Ready(Box::new(released)));
            assert!(app.releases.take_node_requests().is_empty(), "the same range, still loaded");
            assert_eq!(app.releases.cursor(), node_row(&app.releases, "v1.0.0"), "the same row");
            let out = screen(&app);
            assert!(out.contains("▾ v1.0.0 One  1"), "the same open node, now titled:\n{out}");
            assert!(read_title(&out).contains(" One ") && out.contains("Stable at last."));
        }

        #[test]
        fn a_release_a_pre_release_a_bare_tag_and_a_draft_read_apart_in_every_theme() {
            let mut kinds = tree_sample();
            kinds.releases[1].prerelease = true;
            kinds.releases.insert(0, Release { draft: true, ..release("v3.0.0", "Third") });
            let inks = [
                crate::ui::version_ink(Some(&kinds.releases[1])),
                crate::ui::version_ink(Some(&kinds.releases[2])),
                crate::ui::version_ink(None),
            ];
            assert!(inks[0] != inks[1] && inks[1] != inks[2] && inks[0] != inks[2], "{inks:?}");
            for name in crate::theme::NAMES {
                let mut app = app_with(ReleasesView::Ready(Box::new(kinds.clone())));
                app.set_cli_theme(Some(name.to_string()));
                let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                    AREA.width,
                    AREA.height,
                ))
                .unwrap();
                terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
                let buffer = terminal.backend().buffer().clone();
                let fg_of = |needle: &str| {
                    (0..AREA.height)
                        .find_map(|y| {
                            let line: String =
                                (0..AREA.width).map(|x| buffer[(x, y)].symbol()).collect();
                            let col = line.find(needle)?;
                            let x = line[..col].chars().count() as u16;
                            Some(buffer[(x, y)].fg)
                        })
                        .unwrap_or_else(|| panic!("{needle}:\n{}", screen(&app)))
                };
                let (full, bare, pre) = (fg_of("v2.0.0"), fg_of("v1.0.0"), fg_of("v0.9.0"));
                assert!(full != bare && bare != pre && full != pre, "{name}: three kinds apart");
                let draft = fg_of("✎ v3.0.0");
                assert!(draft != full && draft != pre && draft != bare, "{name}: a draft too");
            }
        }

        #[test]
        fn a_version_cut_at_the_selected_commit_takes_the_selection() {
            let mut app = app_with(sample());
            assert_eq!(selected_oid(&app), Some("fffffff000"));
            let mut cut = tree_sample();
            cut.root.remove(0);
            cut.versions.insert(0, version_tag("v3.0.0", "fffffff000"));
            cut.releases.insert(0, release("v3.0.0", "Third."));
            app.apply_releases(ReleasesView::Ready(Box::new(cut)));
            assert!(app.releases.on_release(), "the new node that took the commit");
            assert!(read_title(&screen(&app)).contains(" v3.0.0 "));
        }

        #[test]
        fn narrowing_shrinks_origin_then_upstream_then_drops_the_notice() {
            let mut long = tree_sample();
            long.repository =
                RepoTarget::new("github.com", "mikebronner", "herdr-reviewr").unwrap();
            long.upstream = Some(Upstream {
                repository: RepoTarget::new("github.com", "persiyanov", "herdr-reviewr").unwrap(),
                newer: Some("v0.47.0".into()),
            });
            let app = app_with(ReleasesView::Ready(Box::new(long)));
            let lines_at = |width: u16| {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 10)).unwrap();
                terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
                let buffer = terminal.backend().buffer().clone();
                let line = |y: u16| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>();
                (line(0), line(1))
            };
            let first_width = |lost: &dyn Fn(&str, &str) -> bool| {
                (40..=200).rev().find(|&w| {
                    let (header, title) = lines_at(w);
                    lost(&header, &title)
                })
            };
            let origin = first_width(&|_, title| !title.contains("mikebronner/herdr-reviewr@main"));
            let upstream = first_width(&|header, _| !header.contains("persiyanov/herdr-reviewr"));
            let notice = first_width(&|header, _| !header.contains("v0.47.0 is newer"));
            let (origin, upstream, notice) = (origin.unwrap(), upstream.unwrap(), notice.unwrap());
            assert!(origin > upstream && upstream > notice, "{origin} > {upstream} > {notice}");
        }

        #[test]
        fn origin_titles_the_list_with_its_branch_and_upstream_sits_in_the_header() {
            let link_at = |app: &App, row: usize, needle: &str| {
                let out = screen(app);
                let line = out.lines().nth(row).unwrap();
                let col = line[..line.find(needle).unwrap()].chars().count() as u16;
                app.painted_link_at(col, row as u16).map(|u| u.to_string())
            };
            let app = app_with(sample());
            let out = screen(&app);
            assert!(out.lines().nth(1).unwrap().contains("o/repo@main"), "{out}");
            assert_eq!(
                link_at(&app, 1, "o/repo@main").as_deref(),
                Some("https://github.com/o/repo")
            );
            let header = out.lines().next().unwrap();
            assert!(!header.contains("upstream") && !header.contains("branch"), "{header}");

            let mut with_upstream = tree_sample();
            with_upstream.upstream = Some(Upstream { repository: target("up"), newer: None });
            let app = app_with(ReleasesView::Ready(Box::new(with_upstream.clone())));
            let header = screen(&app).lines().next().unwrap().to_string();
            assert!(
                header.trim_end().ends_with("upstream: up/repo"),
                "no notice when not newer: {header}"
            );
            assert_eq!(link_at(&app, 0, "up/repo").as_deref(), Some("https://github.com/up/repo"));
            assert_eq!(link_at(&app, 0, "upstream:"), None, "only the name is the link");

            with_upstream.upstream.as_mut().unwrap().newer = Some("v3.0.0".into());
            let app = app_with(ReleasesView::Ready(Box::new(with_upstream)));
            let out = screen(&app);
            let header = out.lines().next().unwrap();
            assert!(header.contains("upstream: up/repo  v3.0.0 is newer"), "{header}");
            assert_eq!(link_at(&app, 0, "up/repo").as_deref(), Some("https://github.com/up/repo"));
            assert!(out.lines().nth(1).unwrap().contains("o/repo@main"), "the list title stays");
            // Narrow, both labels shorten and stay links.
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(64, 12)).unwrap();
            terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let line = |y: u16| (0..64).map(|x| buffer[(x, y)].symbol()).collect::<String>();
            let col = |y: u16, needle: &str| {
                let text = line(y);
                text[..text.find(needle).unwrap_or_else(|| panic!("{needle} in {text}"))]
                    .chars()
                    .count() as u16
            };
            assert!(line(0).contains("upstream: up"), "{}", line(0));
            assert!(app.painted_link_at(col(0, "upstream: ") + 10, 0).is_some(), "{}", line(0));
            assert!(line(1).contains("@main"), "the branch survives: {}", line(1));
            assert!(app.painted_link_at(col(1, "@main"), 1).is_some(), "{}", line(1));
            // Another tab's header is its own.
            let mut app = app;
            press(&mut app, KeyCode::Char('1'));
            assert!(!screen(&app).lines().next().unwrap().contains("upstream"));
        }

        #[test]
        fn unreleased_commits_are_what_the_release_slot_reads() {
            assert!(app_with(sample()).releases.has_unreleased());
            let released = ReleasesSnapshot { root: vec![], ..tree_sample() };
            assert!(!app_with(ReleasesView::Ready(Box::new(released))).releases.has_unreleased());
            assert!(!app_with(ReleasesView::NoCli).releases.has_unreleased());
        }

        fn ctrl(app: &mut App, ch: char) {
            let keymap = crate::keymap::Keymap::default();
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
            crate::handle_key(app, key, AREA, &keymap).unwrap();
        }

        fn footer(out: &str) -> String {
            out.lines().last().unwrap().to_string()
        }

        #[test]
        fn the_create_key_shows_and_acts_only_while_unreleased_commits_wait() {
            let mut app = app_with(sample());
            assert!(footer(&screen(&app)).contains("c new release"), "{}", footer(&screen(&app)));
            app.keys_expanded = true;
            let expanded = screen(&app);
            assert!(
                expanded.contains("c new release") && !expanded.contains("c comment"),
                "{expanded}"
            );
            app.keys_expanded = false;
            let released = ReleasesSnapshot { root: vec![], ..tree_sample() };
            let mut done = app_with(ReleasesView::Ready(Box::new(released)));
            assert!(!screen(&done).contains("new release"), "no hint with nothing to release");
            press(&mut done, KeyCode::Char('c'));
            assert!(done.releases.draft.is_none(), "and the key does nothing");
            let mut gitlab = app_with(ReleasesView::NotGitHub(crate::git::Forge::GitLab));
            press(&mut gitlab, KeyCode::Char('c'));
            assert!(
                gitlab.status.contains("needs GitHub; origin is on GitLab"),
                "{}",
                gitlab.status
            );
            press(&mut app, KeyCode::Char('c'));
            assert!(app.releases.draft.is_some());
        }

        /// Type `text` into the focused field, a key at a time.
        fn type_text(app: &mut App, text: &str) {
            for ch in text.chars() {
                press(app, if ch == '\n' { KeyCode::Enter } else { KeyCode::Char(ch) });
            }
        }

        fn take_publish(app: &mut App) -> Option<crate::release_create::Publish> {
            let requests = app.releases.take_draft_requests();
            requests.into_iter().find_map(|request| match request {
                crate::release_create::Request::Publish { release, .. } => Some(release),
                _ => None,
            })
        }

        #[test]
        fn every_field_types_at_once_and_focus_moves_by_key_and_by_click() {
            use crate::release_create::{Categories, Field};
            let mut app = app_with(sample());
            press(&mut app, KeyCode::Char('c'));
            let requests = app.releases.take_draft_requests();
            assert!(
                matches!(requests.as_slice(), [crate::release_create::Request::Categories { .. }]),
                "the form reads its discussion categories: {requests:?}"
            );
            let out = screen(&app);
            assert!(out.contains("› Tag         v2.0.1▏"), "the tag takes typing at once:\n{out}");
            // `y`, `c` and digits are text in the form, never keys.
            type_text(&mut app, "yc4");
            assert!(screen(&app).contains("v2.0.1yc4▏"));
            assert!(app.releases.draft.is_some(), "`c` in a field opens nothing");
            for _ in 0..3 {
                press(&mut app, KeyCode::Backspace);
            }
            press(&mut app, KeyCode::Tab);
            type_text(&mut app, " (hotfix)");
            assert!(screen(&app).contains("v2.0.1 (hotfix)▏"), "{}", screen(&app));
            press(&mut app, KeyCode::BackTab);
            assert_eq!(app.releases.draft.as_ref().unwrap().field, Field::Tag);
            // A click on a row focuses its field.
            let out = screen(&app);
            let row = out.lines().position(|l| l.contains("Discussion")).unwrap() as u16;
            let col =
                out.lines().nth(row as usize).unwrap().chars().position(|c| c == 'D').unwrap();
            click(&mut app, col as u16, row);
            assert_eq!(app.releases.draft.as_ref().unwrap().field, Field::Discussion);
            // The target is shown, and no field reaches it.
            assert!(out.contains("Target      fffffff subject fffffff000"), "{out}");
            assert!(!out.contains("Notes from") && !out.contains("Assets"), "{out}");
            // The notes edit as markdown across lines, in place; there is no preview to switch to.
            press(&mut app, KeyCode::Tab);
            type_text(&mut app, "## Big\n\n- a **bold** change");
            assert!(screen(&app).contains("- a **bold** change▏"), "{}", screen(&app));
            ctrl(&mut app, 'p');
            let out = screen(&app);
            assert!(out.contains("## Big") && out.contains("- a **bold** change▏"), "{out}");
            assert!(!out.contains("Preview") && !out.contains("ctrl+p"), "{out}");
            type_text(&mut app, "x");
            assert!(app.releases.draft.as_ref().unwrap().notes.text.ends_with("changex"));
            press(&mut app, KeyCode::Backspace);
            // Toggles: space flips, arrows step the discussion.
            let draft = app.releases.draft.as_mut().unwrap();
            draft.categories_read(draft.id, Categories::Ready(vec!["Announcements".into()]));
            draft.focus(Field::Prerelease);
            press(&mut app, KeyCode::Char(' '));
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Right);
            let draft = app.releases.draft.as_ref().unwrap();
            assert!(draft.prerelease && !draft.latest && draft.discussion == Some(0));
            let out = screen(&app);
            assert!(
                out.contains("Pre-release [x]") && out.contains("never for a pre-release"),
                "{out}"
            );
            assert!(out.contains("‹ Announcements ›"), "{out}");
        }

        #[test]
        fn nothing_sends_until_the_review_takes_its_key_and_the_review_lists_everything() {
            let mut app = app_with(sample());
            press(&mut app, KeyCode::Char('c'));
            app.releases.take_draft_requests();
            let draft = app.releases.draft.as_mut().unwrap();
            draft.categories_read(
                draft.id,
                crate::release_create::Categories::Ready(vec!["Announcements".into()]),
            );
            press(&mut app, KeyCode::Backspace);
            type_text(&mut app, "0");
            ctrl(&mut app, 's');
            assert!(
                screen(&app).contains("v2.0.0 already exists on origin."),
                "refused before review"
            );
            press(&mut app, KeyCode::Backspace);
            type_text(&mut app, "1");
            let draft = app.releases.draft.as_mut().unwrap();
            draft.focus(crate::release_create::Field::Prerelease);
            press(&mut app, KeyCode::Char(' '));
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Right);
            ctrl(&mut app, 's');
            let out = screen(&app);
            assert!(out.contains("┌ Save this draft? "), "the review is a titled popup:\n{out}");
            let foot =
                out.lines().position(|l| l.contains("y saves the draft")).expect("the question");
            let listed_last = out.lines().position(|l| l.contains("No notes.")).unwrap();
            assert!(foot > listed_last, "the question sits below what will be sent:\n{out}");
            assert!(out.lines().nth(foot).unwrap().contains("esc goes back"), "{out}");
            for (label, value) in [
                ("Repository", "github.com/o/repo"),
                ("Tag", "v2.0.1"),
                ("Target", "fffffff000"),
                ("Title", "v2.0.1"),
                ("Pre-release", "yes"),
                ("Latest", "set when published"),
                ("Discussion", "Announcements"),
            ] {
                // Within the popup: each line's cells between its borders.
                let listed = out.lines().flat_map(|l| l.split('│')).any(|cell| {
                    cell.trim_start().strip_prefix(label).is_some_and(|rest| {
                        rest.starts_with(' ') && rest.trim_start().starts_with(value)
                    })
                });
                assert!(listed, "the review lists {label} {value}:\n{out}");
            }
            assert!(out.contains("No notes.") && !out.contains("Assets"), "{out}");
            assert!(footer(&out).contains("y save draft"), "{}", footer(&out));
            assert!(take_publish(&mut app).is_none(), "the review sent nothing");
            press(&mut app, KeyCode::Esc);
            assert!(screen(&app).contains("› Discussion"), "back to the form, focus kept");
            ctrl(&mut app, 's');
            press(&mut app, KeyCode::Char('y'));
            let release = take_publish(&mut app).expect("one send");
            assert!(release.draft && release.prerelease && release.latest.is_none());
            assert_eq!(release.discussion.as_deref(), Some("Announcements"));
            assert_eq!(release.target, "fffffff000", "the selected commit, in full");
            press(&mut app, KeyCode::Char('y'));
            assert!(take_publish(&mut app).is_none(), "sending takes no second key");
            assert!(screen(&app).contains("Saving the draft on github.com/o/repo…"));
        }

        #[test]
        fn c_opens_the_form_only_on_an_unreleased_commit() {
            let mut app = app_with(sample());
            for (row, opens) in [(0, true), (1, true), (2, false), (3, false)] {
                app.releases.draft = None;
                app.releases.select(row);
                let hint = footer(&screen(&app)).contains("c new release");
                press(&mut app, KeyCode::Char('c'));
                assert_eq!((app.releases.draft.is_some(), hint), (opens, opens), "row {row}");
            }
            // Inside an unfolded version, a commit is released already.
            app.releases.draft = None;
            unfold(&mut app.releases, "v2.0.0");
            land_all(&mut app.releases);
            app.releases.move_by(1);
            assert_eq!(selected_oid(&app), Some("bbbbbbb222"));
            press(&mut app, KeyCode::Char('c'));
            assert!(app.releases.draft.is_none());
            // A draft row is no commit either.
            let mut drafted = tree_sample();
            drafted.releases.insert(0, Release { draft: true, ..release("v3.0.0", "") });
            let mut app = app_with(ReleasesView::Ready(Box::new(drafted)));
            assert!(!footer(&screen(&app)).contains("new release"), "the draft row leads");
            press(&mut app, KeyCode::Char('c'));
            assert!(app.releases.draft.is_none());
        }

        #[test]
        fn the_notes_start_in_the_value_column_at_every_width() {
            for width in [140u16, 90, 70] {
                let mut app = app_with(sample());
                press(&mut app, KeyCode::Char('c'));
                app.releases.draft.as_mut().unwrap().focus(crate::release_create::Field::Notes);
                type_text(&mut app, &format!("first {}\nsecond", "word ".repeat(30)));
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
                terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
                let buffer = terminal.backend().buffer().clone();
                let line = |y: u16| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>();
                let column = |needle: &str| {
                    (0..30).find_map(|y| {
                        let text = line(y);
                        text.find(needle).map(|at| text[..at].chars().count())
                    })
                };
                let value = column("v2.0.1").expect("the tag's value");
                assert_eq!(column("first word"), Some(value), "{width}: the first notes line");
                assert_eq!(column("second▏"), Some(value), "{width}: the caret's line");
                // A wrapped line holds the column too.
                let wrapped = (0..30).map(line).filter(|l| l.contains("word")).count();
                assert!(wrapped > 1, "{width}: the long line wraps");
                for y in 0..30 {
                    let text = line(y);
                    if let Some(at) = text.find("word") {
                        let col = text[..at].chars().count();
                        assert!(col >= value, "{width}: a wrapped line left the column: {text}");
                    }
                }
            }
        }

        #[test]
        fn the_review_scrolls_and_ignores_clicks_and_paste() {
            let mut app = app_with(sample());
            press(&mut app, KeyCode::Char('c'));
            app.releases.draft.as_mut().unwrap().focus(crate::release_create::Field::Notes);
            for i in 0..60 {
                type_text(&mut app, &format!("- line {i}\n"));
            }
            ctrl(&mut app, 'o');
            let out = screen(&app);
            assert!(
                out.contains("┌ Publish this release? ") && out.contains("y publishes"),
                "{out}"
            );
            assert!(out.contains("Repository  github.com/o/repo") && !out.contains("line 59"));
            for _ in 0..4 {
                press(&mut app, KeyCode::PageDown);
            }
            assert!(screen(&app).contains("line 59"), "the review scrolls to the end");
            // A click on the form behind the popup and a paste change nothing.
            let (x, y) = (3, 3);
            click(&mut app, x, y);
            app.input_paste("pasted");
            let draft = app.releases.draft.as_ref().unwrap();
            assert_eq!(draft.field, crate::release_create::Field::Notes);
            assert!(!draft.notes.text.contains("pasted"));
            assert!(matches!(draft.stage, crate::release_create::Stage::Review(_)));
            assert!(take_publish(&mut app).is_none(), "nothing sent");
            press(&mut app, KeyCode::Esc);
            assert!(!screen(&app).contains("Publish this release?"), "esc closes the review");
        }

        #[test]
        fn cancelling_the_form_sends_nothing() {
            let mut app = app_with(sample());
            press(&mut app, KeyCode::Char('c'));
            ctrl(&mut app, 'o');
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);
            assert!(app.releases.draft.is_none(), "esc from the form cancels it");
            press(&mut app, KeyCode::Char('y'));
            assert!(take_publish(&mut app).is_none(), "nothing was ever sent");
            assert!(!screen(&app).contains("New release"));
        }

        #[test]
        fn generated_notes_fill_the_form_without_clobbering_edits_and_a_failure_says_why() {
            use crate::release_create::Request;
            let mut app = app_with(sample());
            press(&mut app, KeyCode::Char('c'));
            app.releases.take_draft_requests();
            ctrl(&mut app, 'g');
            let requests = app.releases.take_draft_requests();
            let [Request::Generate { id, seq, args, .. }] = requests.as_slice() else {
                panic!("one generate: {requests:?}")
            };
            assert!(args.contains(&"tag_name=v2.0.1".to_string()));
            assert!(args.contains(&"target_commitish=fffffff000".to_string()));
            assert!(args.contains(&"previous_tag_name=v2.0.0".to_string()));
            assert!(screen(&app).contains("generating notes…"));
            ctrl(&mut app, 'e');
            assert!(!app.releases.take_notes_edit(), "no editor while notes are on their way");
            app.releases.land_generated(*id, *seq, Ok("## What's Changed\n\n* a **fix**".into()));
            let out = screen(&app);
            assert!(out.contains("## What's Changed"), "the notes land as markdown text:\n{out}");
            ctrl(&mut app, 'o');
            press(&mut app, KeyCode::Char('y'));
            let id = app.releases.draft.as_ref().unwrap().id;
            app.releases.land_published(id, "v2.0.1".into(), Err("HTTP 403: Forbidden".into()));
            let out = screen(&app);
            assert!(out.contains("HTTP 403: Forbidden") && out.contains("v2.0.1▏"), "{out}");
        }

        #[test]
        fn a_saved_draft_and_a_published_release_land_in_the_tree_selected() {
            let mut app = app_with(sample());
            // Cut at the second unreleased commit, so the new draft must take the cursor.
            app.releases.select(1);
            press(&mut app, KeyCode::Char('c'));
            assert_eq!(app.releases.draft.as_ref().unwrap().target, "nnnnnnn999");
            ctrl(&mut app, 's');
            press(&mut app, KeyCode::Char('y'));
            app.releases.take_draft_requests();
            let id = app.releases.draft.as_ref().unwrap().id;
            let url = "https://github.com/o/repo/releases/tag/untagged-1";
            app.releases.land_published(id, "v2.0.1".into(), Ok(url.into()));
            assert_eq!(app.releases.take_pending(), Some(RefreshKind::Forced));
            // GitHub lists the draft with no tag behind it yet.
            let mut saved = tree_sample();
            saved.releases.insert(0, Release { draft: true, ..release("v2.0.1", "Patch.") });
            app.apply_releases(ReleasesView::Ready(Box::new(saved.clone())));
            assert_eq!(row_names(&app.releases)[0], "draft v2.0.1", "drafts lead the tree");
            assert_eq!(app.releases.cursor(), 0, "the saved draft is selected");
            let out = screen(&app);
            assert!(out.contains("✎ v2.0.1  draft"), "{out}");
            assert!(read_title(&out).contains(" v2.0.1 · draft "), "{out}");
            assert!(out.contains(&format!("Published: {url}")) && out.contains("Patch."), "{out}");
            // Published later, the draft becomes its version's node, the selection with it.
            let mut published = saved;
            published.releases[0].draft = false;
            published.releases[0].prerelease = true;
            published.root.clear();
            published.versions.insert(0, version_tag("v2.0.1", "fffffff000"));
            app.apply_releases(ReleasesView::Ready(Box::new(published)));
            assert!(app.releases.on_release());
            let out = screen(&app);
            assert!(read_title(&out).contains(" v2.0.1 · pre-release "), "{out}");
        }
    }
}
