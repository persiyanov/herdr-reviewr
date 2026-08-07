//! GitHub Issues read path for the read-only `Issue` tab.
//!
//! v1 is GitHub-only via `gh issue list`. No comment write, no edit, no close — the tab mirrors
//! open (default) / closed / all issues, optional assignee and priority-label filters, and opens
//! the selected one in a browser. Filters compose into one list call over the same repository
//! identity the PR tab already resolves (`forge::fetch_input`). Successful lists are cached by
//! query so tab re-entry and filter cycling do not hammer `gh` (specs/issue-tab.md).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use crate::forge::{self, PrFetchInput};
use crate::git::Forge;

/// How many issues one fetch returns. Matching the PR surface cap keeps list size predictable.
const ISSUE_CAP: usize = 100;

/// Freshness window for a cached list result. Matches the ambient poll so re-entry within the
/// window paints memory only (specs/issue-tab.md).
pub const ISSUE_CACHE_TTL: Duration = Duration::from_mins(1);

/// Which issue set the navigator lists. Default is every open issue in the repository.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IssueFilter {
    #[default]
    Open,
    Closed,
    All,
}

impl IssueFilter {
    /// Cycle open → closed → all → open for the filter key.
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::All,
            Self::All => Self::Open,
        }
    }

    /// Header / footer chip label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }

    /// `gh issue list --state` value.
    #[must_use]
    pub fn gh_state(self) -> &'static str {
        self.label()
    }
}

/// Assignee dimension. Default lists every assignee; `Mine` is `gh --assignee @me`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IssueAssignee {
    #[default]
    All,
    Mine,
}

impl IssueAssignee {
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Mine,
            Self::Mine => Self::All,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Mine => "mine",
        }
    }
}

/// Priority-label dimension. Cycles any → p0 → p1 → p2; each non-`Any` value is a single
/// `gh --label` name (case as GitHub stores it; we pass lowercase `pN`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IssuePriority {
    #[default]
    Any,
    P0,
    P1,
    P2,
}

impl IssuePriority {
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Any => Self::P0,
            Self::P0 => Self::P1,
            Self::P1 => Self::P2,
            Self::P2 => Self::Any,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::P0 => "p0",
            Self::P1 => "p1",
            Self::P2 => "p2",
        }
    }

    /// `Some` label name for `gh --label`, or `None` when unfiltered.
    #[must_use]
    pub fn gh_label(self) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::P0 => Some("p0"),
            Self::P1 => Some("p1"),
            Self::P2 => Some("p2"),
        }
    }
}

/// Full list query: state × assignee × priority. Cache key and `gh` argument source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IssueQuery {
    pub state: IssueFilter,
    pub assignee: IssueAssignee,
    pub priority: IssuePriority,
}

impl IssueQuery {
    /// Compact summary for empty states: `open`, `open · mine`, `closed · p1`, …
    #[must_use]
    pub fn summary(self) -> String {
        let mut parts = vec![self.state.label().to_string()];
        if self.assignee != IssueAssignee::All {
            parts.push(self.assignee.label().to_string());
        }
        if self.priority != IssuePriority::Any {
            parts.push(self.priority.label().to_string());
        }
        parts.join(" · ")
    }

    /// Navigator title suffix after `Issues · `.
    #[must_use]
    pub fn nav_title(self) -> String {
        self.summary()
    }
}

/// One plain comment on an issue (from `gh issue list --json comments`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueComment {
    pub author: String,
    pub author_is_bot: bool,
    pub body: String,
    /// ISO-8601 `…Z` post time — newest-first sort key.
    pub created_at: String,
    pub url: String,
}

/// One GitHub issue row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: IssueState,
    pub author: String,
    pub updated_at: String,
    pub url: String,
    pub labels: Vec<String>,
    /// Parent issue number when this is a GitHub sub-issue (`parent.number`).
    pub parent_number: Option<u64>,
    /// Closed sub-issues under this issue (`subIssuesSummary.completed`).
    pub sub_completed: u32,
    /// Total sub-issues under this issue (`subIssuesSummary.total`).
    pub sub_total: u32,
    /// Issue-thread comments, newest first (specs/issue-tab.md).
    pub comments: Vec<IssueComment>,
}

impl Issue {
    /// Navigator indent depth: one level when the parent is also present in the list.
    #[must_use]
    pub fn tree_depth(&self, present: &std::collections::HashSet<u64>) -> usize {
        match self.parent_number {
            Some(parent) if present.contains(&parent) => 1,
            _ => 0,
        }
    }
}

/// Lifecycle shown on a row and in the read pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueState {
    Open,
    Closed,
}

