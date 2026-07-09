//! Integration tests for `git.rs` against real repositories.

mod common;

use std::collections::HashMap;

use common::Repo;
use herdr_reviewr::git::{
    all_files, changed_against_tree, changed_files, file_content, merge_base, read_baseline_ref,
    snapshot_worktree, worktree_key, write_baseline_ref,
};
use herdr_reviewr::model::{ChangeKind, ChangedFile, Scope};

fn by_path(files: &[ChangedFile]) -> HashMap<&str, &ChangedFile> {
    files.iter().map(|f| (f.path.as_str(), f)).collect()
}

#[test]
fn lists_every_change_kind_with_stats() {
    let r = Repo::init();
    r.write("keep.rs", "fn a() {}\n");
    r.write("gone.rs", "fn g() {}\n");
    r.write("edit.rs", "one\ntwo\nthree\n");
    r.commit_all("init");

    r.write("edit.rs", "one\nTWO\nthree\nfour\n"); // modify
    r.write("added.rs", "new\n"); // staged add
    r.git(&["add", "added.rs"]);
    r.remove("gone.rs"); // delete
    r.write("untracked.rs", "u\n"); // untracked

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let files = by_path(&files);

    assert_eq!(files["edit.rs"].kind, ChangeKind::Modified);
    assert_eq!(files["added.rs"].kind, ChangeKind::Added);
    assert_eq!(files["gone.rs"].kind, ChangeKind::Deleted);
    assert_eq!(files["untracked.rs"].kind, ChangeKind::Untracked);
    assert!(files["edit.rs"].additions >= 1, "additions counted");
    assert!(files["edit.rs"].deletions >= 1, "deletions counted");
}

#[test]
fn file_content_reads_the_committed_version_not_the_worktree() {
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\ngamma\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nBETA\ngamma\n"); // the worktree moves on

    // The old side of a diff: HEAD's content, not the working tree.
    assert_eq!(file_content(r.path(), "HEAD", "a.rs"), "alpha\nbeta\ngamma\n");
}

#[test]
fn file_content_is_empty_for_a_path_absent_at_that_rev() {
    let r = Repo::init();
    r.write("seed.rs", "x\n");
    r.commit_all("init");
    r.write("fresh.rs", "line one\nline two\n"); // untracked — not in HEAD

    // An added/untracked file has no old side, so its HEAD content is empty.
    assert_eq!(file_content(r.path(), "HEAD", "fresh.rs"), "");
    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    assert_eq!(by_path(&files)["fresh.rs"].additions, 2);
}

#[test]
fn merge_base_is_the_branch_point() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let branch_point = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    assert_eq!(merge_base(r.path(), Some("main")), Some(branch_point));
}

#[test]
fn base_resolves_via_the_default_list_without_a_flag() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let branch_point = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    // No flag: the default `base_branches` list skips the absent `origin/*` and finds `main`.
    assert_eq!(merge_base(r.path(), None), Some(branch_point));
}

#[test]
fn a_nonexistent_flag_falls_through_to_the_list() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let branch_point = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    // A `--base` naming no existing ref is skipped, not an error; resolution uses the list.
    assert_eq!(merge_base(r.path(), Some("no-such-ref")), Some(branch_point));
}

#[test]
fn branch_scope_is_a_superset_of_uncommitted() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("committed.rs", "new\n");
    r.commit_all("feature work");
    r.write("dirty.rs", "wip\n"); // uncommitted edit
    r.write("untracked.rs", "scratch\n"); // untracked, not yet added

    let branch = changed_files(r.path(), Scope::Branch, Some("main")).unwrap();
    let names: Vec<&str> = branch.iter().map(|f| f.path.as_str()).collect();
    assert!(names.contains(&"committed.rs"), "branch shows committed work");
    assert!(names.contains(&"dirty.rs"), "branch shows uncommitted edits");
    assert!(names.contains(&"untracked.rs"), "branch shows untracked files");

    // Branch is a superset of uncommitted.
    let uncommitted = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    for f in &uncommitted {
        assert!(names.contains(&f.path.as_str()), "branch contains uncommitted {}", f.path);
    }
}

#[test]
fn branch_scope_equals_uncommitted_when_head_is_the_base() {
    // HEAD sits exactly on the base, so the merge-base is HEAD: branch shows the
    // working-tree changes rather than going empty.
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.write("base.rs", "1\nchanged\n"); // uncommitted edit to a tracked file

    let branch = changed_files(r.path(), Scope::Branch, Some("main")).unwrap();
    assert!(branch.iter().any(|f| f.path == "base.rs"), "branch is not empty at the base");
}

