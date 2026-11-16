//! A refresh limited to a watcher batch's paths lands exactly what a full build of the same
//! worktree would, in every scope and on both file tabs, through the real world worker.

mod common;

use std::collections::BTreeSet;
use std::sync::mpsc;

use common::Repo;
use herdr_reviewr::app::{App, Tab};
use herdr_reviewr::model::{CommitPick, Scope};
use herdr_reviewr::wake::{Waker, channel};
use herdr_reviewr::world::{self, Refresh, WorldCompletion, WorldInput, WorldJob, WorldSnapshot};

struct Worker {
    jobs: mpsc::Sender<WorldJob>,
    done: mpsc::Receiver<WorldCompletion>,
    generation: u64,
}

impl Worker {
    fn on(repo: &Repo) -> Self {
        let (jobs, rx) = mpsc::channel();
        let (tx, done) = channel(&Waker::detached());
        world::spawn(repo.path_buf(), rx, tx);
        Self { jobs, done, generation: 0 }
    }

    /// One job through the worker: the input it built for and its snapshot, `None` when nothing
    /// needed re-reading.
    fn run(&mut self, input: &WorldInput, refresh: Refresh) -> (WorldInput, Option<WorldSnapshot>) {
        self.generation += 1;
        let job =
            WorldJob { generation: self.generation, input: input.clone(), reveal: false, refresh };
        self.jobs.send(job).unwrap();
        let completion = self.done.recv().unwrap();
        (completion.input, completion.snapshot.map(|s| s.expect("the build succeeds")))
    }
}

fn paths(list: &[&str]) -> Refresh {
    Refresh::paths(list.iter().map(|p| (*p).to_string()))
}

/// A repository with every kind of file a change can touch, and a feature branch off `main`.
fn fixture() -> Repo {
    let r = Repo::init();
    r.write(".gitignore", "target/\n");
    r.write(".gitattributes", "attr.txt -diff\n");
    // Each file its own content: identical files pair renames ambiguously, and a narrow diff may
    // then pair differently from a full one (both right; the next full build settles it).
    for f in ["a.txt", "b.txt", "c.txt", "sub/d.txt", "sub/e.txt", "attr.txt"] {
        r.write(f, &format!("one\ntwo\n{f}\n"));
    }
    std::fs::write(r.path().join("bin.dat"), [0_u8, 1, 2]).unwrap();
    r.commit_all("init");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("a.txt", "one\ntwo\nfeature\n");
    r.commit_all("feature");
    r
}

/// A named mutation of the worktree and the paths its watcher batch carries.
type Step = (&'static str, Box<dyn Fn(&Repo)>, Vec<&'static str>);

/// A name full of pathspec glob characters; Windows forbids `*` in a file name.
const GLOB_NAME: &str = if cfg!(windows) { "glob[x].txt" } else { "glob*[x].txt" };

fn steps() -> Vec<Step> {
    vec![
        ("modify", Box::new(|r: &Repo| r.write("a.txt", "changed\n")), vec!["a.txt"]),
        ("untracked add", Box::new(|r: &Repo| r.write("new.txt", "x\ny\n")), vec!["new.txt"]),
        ("tracked delete", Box::new(|r: &Repo| r.remove("b.txt")), vec!["b.txt"]),
        ("untracked delete", Box::new(|r: &Repo| r.remove("new.txt")), vec!["new.txt"]),
        (
            "binary",
            Box::new(|r: &Repo| std::fs::write(r.path().join("bin.dat"), [0_u8, 9]).unwrap()),
            vec!["bin.dat"],
        ),
        ("-diff attribute", Box::new(|r: &Repo| r.write("attr.txt", "z\n")), vec!["attr.txt"]),
        (
            "rename on disk",
            Box::new(|r: &Repo| {
                std::fs::rename(r.path().join("c.txt"), r.path().join("c2.txt")).unwrap();
            }),
            vec!["c.txt", "c2.txt"],
        ),
        // A rename's new side drifts, then a closer copy of its source appears in a batch that
        // names neither side of the rename: the source pairs with the copy.
        (
            "a rename's new side drifts",
            Box::new(|r: &Repo| r.write("c2.txt", "one\ntwo\nc.txt\nmore\n")),
            vec!["c2.txt"],
        ),
        (
            "a closer copy of a rename's source",
            Box::new(|r: &Repo| r.write("c3.txt", "one\ntwo\nc.txt\n")),
            vec!["c3.txt"],
        ),
        (
            "staged rename",
            Box::new(|r: &Repo| {
                r.git(&["mv", "a.txt", "a2.txt"]);
            }),
            vec!["a.txt", "a2.txt"],
        ),
        // A later batch that names one side of a rename re-reads its partner too.
        (
            "edit a rename's new side",
            Box::new(|r: &Repo| r.write("a2.txt", "one\ntwo\nfeature\nmore\n")),
            vec!["a2.txt"],
        ),
        (
            "recreate a rename's old side",
            Box::new(|r: &Repo| r.write("a.txt", "back\n")),
            vec!["a.txt"],
        ),
        (
            "odd names",
            Box::new(|r: &Repo| {
                r.write("sp ace.txt", "x\n");
                r.write("-dash.txt", "x\n");
                r.write(GLOB_NAME, "x\n");
            }),
            vec!["sp ace.txt", "-dash.txt", GLOB_NAME],
        ),
        (
            "folder removed",
            Box::new(|r: &Repo| std::fs::remove_dir_all(r.path().join("sub")).unwrap()),
            vec!["sub"],
        ),
        ("ignored write", Box::new(|r: &Repo| r.write("target/out.o", "x\n")), vec!["target"]),
    ]
}

