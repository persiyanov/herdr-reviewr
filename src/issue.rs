//! GitHub Issues read path for the read-only `Issue` tab.
//!
//! v1 is GitHub-only via `gh issue list`. No comment write, no edit, no close — the tab mirrors
//! open (default) / closed / all issues and opens the selected one in a browser. List filters
//! are local presentation over the same repository identity the PR tab already resolves
//! (`forge::fetch_input`).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::forge::{self, PrFetchInput};
use crate::git::Forge;

/// How many issues one fetch returns. Matching the PR surface cap keeps list size predictable.
const ISSUE_CAP: usize = 100;

/// Which issue set the navigator lists. Default is every open issue in the repository.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
}

/// Lifecycle shown on a row and in the read pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueState {
    Open,
    Closed,
}

/// A successful list fetch for one filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueSnapshot {
    pub filter: IssueFilter,
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
    /// A list (possibly empty) for the active filter.
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

/// Read issues for one already-derived forge input and filter.
#[must_use]
pub fn fetch(
    repo: &Path,
    input: &PrFetchInput,
    filter: IssueFilter,
    cancelled: &AtomicBool,
) -> IssueView {
    match fetch_inner(repo, input, filter, cancelled) {
        Ok(view) => view,
        Err(error) => error.into(),
    }
}

fn fetch_inner(
    repo: &Path,
    input: &PrFetchInput,
    filter: IssueFilter,
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
    let state = filter.gh_state();
    let host = target.host();
    let json = gh_issue_list(repo, host, &repo_slug, state, cancelled)?;
    let issues = parse_issues(&json)?;
    let truncated = issues.len() >= ISSUE_CAP;
    Ok(IssueView::List(IssueSnapshot { filter, issues, truncated }))
}

fn gh_issue_list(
    repo: &Path,
    host: &str,
    repo_slug: &str,
    state: &str,
    cancelled: &AtomicBool,
) -> Result<String, IssueError> {
    let mut cmd = Command::new("gh");
    cmd.current_dir(repo).args([
        "issue",
        "list",
        "--hostname",
        host,
        "--repo",
        repo_slug,
        "--state",
        state,
        "--limit",
        &ISSUE_CAP.to_string(),
        "--json",
        "number,title,body,state,author,updatedAt,url,labels",
    ]);
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
        issues.push(Issue { number, title, body, state, author, updated_at, url, labels });
    }
    Ok(issues)
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
            "labels": [{"name": "bug"}, {"name": "ui"}]
          },
          {
            "number": 7,
            "title": "Done",
            "body": "",
            "state": "CLOSED",
            "author": {"login": "bob"},
            "updatedAt": "2026-07-01T12:00:00Z",
            "url": "https://github.com/o/r/issues/7",
            "labels": []
          }
        ]"#;
        let issues = parse_issues(json).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 42);
        assert_eq!(issues[0].title, "Fix the pane");
        assert_eq!(issues[0].state, IssueState::Open);
        assert_eq!(issues[0].author, "alice");
        assert_eq!(issues[0].labels, vec!["bug", "ui"]);
        assert_eq!(issues[1].state, IssueState::Closed);
    }

}