#[test]
fn ignored_paths_never_enter_changes() {
    let r = Repo::init();
    r.write(".gitignore", "ignored/\nbuild/\n");
    r.commit_all("init");
    r.write("ignored/note.md", "scratch\n");
    r.write("build/out.o", "junk\n");

    // Every scope respects .gitignore, without exception: a path git ignores is not a
    // change. To review a file, track it (specs/review-model.md).
    let has_ignored = |files: &[ChangedFile]| {
        files.iter().any(|f| f.path.starts_with("ignored/") || f.path.starts_with("build/"))
    };
    assert!(
        !has_ignored(&changed_files(r.path(), Scope::Uncommitted, None).unwrap()),
        "uncommitted"
    );
    assert!(!has_ignored(&changed_files(r.path(), Scope::Branch, Some("main")).unwrap()), "branch");

    // last-turn: even an ignored file that changes within the turn stays out, because the
    // baseline snapshot and the live snapshot both honor .gitignore.
    let base = snapshot_worktree(r.path()).unwrap();
    r.write("ignored/note.md", "scratch v2\n");
    assert!(!has_ignored(&changed_against_tree(r.path(), &base).unwrap()), "last-turn");
}

#[test]
fn branch_scope_falls_back_to_master_when_main_is_absent() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.git(&["branch", "-m", "main", "master"]); // no `main` ref exists anymore
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("feature.rs", "x\n");
    r.commit_all("feature work");

    // base = None → the fallback chain (origin/main, origin/master, main, master) finds master.
    let files = changed_files(r.path(), Scope::Branch, None).unwrap();
    assert!(files.iter().any(|f| f.path == "feature.rs"), "resolved master as the base ref");
}

#[test]
fn rename_is_reported_at_the_new_path() {
    let r = Repo::init();
    r.write("old_name.rs", "stable contents that survive the move\n");
    r.commit_all("init");
    r.git(&["mv", "old_name.rs", "new_name.rs"]);

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let renamed = files.iter().find(|f| f.kind == ChangeKind::Renamed).expect("a renamed file");
    assert_eq!(renamed.path, "new_name.rs");
    // The old path is carried so the diff can read the old content and show `old → new`.
    assert_eq!(renamed.previous_path.as_deref(), Some("old_name.rs"));
}

#[test]
fn a_directory_removing_rename_keeps_its_stats() {
    // Regression for the `-z` migration: `a/b/f.rs -> a/f.rs` once produced a `a//f.rs`
    // numstat key that never matched, so the renamed+edited file showed +0 -0.
    let r = Repo::init();
    r.write("a/b/file.rs", "one\ntwo\nthree\nfour\nfive\nsix\n");
    r.commit_all("init");
    r.git(&["mv", "a/b/file.rs", "a/file.rs"]);
    r.write("a/file.rs", "one\nTWO\nthree\nfour\nfive\nsix\n"); // small edit keeps it a rename

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let renamed = files.iter().find(|f| f.kind == ChangeKind::Renamed).expect("a renamed file");
    assert_eq!(renamed.path, "a/file.rs");
    assert_eq!(renamed.previous_path.as_deref(), Some("a/b/file.rs"));
    assert!(renamed.additions + renamed.deletions > 0, "the edit's stats are counted");
}

#[test]
fn untracked_paths_with_spaces_survive_verbatim() {
    // `-z` status never quotes or trims, so a name with spaces round-trips byte-for-byte.
    let r = Repo::init();
    r.write("seed.rs", "x\n");
    r.commit_all("init");
    r.write("a file with spaces.rs", "u\n");

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let f = by_path(&files)["a file with spaces.rs"];
    assert_eq!(f.kind, ChangeKind::Untracked);
    assert_eq!(f.additions, 1);
}

#[test]
fn untracked_files_in_a_new_directory_are_listed_individually() {
    // git collapses a brand-new directory to one `dir/` entry by default; `--untracked-files=all`
    // expands it so each new file is reviewable, not the directory.
    let r = Repo::init();
    r.write("seed.rs", "x\n");
    r.commit_all("init");
    r.write("docs/new/a.md", "alpha\n");
    r.write("docs/new/b.md", "beta\n");

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let by = by_path(&files);
    assert!(by.contains_key("docs/new/a.md"), "the file is listed, not the directory");
    assert!(by.contains_key("docs/new/b.md"));
    assert!(!by.contains_key("docs/new/"), "the bare directory is not an entry");
    assert_eq!(by["docs/new/a.md"].kind, ChangeKind::Untracked);
}