fn input_for(r: &Repo, scope: Scope, tab: Tab) -> WorldInput {
    let root = herdr_reviewr::git::toplevel(r.path()).unwrap();
    let mut input = App::new(root, scope, Some("main".into())).world_input();
    input.tab = tab;
    if scope == Scope::Commits {
        let head = r.git(&["rev-parse", "HEAD"]).trim().to_string();
        input.commit_pick = Some(CommitPick::single(&head));
    }
    input
}

/// Walk every step in one scope and tab, each path-limited landing checked against a full build.
fn path_limited_equals_full(scope: Scope, tab: Tab) {
    let r = fixture();
    if scope == Scope::LastTurn {
        let tree = r.git(&["rev-parse", "HEAD^{tree}"]).trim().to_string();
        herdr_reviewr::git::write_baseline_ref(r.path(), &tree).unwrap();
    }
    let mut worker = Worker::on(&r);
    let input = input_for(&r, scope, tab);
    let (_, first) = worker.run(&input, Refresh::Full);
    assert!(first.is_some(), "the first build is full");
    for (name, mutate, batch) in steps() {
        mutate(&r);
        let (built_for, scoped) = worker.run(&input, paths(&batch));
        // A pick reads committed trees: on `Changes`, a worktree batch has nothing to re-read.
        if scope == Scope::Commits && tab == Tab::Changes {
            assert!(scoped.is_none(), "{name}: a pick ignores the worktree");
            continue;
        }
        let scoped = scoped.unwrap_or_else(|| panic!("{scope:?}/{tab:?} {name}: a batch re-reads"));
        let full = world::build(&built_for).unwrap();
        let files = |s: &WorldSnapshot| s.changeset.files.clone();
        assert_eq!(files(&scoped), files(&full), "{scope:?}/{tab:?} {name}: the changeset");
        let ends = |s: &WorldSnapshot| s.changeset.ends.clone();
        assert_eq!(ends(&scoped), ends(&full), "{scope:?}/{tab:?} {name}: the ends");
        assert_eq!(scoped.entries, full.entries, "{scope:?}/{tab:?} {name}: the navigator");
        assert_eq!(scoped.checkout.as_deref(), Some("feature"), "{scope:?}/{tab:?} {name}");
        let batch: BTreeSet<String> = batch.iter().map(|p| (*p).to_string()).collect();
        let touched = scoped.touched.unwrap_or_else(|| panic!("{scope:?}/{tab:?} {name}: by path"));
        assert!(touched.is_superset(&batch), "{scope:?}/{tab:?} {name}: re-read {touched:?}");
    }
}

#[test]
fn uncommitted_changes_tab() {
    path_limited_equals_full(Scope::Uncommitted, Tab::Changes);
}

#[test]
fn uncommitted_all_files_tab() {
    path_limited_equals_full(Scope::Uncommitted, Tab::AllFiles);
}

#[test]
fn branch_changes_tab() {
    path_limited_equals_full(Scope::Branch, Tab::Changes);
}

