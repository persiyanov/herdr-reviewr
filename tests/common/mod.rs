//! A real on-disk git repo for integration tests. Every helper shells out to the
//! actual `git` binary, so tests exercise the same surface the app does at runtime.
//!
//! `dead_code`/`unreachable_pub` are allowed because each test binary includes this
//! module and uses only the subset of helpers it needs.
#![allow(dead_code, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::process::Command;

use herdr_reviewr::app::App;
use herdr_reviewr::model::Scope;
use tempfile::TempDir;

pub struct Repo {
    dir: TempDir,
}

impl Repo {
    /// A fresh repo on branch `main` with an identity configured.
    pub fn init() -> Self {
        let repo = Self { dir: TempDir::new().expect("tempdir") };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn path_buf(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Run `git -C <repo> <args>`, asserting success, returning stdout.
    pub fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@herdr.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@herdr.test")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, contents).expect("write");
    }

    pub fn remove(&self, rel: &str) {
        std::fs::remove_file(self.path().join(rel)).expect("remove");
    }

    /// Stage everything and commit.
    pub fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }
}

pub fn app_on(repo: &Repo) -> App {
    let mut app = App::new(repo.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app
}

pub fn typed(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.input_push(ch);
    }
}

/// A minimal open-PR snapshot. Tests override only the fields they exercise:
/// `PrSnapshot { comments, ..common::pr_snapshot() }` — so a new snapshot field
/// touches this one literal instead of every test.
pub fn pr_snapshot() -> herdr_reviewr::forge::PrSnapshot {
    use herdr_reviewr::forge::{Merge, PrSnapshot, PrState, Sync};
    PrSnapshot {
        number: 1,
        title: "t".into(),
        body: String::new(),
        url: "u".into(),
        state: PrState::Open,
        is_draft: false,
        head_ref: "feature".into(),
        head_is_fork: false,
        base_ref: "main".into(),
        merge: Merge::Clean,
        sync: Sync::InSync,
        checks: Vec::new(),
        comments: Vec::new(),
        truncated: false,
    }
}

/// A minimal PR conversation comment. Tests override the fields they exercise:
/// `Comment { body: "...".into(), ..common::comment() }`.
pub fn comment() -> herdr_reviewr::forge::Comment {
    use herdr_reviewr::forge::{Comment, CommentKind};
    Comment {
        kind: CommentKind::Comment,
        author: "ann".into(),
        author_is_bot: false,
        anchor: "comment".into(),
        body: "b".into(),
        snippet: None,
        created_at: "2026-06-27T10:00:00Z".into(),
        is_resolved: false,
        is_outdated: false,
        reply_count: 0,
    }
}

/// Switch to `tab` and service the deferred reload the switch schedules, so assertions run
/// against the freshly reloaded state — the same sequence the event loop performs.
pub fn enter_tab(app: &mut App, tab: herdr_reviewr::app::Tab) {
    app.set_tab(tab).unwrap();
    land_world(app);
}

/// Land the queued world refresh synchronously, as the worker's completion would.
pub fn land_world(app: &mut App) {
    let snapshot = herdr_reviewr::world::build(&app.world_input()).unwrap();
    app.reconcile_world(snapshot);
    app.world_request = None;
}