#[test]
fn a_repo_with_no_commits_lists_untracked_without_erroring() {
    // A fresh `git init` has no HEAD; diffing against it would error and kill the process.
    // Diffing against the empty tree lets a commitless repo list its files instead.
    let r = Repo::init();
    r.write("fresh.rs", "one\ntwo\n");
    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    assert!(by_path(&files).contains_key("fresh.rs"), "lists files in a commitless repo");
}

#[test]
fn a_binary_change_lists_with_zero_stats() {
    let r = Repo::init();
    r.write("blob.bin", "\0\0seed\0\0");
    r.commit_all("init");
    r.write("blob.bin", "\0\0changed\0\0\0");

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let f = by_path(&files)["blob.bin"];
    assert_eq!(f.kind, ChangeKind::Modified);
    assert_eq!((f.additions, f.deletions), (0, 0));
}

#[test]
fn git_access_never_mutates_the_repo() {
    let r = Repo::init();
    r.write("a.rs", "x\n");
    r.commit_all("init");
    r.write("a.rs", "y\n");

    let head_before = r.git(&["rev-parse", "HEAD"]);
    let status_before = r.git(&["status", "--porcelain"]);

    let _ = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let _ = file_content(r.path(), "HEAD", "a.rs");
    let _ = changed_files(r.path(), Scope::Branch, Some("main")).unwrap();

    assert_eq!(head_before, r.git(&["rev-parse", "HEAD"]), "HEAD unchanged");
    assert_eq!(status_before, r.git(&["status", "--porcelain"]), "working tree unchanged");
}

// --- turn baseline (last-turn scope) -------------------------------------------

#[test]
fn changed_against_tree_shows_edits_creates_and_deletes_since_the_snapshot() {
    let r = Repo::init();
    r.write("tracked.rs", "one\ntwo\n");
    r.write("doomed.rs", "bye\n");
    r.commit_all("init");
    r.write("idle_untracked.rs", "u\n"); // untracked already at snapshot time

    let base = snapshot_worktree(r.path()).unwrap();

    // The turn: edit a tracked file, create a new file, delete one, and leave the
    // pre-existing untracked file untouched.
    r.write("tracked.rs", "one\nTWO\nthree\n");
    r.write("created.rs", "new\n");
    r.remove("doomed.rs");

    let files = changed_against_tree(r.path(), &base).unwrap();
    let files = by_path(&files);
    assert_eq!(files["tracked.rs"].kind, ChangeKind::Modified);
    assert_eq!(files["created.rs"].kind, ChangeKind::Added);
    assert_eq!(files["doomed.rs"].kind, ChangeKind::Deleted);
    assert!(
        !files.contains_key("idle_untracked.rs"),
        "an untracked file unchanged across the turn is not a phantom delete"
    );
}

#[test]
fn changed_against_tree_sees_an_untracked_only_turn() {
    // A turn whose only act is creating a new file must register as a change — the
    // promotion path depends on this being a real diff.
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let base = snapshot_worktree(r.path()).unwrap();
    r.write("fresh.rs", "x\n");
    let files = changed_against_tree(r.path(), &base).unwrap();
    assert_eq!(by_path(&files)["fresh.rs"].kind, ChangeKind::Added);
}

#[test]
fn snapshot_worktree_never_mutates_the_repo() {
    let r = Repo::init();
    r.write("a.rs", "x\n");
    r.commit_all("init");
    r.write("a.rs", "y\n");
    r.write("untracked.rs", "u\n");

    let git_dir = r.git(&["rev-parse", "--absolute-git-dir"]);
    let git_dir = std::path::Path::new(git_dir.trim());
    // The index's logical content (entries, not the racy stat cache `git status` rewrites).
    let staged_before = r.git(&["ls-files", "--stage"]);
    let status_before = r.git(&["status", "--porcelain"]);
    let head_before = r.git(&["rev-parse", "HEAD"]);
    let branches_before = r.git(&["branch", "-a"]);

    let tree = snapshot_worktree(r.path()).unwrap();
    assert_eq!(tree.len(), 40, "a tree object id");

    assert_eq!(r.git(&["ls-files", "--stage"]), staged_before, "real index entries untouched");
    assert_eq!(r.git(&["status", "--porcelain"]), status_before, "working tree status unchanged");
    assert_eq!(r.git(&["rev-parse", "HEAD"]), head_before, "HEAD unchanged");
    assert_eq!(r.git(&["branch", "-a"]), branches_before, "no branch created");
    assert!(!git_dir.join("reviewr-turn-index").exists(), "the temp index is cleaned up");
}