#[test]
fn branch_all_files_tab() {
    path_limited_equals_full(Scope::Branch, Tab::AllFiles);
}

#[test]
fn last_turn_changes_tab() {
    path_limited_equals_full(Scope::LastTurn, Tab::Changes);
}

#[test]
fn last_turn_all_files_tab() {
    path_limited_equals_full(Scope::LastTurn, Tab::AllFiles);
}

#[test]
fn commits_changes_tab() {
    path_limited_equals_full(Scope::Commits, Tab::Changes);
}

#[test]
fn a_full_build_names_what_the_worktree_has_checked_out() {
    let r = fixture();
    let mut worker = Worker::on(&r);
    let input = input_for(&r, Scope::Uncommitted, Tab::Changes);
    let checkout = |worker: &mut Worker| worker.run(&input, Refresh::Full).1.unwrap().checkout;
    assert_eq!(checkout(&mut worker).as_deref(), Some("feature"));
    r.git(&["checkout", "-q", "main"]);
    assert_eq!(checkout(&mut worker).as_deref(), Some("main"), "a checkout");
    r.git(&["checkout", "-q", "--detach"]);
    let head = r.git(&["rev-parse", "--short=7", "HEAD"]).trim().to_string();
    assert_eq!(checkout(&mut worker), Some(format!("detached {head}")), "a detached HEAD");
}

#[test]
fn the_pr_tab_reads_only_the_checkout_and_leaves_the_file_tabs_frozen() {
    let r = fixture();
    let root = herdr_reviewr::git::toplevel(r.path()).unwrap();
    let mut app = App::new(root.clone(), Scope::Uncommitted, Some("main".into()));
    app.reload().unwrap();
    app.set_tab(Tab::Pr).unwrap();
    let frozen = app.entries.clone();
    r.git(&["checkout", "-q", "-b", "other"]);
    r.write("new.txt", "x\n");

    let mut worker = Worker::on(&r);
    let (_, read) = worker.run(&app.world_input(), paths(&["new.txt"]));
    assert!(read.is_none(), "a worktree batch reads nothing on the PR tab");
    let (_, read) = worker.run(&app.world_input(), Refresh::Full);
    app.reconcile_world(read.expect("a full refresh reads the checkout"));
    assert_eq!(app.worktree_name().1, Some("other"), "the footer follows the checkout");
    assert_eq!(app.entries, frozen, "the file tabs stay frozen");

    // Back on a file tab, the footer names what the frozen list was built from until it lands.
    app.set_tab(Tab::Changes).unwrap();
    assert_eq!(app.worktree_name().1, Some("feature"), "the list on screen is `feature`'s");
    // Back on `PR` before that lands, it still names the latest read.
    app.set_tab(Tab::Pr).unwrap();
    assert_eq!(app.worktree_name().1, Some("other"));
    app.set_tab(Tab::Changes).unwrap();
    let (_, read) = worker.run(&app.world_input(), Refresh::Full);
    app.reconcile_world(read.expect("a return refreshes"));
    assert_eq!(app.worktree_name().1, Some("other"), "the refresh lands both");

    // The other file tab paints its own stash the same way.
    common::enter_tab(&mut app, Tab::AllFiles);
    r.git(&["checkout", "-q", "feature"]);
    let (_, read) = worker.run(&app.world_input(), Refresh::Full);
    app.reconcile_world(read.expect("a checkout refreshes"));
    app.set_tab(Tab::Changes).unwrap();
    assert_eq!(app.worktree_name().1, Some("other"), "Changes' stash is `other`'s");
    r.git(&["checkout", "-q", "other"]);

    // A pane that opens or switches onto the PR tab names its checkout too.
    let mut opened = App::new(root, Scope::Uncommitted, Some("main".into()));
    opened.set_tab(Tab::Pr).unwrap();
    opened.reload().unwrap();
    assert_eq!(opened.worktree_name().1, Some("other"));
}

#[test]
fn a_worktree_that_stops_being_a_repo_names_no_checkout() {
    let r = fixture();
    let root = herdr_reviewr::git::toplevel(r.path()).unwrap();
    let mut app = App::new(root, Scope::Uncommitted, Some("main".into()));
    app.reload().unwrap();
    assert_eq!(app.worktree_name().1, Some("feature"));
    std::fs::remove_dir_all(r.path().join(".git")).unwrap();
    app.reload().unwrap();
    assert_eq!(app.worktree_name().1, None, "nothing is checked out outside a repo");
}

