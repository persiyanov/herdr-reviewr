//! A real git repo for integration tests, and the fake herdr; each binary uses a subset.
#![allow(dead_code, unreachable_pub)]

mod fixture;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use herdr_reviewr::app::App;
use herdr_reviewr::model::Scope;
use tempfile::TempDir;

#[allow(unused_imports)]
pub use fixture::fixture;

/// The fake herdr, built here once per test process: `cargo test --test` builds no examples.
pub fn fake_herdr() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let output = Command::new(env!("CARGO"))
            .args(["build", "--example", "fake_herdr", "--message-format=json"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|message| message["target"]["name"] == "fake_herdr")
            .find_map(|message| message["executable"].as_str().map(PathBuf::from))
            .expect("cargo reports the fake herdr's executable")
    })
}

/// Every herdr call the fake in `dir` logged, one per line. Empty when herdr was never called.
pub fn herdr_calls(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("herdr.log")).unwrap_or_default()
}

/// herdr's error envelope for `code`, as a failed CLI call writes it to stderr.
pub fn herdr_error(code: &str) -> String {
    serde_json::json!({"error": {"code": code, "message": "boom"}, "id": "cli:request"}).to_string()
}

pub struct Repo {
    dir: TempDir,
}

impl Repo {
    /// A fresh repo on branch `main` with an identity configured.
    pub fn init() -> Self {
        let repo = Self { dir: TempDir::new().expect("tempdir") };
        repo.git(&["init", "-q", "-b", "main"]);
        // Pin `init.defaultBranch` so the machine's global config never steers a test.
        repo.git(&["config", "init.defaultBranch", "no-such-default"]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn path_buf(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// [`Self::git`] with extra environment, such as a pinned committer date.
    pub fn git_env(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let out = Command::new("git")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@herdr.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@herdr.test")
            .envs(env.iter().copied())
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

    /// Run `git -C <repo> <args>`, asserting success, returning stdout.
    pub fn git(&self, args: &[&str]) -> String {
        self.git_env(args, &[])
    }

    /// Fake `origin/<name>` at `rev` as the remote default, with no real remote.
    pub fn set_origin_default(&self, name: &str, rev: &str) {
        let oid = self.git(&["rev-parse", rev]).trim().to_string();
        self.git(&["update-ref", &format!("refs/remotes/origin/{name}"), &oid]);
        self.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            &format!("refs/remotes/origin/{name}"),
        ]);
    }

    /// Record `content` as a blob and point `git_ref` at it, bypassing `write_base_pick`.
    fn plant_blob(&self, git_ref: &str, content: &str) {
        let path = self.path().join("plant-blob");
        std::fs::write(&path, content).unwrap();
        let blob = self.git(&["hash-object", "-w", path.to_str().unwrap()]).trim().to_string();
        self.git(&["update-ref", git_ref, &blob]);
        std::fs::remove_file(&path).unwrap();
    }

    /// Write `content` to the pick ref verbatim, as only a foreign writer could.
    pub fn write_raw_base_pick(&self, content: &str) {
        self.plant_blob("refs/worktree/reviewr/base-pick", content);
    }

    /// A leftover clone-wide pick from before the worktree-private cutover.
    pub fn plant_legacy_base_pick(&self, content: &str) {
        self.plant_blob("refs/reviewr/base-pick", content);
    }

    /// The path-hashed last-turn ref an old binary left, before refs went worktree-private.
    pub fn plant_legacy_turn_base(&self, sha: &str) {
        let key = legacy_worktree_key(self.path());
        self.git(&["update-ref", &format!("refs/reviewr/turn-base/{key}"), sha]);
    }

    /// A linked worktree of this clone on a new branch. Lives as long as the returned value.
    pub fn add_worktree(&self, branch: &str) -> LinkedWorktree {
        let keep = TempDir::new().expect("tempdir");
        let path = keep.path().join("wt");
        self.git(&["worktree", "add", "-q", "-b", branch, path.to_str().unwrap()]);
        LinkedWorktree { _keep: keep, path }
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

/// A linked worktree created by [`Repo::add_worktree`]. The directory is deleted when dropped.
pub struct LinkedWorktree {
    _keep: TempDir,
    path: PathBuf,
}

impl LinkedWorktree {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn legacy_worktree_key(repo: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in repo.to_string_lossy().bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
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

/// A minimal open-PR snapshot to override per test: `PrSnapshot { .., ..pr_snapshot() }`.
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
        head_oid: String::new(),
        base_ref: "main".into(),
        merge: Merge::Clean,
        sync: Sync::InSync,
        checks: Vec::new(),
        comments: Vec::new(),
        comments_truncated: false,
        checks_truncated: false,
    }
}

/// A minimal PR comment to override per test.
pub fn comment() -> herdr_reviewr::forge::Comment {
    use herdr_reviewr::forge::{Comment, CommentKind};
    Comment {
        kind: CommentKind::Comment,
        author: "ann".into(),
        author_is_bot: false,
        anchor: "comment".into(),
        place: None,
        body: "b".into(),
        snippet: None,
        created_at: "2026-06-27T10:00:00Z".into(),
        is_resolved: false,
        is_outdated: false,
        replies: Vec::new(),
        draft_id: None,
    }
}

/// Switch to `tab` and run its deferred reload, as the event loop does.
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

/// A pane over `repo` whose config sets `markdown_view = "rendered"`: markdown opens rendered.
pub fn app_on_rendered(repo: &Repo) -> App {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "markdown_view = \"rendered\"\n").unwrap();
    let config = herdr_reviewr::config::plugin_config_in(dir.path()).unwrap();
    let mut app = App::new(repo.path_buf(), Scope::Uncommitted, None);
    app.seed_from_config(&config);
    app.reload().unwrap();
    app
}