#[test]
fn snapshot_worktree_recovers_from_a_stale_index_lock() {
    let r = Repo::init();
    r.write("a.rs", "x\n");
    r.commit_all("init");

    let git_dir = r.git(&["rev-parse", "--absolute-git-dir"]);
    let git_dir = std::path::Path::new(git_dir.trim());
    // A hard crash mid-`add` leaves git's lock on the temp index behind; a later snapshot
    // must clear it instead of failing "Unable to create ... File exists" forever after.
    std::fs::write(git_dir.join("reviewr-turn-index.lock"), "").unwrap();

    let tree = snapshot_worktree(r.path()).unwrap();
    assert_eq!(tree.len(), 40, "a tree object id");
    assert!(!git_dir.join("reviewr-turn-index.lock").exists(), "the stale lock is cleared");
}

#[test]
fn baseline_ref_round_trips_under_the_private_namespace() {
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let key = worktree_key(r.path());
    assert!(read_baseline_ref(r.path(), &key).is_none(), "no baseline initially");

    let tree = snapshot_worktree(r.path()).unwrap();
    write_baseline_ref(r.path(), &key, &tree).unwrap();
    assert_eq!(read_baseline_ref(r.path(), &key).as_deref(), Some(tree.as_str()));

    assert!(!r.git(&["branch", "-a"]).contains("reviewr"), "the baseline is not a branch");
    assert!(
        r.git(&["show-ref"]).contains("refs/reviewr/turn-base/"),
        "the baseline lives under the private ref namespace"
    );
}

#[test]
fn worktree_key_is_stable_and_path_specific() {
    let a = std::path::Path::new("/repo/one");
    let b = std::path::Path::new("/repo/two");
    assert_eq!(worktree_key(a), worktree_key(a), "deterministic for one path");
    assert_ne!(worktree_key(a), worktree_key(b), "distinct per worktree path");
}

#[test]
fn all_files_lists_tracked_untracked_and_ignored_dirs_collapsed() {
    let r = Repo::init();
    r.write("src/app.rs", "fn main() {}\n");
    r.write("Cargo.toml", "[package]\n");
    r.commit_all("init");
    r.write("untracked.rs", "u\n"); // untracked, not ignored
    r.write(".gitignore", "target/\nbuild.log\n");
    r.write("target/build.o", "binary\n"); // ignored, in a wholly-ignored dir
    r.write("target/deep/x.o", "binary\n"); // ignored, deeper — must not be walked
    r.write("build.log", "noise\n"); // ignored, individual file

    let files = all_files(r.path()).unwrap();
    let by = |p: &str| files.iter().find(|e| e.path == p);
    assert!(by("src/app.rs").is_some_and(|e| !e.ignored && !e.is_dir), "tracked file listed");
    assert!(by("untracked.rs").is_some_and(|e| !e.ignored), "untracked-not-ignored listed");
    // A wholly-ignored directory collapses to one ignored placeholder — its contents are NOT listed.
    assert!(by("target").is_some_and(|e| e.ignored && e.is_dir), "ignored dir is a placeholder");
    assert!(!files.iter().any(|e| e.path.starts_with("target/")), "ignored dir is not walked");
    // An individually-ignored file is listed as an ignored file.
    assert!(by("build.log").is_some_and(|e| e.ignored && !e.is_dir), "ignored file listed, dimmed");

    let paths: Vec<&str> = files.iter().map(|e| e.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted, "the listing is sorted");
}

#[test]
fn list_ignored_dir_returns_immediate_children_only() {
    use herdr_reviewr::git::list_ignored_dir;
    let r = Repo::init();
    r.write(".gitignore", "target/\n");
    r.write("target/build.o", "x\n");
    r.write("target/deep/x.o", "y\n");
    r.commit_all("init");

    let kids = list_ignored_dir(r.path(), "target");
    assert!(kids.iter().all(|e| e.ignored), "every child of an ignored dir is ignored");
    assert!(kids.iter().any(|e| e.path == "target/build.o" && !e.is_dir), "immediate file");
    assert!(kids.iter().any(|e| e.path == "target/deep" && e.is_dir), "subdir as a placeholder");
    assert!(!kids.iter().any(|e| e.path == "target/deep/x.o"), "does not recurse past one level");
}