#[test]
fn an_index_rewrite_rebuilds_only_when_what_it_stages_moved() {
    let r = fixture();
    let mut worker = Worker::on(&r);
    let input = input_for(&r, Scope::Uncommitted, Tab::Changes);
    worker.run(&input, Refresh::Full);
    let index_only = Refresh::Paths { paths: BTreeSet::new(), index: true };

    // An agent's `git status` after touching files rewrites the index for stat data alone.
    let past = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
    std::fs::File::options()
        .write(true)
        .open(r.path().join("a.txt"))
        .unwrap()
        .set_modified(past)
        .unwrap();
    r.git(&["status", "--porcelain"]);
    let (_, unchanged) = worker.run(&input, index_only.clone());
    assert!(unchanged.is_none(), "a stat-only rewrite re-reads nothing");

    r.write("a.txt", "staged\n");
    r.git(&["add", "a.txt"]);
    let (built_for, staged) = worker.run(&input, index_only);
    let staged = staged.expect("a real stage rebuilds");
    assert_eq!(staged.touched, None, "in full");
    assert_eq!(staged.changeset.files, world::build(&built_for).unwrap().changeset.files);
}

#[test]
fn a_batch_past_the_byte_cap_rebuilds_in_full() {
    // Long names pass the cap in bytes long before they would in count.
    let long = |i: usize| format!("{}/{i}", "deep".repeat(60));
    let fits: BTreeSet<String> = (0..60).map(long).collect();
    let mut refresh = Refresh::default();
    refresh.absorb(Refresh::paths(fits.clone()));
    assert_eq!(refresh.named_paths(), Some(&fits), "under the cap stays by path");
    let many: BTreeSet<String> = (0..80).map(long).collect();
    refresh.absorb(Refresh::paths(many));
    assert!(refresh.is_full(), "past the cap is a full rebuild");

    let mut small = paths(&["a.txt"]);
    small.absorb(Refresh::Full);
    assert!(small.is_full(), "anything full stays full");
}

#[test]
fn commits_all_files_tab() {
    path_limited_equals_full(Scope::Commits, Tab::AllFiles);
}

#[test]
fn a_batch_rereads_the_open_file_only_when_it_names_it() {
    let r = Repo::init();
    r.write("a.txt", "one\n");
    r.write("b.txt", "b\n");
    r.commit_all("init");
    r.write("a.txt", "two\n");
    r.write("b.txt", "bb\n");
    let mut app = common::app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("a.txt"), "the first changed file is open");
    let text =
        |app: &App| app.visible.iter().map(common::UnifiedRow::text).collect::<Vec<_>>().join("\n");
    assert!(text(&app).contains("two"));

    r.write("a.txt", "three\n");
    let mut other = world::build(&app.world_input()).unwrap();
    other.touched = Some(["b.txt".to_string()].into());
    app.reconcile_world(other);
    assert!(!text(&app).contains("three"), "a batch naming only b.txt does not reread a.txt");

    let mut named = world::build(&app.world_input()).unwrap();
    named.touched = Some(["a.txt".to_string()].into());
    app.reconcile_world(named);
    assert!(text(&app).contains("three"), "a batch naming the open file rereads it");

    r.write("a.txt", "four\n");
    app.reconcile_world(world::build(&app.world_input()).unwrap());
    assert!(text(&app).contains("four"), "a full build rereads it");
}

/// A last-turn worker past its first full build, on the fixture with a baseline at `HEAD`.
fn last_turn(r: &Repo) -> (Worker, WorldInput) {
    let tree = r.git(&["rev-parse", "HEAD^{tree}"]).trim().to_string();
    herdr_reviewr::git::write_baseline_ref(r.path(), &tree).unwrap();
    let mut worker = Worker::on(r);
    let input = input_for(r, Scope::LastTurn, Tab::Changes);
    worker.run(&input, Refresh::Full);
    (worker, input)
}