/// A successful list fetch for one query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueSnapshot {
    pub query: IssueQuery,
    pub issues: Vec<Issue>,
    /// `true` when the fetch hit the cap — more issues may exist on GitHub.
    pub truncated: bool,
}

/// What the `Issue` tab shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueView {
    /// Work pending; under the loading-indicator delay.
    Pending,
    /// Work crossed the loading-indicator delay without a snapshot.
    Loading,
    /// A list (possibly empty) for the active query.
    List(IssueSnapshot),
    /// `gh` is not on `PATH`.
    NoCli,
    /// `gh` is installed but not authenticated for this host.
    NotAuthed(String),
    /// No recognized forge remote.
    NeedsForgeRemote,
    /// Remote host is not a supported forge host.
    UnsupportedHost(String),
    /// Remote host is supported but path is not a repository.
    MalformedOrigin(String),
    /// Repository is not GitHub — v1 Issues is GitHub-only.
    NeedsGitHub,
    /// Local Git read failed before the fetch.
    GitError(String),
    /// Transient CLI failure; the app freezes the last good view when one exists.
    Error(String),
}

impl IssueView {
    /// Retryable failure message for the notice strip / empty pane.
    pub fn retry_remedy(&self, refresh: crate::keymap::Key) -> Option<String> {
        match self {
            Self::NoCli => {
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

/// Read issues for one already-derived forge input and query.
#[must_use]
pub fn fetch(
    repo: &Path,
    input: &PrFetchInput,
    query: IssueQuery,
    cancelled: &AtomicBool,
) -> IssueView {
    match fetch_inner(repo, input, query, cancelled) {
        Ok(view) => view,
        Err(error) => error.into(),
    }
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    query: IssueQuery,
    cancelled: &AtomicBool,
) -> Result<IssueView, IssueError> {
    let target = match &input.repository {
        crate::git::RepositoryIdentity::Repository(target) => target,
        crate::git::RepositoryIdentity::Missing | crate::git::RepositoryIdentity::Hostless => {
            return Ok(IssueView::NeedsForgeRemote);
        }
        crate::git::RepositoryIdentity::Unsupported(host) => {
            return Ok(IssueView::UnsupportedHost(host.clone()));
        }
        crate::git::RepositoryIdentity::Malformed(host) => {
            return Ok(IssueView::MalformedOrigin(host.clone()));
        }
    };
    if target.forge() != Forge::GitHub {
        return Ok(IssueView::NeedsGitHub);
    }

    let repo_slug = format!("{}/{}", target.owner(), target.name());
    let host = target.host();
    let json = gh_issue_list(repo, host, &repo_slug, query, cancelled)?;
    let mut issues = parse_issues(&json)?;
    // Nest children under parents that landed in the same fetch (specs/issue-tab.md).
    issues = order_as_tree(issues);
    let truncated = issues.len() >= ISSUE_CAP;
    Ok(IssueView::List(IssueSnapshot { query, issues, truncated }))
}

fn gh_issue_list(
    repo: &Path,
    host: &str,
    repo_slug: &str,
    query: IssueQuery,
    cancelled: &AtomicBool,
) -> Result<String, IssueError> {
    // `gh issue list` has no `--hostname` flag. Pin the host with the `[HOST/]OWNER/REPO`
    // form of `--repo` (enterprise) or plain `OWNER/REPO` (github.com).
    let repo_arg = if host.eq_ignore_ascii_case("github.com") {
        repo_slug.to_string()
    } else {
        format!("{host}/{repo_slug}")
    };
    let mut cmd = Command::new("gh");
    cmd.current_dir(repo).args([
        "issue",
        "list",
        "--repo",
        &repo_arg,
        "--state",
        query.state.gh_state(),
        "--limit",
        &ISSUE_CAP.to_string(),
        "--json",
        "number,title,body,state,author,updatedAt,url,labels,parent,subIssuesSummary,comments",
    ]);
    if query.assignee == IssueAssignee::Mine {
        cmd.args(["--assignee", "@me"]);
    }
    if let Some(label) = query.priority.gh_label() {
        cmd.args(["--label", label]);
    }
    forge::run_provider(
        &mut cmd,
        cancelled,
        IssueError::NoCli,
        |stderr| classify_failure(stderr, host),
        IssueError::Other,
    )
}

fn classify_failure(stderr: &str, host: &str) -> IssueError {
    let s = stderr.to_lowercase();
    if s.contains("not logged")
        || s.contains("authentication")
        || s.contains("gh auth login")
        || s.contains("bad credentials")
        || s.contains("http 401")
        || s.contains("status 401")
    {
        IssueError::NotAuthed(host.to_owned())
    } else {
        IssueError::Other(stderr.trim().to_string())
    }
}

fn parse_issues(json: &str) -> Result<Vec<Issue>, IssueError> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| IssueError::Other(format!("invalid issue list JSON: {error}")))?;
    let rows = value
        .as_array()
        .ok_or_else(|| IssueError::Other("issue list JSON is not an array".into()))?;
    let mut issues = Vec::with_capacity(rows.len());
    for row in rows {
        let number = row["number"].as_u64().unwrap_or(0);
        if number == 0 {
            continue;
        }
        let title = row["title"].as_str().unwrap_or("").to_string();
        let body = row["body"].as_str().unwrap_or("").to_string();
        let state = match row["state"].as_str().unwrap_or("OPEN").to_ascii_uppercase().as_str() {
            "CLOSED" => IssueState::Closed,
            _ => IssueState::Open,
        };
        let author = row["author"]["login"].as_str().unwrap_or("").to_string();
        let updated_at = row["updatedAt"].as_str().unwrap_or("").to_string();
        let url = row["url"].as_str().unwrap_or("").to_string();
        let labels = row["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label["name"].as_str().map(str::to_string))
            .collect();
        // `parent` is null or `{ "number": N, ... }`.
        let parent_number = row.get("parent").and_then(|p| {
            if p.is_null() {
                None
            } else {
                p.get("number").and_then(Value::as_u64).filter(|&n| n > 0)
            }
        });
        let summary = row.get("subIssuesSummary");
        let sub_completed = summary
            .and_then(|s| s.get("completed"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let sub_total = summary
            .and_then(|s| s.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let mut comments = parse_comments(row.get("comments"));
        // Newest first — matches the PR tab's comment surface (specs/issue-tab.md).
        comments.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        issues.push(Issue {
            number,
            title,
            body,
            state,
            author,
            updated_at,
            url,
            labels,
            parent_number,
            sub_completed,
            sub_total,
            comments,
        });
    }
    Ok(issues)
}

fn parse_comments(value: Option<&Value>) -> Vec<IssueComment> {
    let Some(rows) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let author = row["author"]["login"].as_str().unwrap_or("").to_string();
        if author.is_empty() && row["body"].as_str().unwrap_or("").is_empty() {
            continue;
        }
        out.push(IssueComment {
            author_is_bot: forge::is_named_bot(&author),
            author,
            body: row["body"].as_str().unwrap_or("").to_string(),
            created_at: row["createdAt"].as_str().unwrap_or("").to_string(),
            url: row["url"].as_str().unwrap_or("").to_string(),
        });
    }
    out
}

/// Put each child immediately under its parent when both are in the list. One nesting level.
/// Children whose parent is missing (e.g. closed parent while listing open) stay top-level.
/// Relative order among roots and among siblings follows the input order.
#[must_use]
pub fn order_as_tree(issues: Vec<Issue>) -> Vec<Issue> {
    use std::collections::{HashMap, HashSet};
    let present: HashSet<u64> = issues.iter().map(|i| i.number).collect();
    let mut children: HashMap<u64, Vec<Issue>> = HashMap::new();
    let mut roots = Vec::new();
    for issue in issues {
        match issue.parent_number {
            Some(parent) if present.contains(&parent) => {
                children.entry(parent).or_default().push(issue);
            }
            _ => roots.push(issue),
        }
    }
    let mut out = Vec::with_capacity(roots.len() + children.values().map(Vec::len).sum::<usize>());
    for root in roots {
        let number = root.number;
        out.push(root);
        if let Some(kids) = children.remove(&number) {
            out.extend(kids);
        }
    }
    // Orphans of parents that were themselves nested (multi-level) or removed — list flat.
    for (_, kids) in children {
        out.extend(kids);
    }
    out
}

#[derive(Debug)]
enum IssueError {
    NoCli,
    NotAuthed(String),
    Other(String),
}

impl From<IssueError> for IssueView {
    fn from(error: IssueError) -> Self {
        match error {
            IssueError::NoCli => Self::NoCli,
            IssueError::NotAuthed(host) => Self::NotAuthed(host),
            IssueError::Other(message) => Self::Error(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_cycles_open_closed_all() {
        assert_eq!(IssueFilter::Open.cycle(), IssueFilter::Closed);
        assert_eq!(IssueFilter::Closed.cycle(), IssueFilter::All);
        assert_eq!(IssueFilter::All.cycle(), IssueFilter::Open);
        assert_eq!(IssueFilter::Open.gh_state(), "open");
    }

    #[test]
    fn assignee_and_priority_cycle() {
        assert_eq!(IssueAssignee::All.cycle(), IssueAssignee::Mine);
        assert_eq!(IssueAssignee::Mine.cycle(), IssueAssignee::All);
        assert_eq!(IssuePriority::Any.cycle(), IssuePriority::P0);
        assert_eq!(IssuePriority::P0.cycle(), IssuePriority::P1);
        assert_eq!(IssuePriority::P1.cycle(), IssuePriority::P2);
        assert_eq!(IssuePriority::P2.cycle(), IssuePriority::Any);
        assert_eq!(IssuePriority::P1.gh_label(), Some("p1"));
        assert_eq!(IssuePriority::Any.gh_label(), None);
    }

    #[test]
    fn query_summary_omits_default_assignee_and_priority() {
        let q = IssueQuery::default();
        assert_eq!(q.summary(), "open");
        let mine = IssueQuery { assignee: IssueAssignee::Mine, ..IssueQuery::default() };
        assert_eq!(mine.summary(), "open · mine");
        let p0 = IssueQuery { priority: IssuePriority::P0, ..IssueQuery::default() };
        assert_eq!(p0.summary(), "open · p0");
    }

    #[test]
    fn parse_issues_reads_gh_json_shape() {
        let json = r#"[
          {
            "number": 42,
            "title": "Fix the pane",
            "body": "Details here",
            "state": "OPEN",
            "author": {"login": "alice"},
            "updatedAt": "2026-08-01T12:00:00Z",
            "url": "https://github.com/o/r/issues/42",
            "labels": [{"name": "bug"}, {"name": "ui"}],
            "parent": null,
            "subIssuesSummary": {"completed": 1, "percentCompleted": 50, "total": 2},
            "comments": [
              {
                "author": {"login": "carol"},
                "body": "older note",
                "createdAt": "2026-07-01T12:00:00Z",
                "url": "https://github.com/o/r/issues/42#issuecomment-1"
              },
              {
                "author": {"login": "dave"},
                "body": "newer note",
                "createdAt": "2026-08-01T12:00:00Z",
                "url": "https://github.com/o/r/issues/42#issuecomment-2"
              }
            ]
          },
          {
            "number": 7,
            "title": "Done",
            "body": "",
            "state": "CLOSED",
            "author": {"login": "bob"},
            "updatedAt": "2026-07-01T12:00:00Z",
            "url": "https://github.com/o/r/issues/7",
            "labels": [],
            "parent": {"number": 42, "title": "Fix the pane"},
            "subIssuesSummary": {"completed": 0, "percentCompleted": 0, "total": 0},
            "comments": []
          }
        ]"#;
        let issues = parse_issues(json).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 42);
        assert_eq!(issues[0].title, "Fix the pane");
        assert_eq!(issues[0].state, IssueState::Open);
        assert_eq!(issues[0].author, "alice");
        assert_eq!(issues[0].labels, vec!["bug", "ui"]);
        assert_eq!(issues[0].parent_number, None);
        assert_eq!(issues[0].sub_completed, 1);
        assert_eq!(issues[0].sub_total, 2);
        assert_eq!(issues[0].comments.len(), 2);
        assert_eq!(issues[0].comments[0].author, "dave", "comments are newest-first");
        assert_eq!(issues[0].comments[1].author, "carol");
        assert_eq!(issues[1].state, IssueState::Closed);
        assert_eq!(issues[1].parent_number, Some(42));
        assert!(issues[1].comments.is_empty());
    }

    fn bare(number: u64, parent: Option<u64>) -> Issue {
        Issue {
            number,
            title: format!("#{number}"),
            body: String::new(),
            state: IssueState::Open,
            author: "a".into(),
            updated_at: String::new(),
            url: String::new(),
            labels: vec![],
            parent_number: parent,
            sub_completed: 0,
            sub_total: 0,
            comments: vec![],
        }
    }

    #[test]
    fn order_as_tree_nests_children_under_present_parents() {
        // Input: child, parent, other — output: parent, child, other.
        let ordered = order_as_tree(vec![bare(2, Some(1)), bare(1, None), bare(3, None)]);
        assert_eq!(
            ordered.iter().map(|i| i.number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn order_as_tree_leaves_orphan_children_top_level() {
        // Parent #99 is not in the list (e.g. closed while listing open).
        let ordered = order_as_tree(vec![bare(2, Some(99)), bare(1, None)]);
        assert_eq!(
            ordered.iter().map(|i| i.number).collect::<Vec<_>>(),
            vec![2, 1],
            "orphan child keeps its place among roots"
        );
    }
}