#[test]
fn a_path_limited_last_turn_tree_is_the_full_snapshots_tree() {
    let r = fixture();
    let (mut worker, input) = last_turn(&r);
    r.write("a.txt", "edited\n");
    r.write("brand new.txt", "x\n");
    r.remove("b.txt");
    let (_, scoped) = worker.run(&input, paths(&["a.txt", "brand new.txt", "b.txt"]));
    let scoped = scoped.expect("a batch re-reads");
    assert!(scoped.touched.is_some(), "by path");
    let full = herdr_reviewr::git::snapshot_worktree(r.path()).unwrap();
    let ends = scoped.changeset.ends.expect("last-turn has ends");
    assert_eq!(ends.new.as_deref(), Some(full.as_str()), "the same tree id");
}

#[test]
fn a_moved_folder_past_the_byte_cap_rebuilds_in_full() {
    let r = fixture();
    // Names long enough that the folder's files pass the cap once listed one by one.
    // Each path stays under Windows' 260-character limit; together they pass the cap.
    for i in 0..110 {
        r.write(&format!("big/{}-{i}.txt", "n".repeat(150)), &format!("{i}\n"));
    }
    r.commit_all("big");
    let (mut worker, input) = last_turn(&r);
    std::fs::rename(r.path().join("big"), r.path().join("moved")).unwrap();
    let (built_for, landed) = worker.run(&input, paths(&["big", "moved"]));
    let landed = landed.expect("a batch re-reads");
    assert_eq!(landed.touched, None, "past the cap the build is full");
    let full = world::build(&built_for).unwrap();
    assert_eq!(landed.changeset.files, full.changeset.files);
}

#[cfg(unix)]
#[test]
fn a_failed_path_limited_read_retries_in_full_at_once() {
    let r = fixture();
    let (mut worker, input) = last_turn(&r);
    // A folder replaced by a link: `git add` refuses a pathspec beyond a symbolic link.
    std::fs::remove_dir_all(r.path().join("sub")).unwrap();
    std::os::unix::fs::symlink(r.path().join("target"), r.path().join("sub")).unwrap();
    let (built_for, landed) = worker.run(&input, paths(&["sub/d.txt"]));
    let landed = landed.expect("a batch re-reads");
    assert_eq!(landed.touched, None, "the retry is a full build");
    assert_eq!(landed.changeset.files, world::build(&built_for).unwrap().changeset.files);
}

#[test]
fn a_path_limited_read_never_rewrites_the_index() {
    let r = fixture();
    let mut worker = Worker::on(&r);
    let input = input_for(&r, Scope::Uncommitted, Tab::AllFiles);
    worker.run(&input, Refresh::Full);
    let index = r.path().join(".git/index");
    let stamp = || std::fs::metadata(&index).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    // Same content, a later mtime: the stat data a refresh would write back.
    r.write("a.txt", "one\ntwo\nfeature\n");
    r.write("c.txt", "edited\n");
    let before = stamp();
    let (_, landed) = worker.run(&input, paths(&["a.txt", "c.txt"]));
    assert!(landed.expect("a batch re-reads").touched.is_some(), "by path");
    assert_eq!(stamp(), before, ".git/index was rewritten");
}

#[test]
fn a_worktree_index_copy_goes_once_neither_pane_nor_worker_holds_it() {
    let r = fixture();
    let input = input_for(&r, Scope::Uncommitted, Tab::Changes);
    let copies = || {
        let dir = std::fs::read_dir(r.path().join(".git/reviewr")).unwrap();
        dir.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("index-")).count()
    };
    // One full build, then the worker stops, as the event loop's does when it ends.
    let build_then_stop = || {
        let (jobs, rx) = mpsc::channel();
        let (tx, done) = channel(&Waker::detached());
        // The event loop spawns its worker on the repo its jobs name.
        let worker = world::spawn(input.repo.clone(), rx, tx);
        let job =
            WorldJob { generation: 1, input: input.clone(), reveal: false, refresh: Refresh::Full };
        jobs.send(job).unwrap();
        done.recv().unwrap();
        assert_eq!(copies(), 1, "a build works on a session copy");
        drop(jobs);
        worker.join().unwrap();
    };
    // The pane still reviews the worktree, as after a switch back or a herdr too old to connect.
    let pane = herdr_reviewr::git::lease_session(&input.repo);
    build_then_stop();
    assert_eq!(copies(), 1, "a worker that stops under a live pane leaves the session");
    drop(pane);
    assert_eq!(copies(), 0, "the last holder ends it");
    // The pane switched away while the worker finished its build.
    build_then_stop();
    assert_eq!(copies(), 0, "the stopped worker ended its session");
}
