//! Integration tests for `git.rs` against real repositories.

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::Repo;
use herdr_reviewr::git::{
    ResolvedBase, abbreviate_oid, all_files, changed_against_tree,
    changed_files as changed_files_oid, default_branch_name, file_content, list_branches,
    merge_base as merge_base_oid, read_base_pick, read_baseline_ref, resolve_base, resolve_commit,
    snapshot_worktree, write_base_pick, write_baseline_ref,
};
use herdr_reviewr::model::{ChangeKind, ChangedFile, Scope};

fn by_path(files: &[ChangedFile]) -> HashMap<&str, &ChangedFile> {
    files.iter().map(|f| (f.path.as_str(), f)).collect()
}

fn changed_files(
    repo: &Path,
    scope: Scope,
    base: Option<&str>,
) -> anyhow::Result<Vec<ChangedFile>> {
    let winner = resolve_base(repo, base).map_err(|e| anyhow::anyhow!("{}", e.0))?.status.winner;
    changed_files_oid(repo, scope, winner.as_ref().map(herdr_reviewr::git::ResolvedBase::oid))
}

fn merge_base(repo: &Path, base: Option<&str>) -> Option<String> {
    let winner = resolve_base(repo, base).ok()?.status.winner?;
    merge_base_oid(repo, winner.oid())
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
fn the_chain_is_flag_then_pick_then_default() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.set_origin_default("main", "HEAD");
    r.git(&["branch", "picked-base"]);
    r.git(&["branch", "flagged-base"]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    // Default branch alone: `origin/HEAD` names `main` (specs/review-model.md).
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "main");

    // A pick outranks the default.
    write_base_pick(r.path(), "picked-base").unwrap();
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "picked-base");

    // The flag outranks the pick.
    let winner = resolve_base(r.path(), Some("flagged-base")).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "flagged-base");
}

#[test]
fn base_resolves_via_the_pick_without_a_flag() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let branch_point = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    // No flag, no origin: only a recorded pick names the base (specs/review-model.md).
    assert_eq!(merge_base(r.path(), None), None);
    write_base_pick(r.path(), "main").unwrap();
    assert_eq!(merge_base(r.path(), None), Some(branch_point));
}

#[test]
fn a_nonexistent_flag_falls_through_and_reads_as_skipped() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let branch_point = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");
    write_base_pick(r.path(), "main").unwrap();

    // A `--base` naming no existing ref is skipped, not an error; the pick resolves, and
    // the header can name the dead flag (specs/review-model.md, specs/tui.md).
    assert_eq!(merge_base(r.path(), Some("no-such-ref")), Some(branch_point));
    let status = resolve_base(r.path(), Some("no-such-ref")).unwrap().status;
    assert_eq!(status.skipped.as_deref(), Some("no-such-ref"));
}

#[test]
fn a_prefixed_flag_spelling_resolves_to_the_bare_name() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.set_origin_default("main", "HEAD");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    // `--base origin/main` resolves as a verbatim rev, but the header and the PR name
    // shield carry the bare spelling (specs/tui.md, specs/forge-host.md).
    let winner = resolve_base(r.path(), Some("origin/main")).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "main");

    // A prefixed rev that is not a branch keeps the flag spelling, so `origin/HEAD` does
    // not paint as live `HEAD`.
    let winner = resolve_base(r.path(), Some("origin/HEAD")).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "origin/HEAD");

    let status = resolve_base(r.path(), Some("origin/HEAD~99")).unwrap().status;
    assert_eq!(status.skipped.as_deref(), Some("origin/HEAD~99"));

    // A prefixed spelling that resolves to nothing is skipped under the same bare name,
    // so the header reads `· gone missing`, never `· origin/gone missing`.
    let status = resolve_base(r.path(), Some("origin/gone")).unwrap().status;
    assert_eq!(status.skipped.as_deref(), Some("gone"));
}

#[test]
fn a_pick_git_could_never_have_written_is_no_pick() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.set_origin_default("main", "HEAD");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    // The pick ref is shared repository state any tool can write, and a skipped pick paints
    // its name in the header: a blob carrying control bytes is no pick at all, so nothing
    // can smuggle an escape sequence into the frame (specs/review-model.md).
    r.write_raw_base_pick("dev\u{1b}]0;pwned\u{7}");
    assert_eq!(read_base_pick(r.path()).unwrap(), None);

    // A leftover expression in the blob is a spelling (`specs/review-model.md`). Too
    // deep to resolve, it is skipped, not discarded.
    r.write_raw_base_pick("main~5");
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("main~5"));

    let status = resolve_base(r.path(), None).unwrap().status;
    assert_eq!(status.skipped.as_deref(), Some("main~5"));
    assert_eq!(status.winner.unwrap().name(), "main");
}

#[test]
fn a_dormant_pick_is_skipped_and_reactivates() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.set_origin_default("main", "HEAD");
    r.git(&["branch", "dev"]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");
    write_base_pick(r.path(), "dev").unwrap();

    // The pick wins while its branch resolves.
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "dev");

    // The branch disappears: the pick is kept and skipped, the default wins, and the
    // header can say so (specs/review-model.md).
    r.git(&["branch", "-D", "dev"]);
    let status = resolve_base(r.path(), None).unwrap().status;
    let winner = status.winner.unwrap();
    assert_eq!(winner.name(), "main");
    assert_eq!(status.skipped.as_deref(), Some("dev"));
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("dev"));

    // The branch returns: the pick reactivates without a new choice.
    r.git(&["branch", "dev", "main"]);
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "dev");
}

#[test]
fn a_dormant_pick_survives_even_when_nothing_resolves() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    write_base_pick(r.path(), "gone").unwrap();

    // No flag, no default, and the picked branch is missing: the skip still reports, so
    // the header reads `no base · gone missing`, never a bare `no base` (specs/tui.md).
    let status = resolve_base(r.path(), None).unwrap().status;
    assert_eq!(status.winner, None);
    assert_eq!(status.skipped.as_deref(), Some("gone"));
}

fn ref_names(repo: &std::path::Path, prefix: &str) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "for-each-ref", "--format=%(refname)", prefix])
        .output()
        .expect("git");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect()
}

#[test]
fn the_pick_persists_in_a_private_worktree_ref() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");

    assert_eq!(read_base_pick(r.path()).unwrap(), None);
    write_base_pick(r.path(), "dev").unwrap();
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("dev"));
    write_base_pick(r.path(), "release/1.0").unwrap();
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("release/1.0"));

    let reviewr_before = ref_names(r.path(), "refs/reviewr");
    write_base_pick(r.path(), "dev").unwrap();
    let tree = snapshot_worktree(r.path()).unwrap();
    write_baseline_ref(r.path(), &tree).unwrap();
    assert_eq!(
        ref_names(r.path(), "refs/worktree/reviewr"),
        ["refs/worktree/reviewr/base-pick", "refs/worktree/reviewr/turn-base"]
    );
    assert_eq!(ref_names(r.path(), "refs/reviewr"), reviewr_before);
    assert_eq!(r.git(&["status", "--porcelain"]).trim(), "");
}

#[test]
fn a_head_tilde_pick_re_resolves_after_a_commit() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("one");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    write_base_pick(r.path(), "HEAD~1").unwrap();

    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.oid(), parent);
    assert_eq!(winner.name(), "HEAD~1");
    assert!(matches!(winner, ResolvedBase::Rev { .. }));
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("HEAD~1"));

    r.write("base.rs", "3\n");
    r.commit_all("two");
    let moved = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    assert_ne!(moved, parent);
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "HEAD~1");
    assert_eq!(winner.oid(), moved, "a later commit still diffs one back");
    assert_eq!(merge_base(r.path(), None).as_deref(), Some(moved.as_str()));
}

#[test]
fn a_sha_pick_stays_pinned() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("one");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    write_base_pick(r.path(), &parent).unwrap();

    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.oid(), parent);
    assert_eq!(winner.name(), parent);
    assert!(matches!(winner, ResolvedBase::Rev { .. }));

    r.write("base.rs", "3\n");
    r.commit_all("two");
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.oid(), parent, "a SHA spelling is a pin");
    assert_eq!(merge_base(r.path(), None).as_deref(), Some(parent.as_str()));
}

#[test]
fn a_tag_pick_re_resolves() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("one");
    let first = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    r.git(&["tag", "qa-pin-base", &first]);
    write_base_pick(r.path(), "qa-pin-base").unwrap();

    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "qa-pin-base");
    assert_eq!(winner.oid(), first);
    assert!(matches!(winner, ResolvedBase::Rev { .. }));

    let tip = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["tag", "-f", "qa-pin-base", &tip]);
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "qa-pin-base");
    assert_eq!(winner.oid(), tip, "moving the tag moves the base");
}

#[test]
fn a_unique_short_sha_pick_keeps_that_spelling() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let oid = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    let short = abbreviate_oid(&oid);
    write_base_pick(r.path(), &short).unwrap();
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), short);
    assert_eq!(winner.oid(), oid);
    assert!(matches!(winner, ResolvedBase::Rev { .. }));
}

#[test]
fn a_unique_short_sha_resolves_and_a_too_deep_or_dashed_rev_does_not() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let oid = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    let short = abbreviate_oid(&oid);
    assert_eq!(resolve_commit(r.path(), &short).unwrap().as_deref(), Some(oid.as_str()));
    assert_eq!(resolve_commit(r.path(), "HEAD~1").unwrap(), None, "too deep is a miss");
    assert_eq!(resolve_commit(r.path(), "-n").unwrap(), None, "a leading dash is not a rev");
}

#[test]
fn a_tree_ish_does_not_resolve_as_a_commit() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let tree = r.git(&["rev-parse", "HEAD^{tree}"]).trim().to_string();
    assert_eq!(resolve_commit(r.path(), &tree).unwrap(), None);
}

#[test]
fn a_flag_that_is_not_a_branch_keeps_its_spelling() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.write("base.rs", "1b\n");
    r.commit_all("main-2");
    r.set_origin_default("main", "HEAD");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let winner = resolve_base(r.path(), Some("HEAD~1")).unwrap().status.winner.unwrap();
    assert_eq!(winner.oid(), parent);
    assert_eq!(winner.name(), "HEAD~1");
    assert!(matches!(winner, ResolvedBase::Rev { .. }));
}

#[test]
fn a_missing_head_tilde_pick_is_skipped_and_reactivates() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.set_origin_default("main", "HEAD");
    write_base_pick(r.path(), "HEAD~1").unwrap();

    let status = resolve_base(r.path(), None).unwrap().status;
    assert_eq!(status.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name), Some("main"));
    assert_eq!(status.skipped.as_deref(), Some("HEAD~1"));

    r.write("base.rs", "2\n");
    r.commit_all("two");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "HEAD~1");
    assert_eq!(winner.oid(), parent);
}

#[test]
fn default_branch_name_reads_the_origin_head_symref() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    assert_eq!(default_branch_name(r.path()).unwrap(), None);
    r.set_origin_default("trunk", "HEAD");
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("trunk"));
}

#[test]
fn a_dangling_origin_head_symref_names_no_default() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.set_origin_default("master", "HEAD");

    // `fetch --prune` after a server-side rename deletes the target but leaves the
    // symref: a name resolving to nothing is no default (specs/review-model.md).
    r.git(&["update-ref", "-d", "refs/remotes/origin/master"]);
    assert_eq!(default_branch_name(r.path()).unwrap(), None);
}

#[test]
fn a_plain_ref_origin_head_names_the_matching_tip() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let oid = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["update-ref", "refs/remotes/origin/trunk", &oid]);

    // Some clones carry `origin/HEAD` as a plain ref, not a symref: the default is the
    // origin tip at the same commit (specs/review-model.md).
    r.git(&["update-ref", "refs/remotes/origin/HEAD", &oid]);
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("trunk"));
}

#[test]
fn list_branches_merges_names_newest_first_and_hides_the_checked_out() {
    let r = Repo::init();
    r.write("a.rs", "1\n");
    r.git(&["add", "-A"]);
    r.git_env(&["commit", "-q", "-m", "one"], &[("GIT_COMMITTER_DATE", "2026-01-01T00:00:00")]);
    r.git(&["branch", "older"]);
    // Every branch gets its own commit date, so the asserted order follows the contract
    // rather than git's tie-break between two branches sharing a timestamp.
    r.write("a.rs", "1b\n");
    r.git(&["add", "-A"]);
    r.git_env(&["commit", "-q", "-m", "middle"], &[("GIT_COMMITTER_DATE", "2026-02-01T00:00:00")]);
    r.set_origin_default("main", "HEAD");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("a.rs", "2\n");
    r.commit_all("two");
    r.git(&["branch", "newer"]);

    // Local and origin names merge (main exists only as origin/main here), the newest
    // commit sorts first, and the checked-out branch is not listed (specs/input.md).
    let names = list_branches(r.path(), default_branch_name(r.path()).unwrap().as_deref()).unwrap();
    assert_eq!(names, ["newer", "main", "older"]);
}

#[test]
fn the_checked_out_default_branch_stays_listed() {
    let r = Repo::init();
    r.write("a.rs", "1\n");
    r.git(&["add", "-A"]);
    r.git_env(&["commit", "-q", "-m", "one"], &[("GIT_COMMITTER_DATE", "2026-01-01T00:00:00")]);
    r.git(&["branch", "dev"]);
    r.write("a.rs", "2\n");
    r.commit_all("two");
    r.set_origin_default("main", "HEAD");

    // Checked out on the default branch itself: its row stays so that name can still be picked.
    let names = list_branches(r.path(), default_branch_name(r.path()).unwrap().as_deref()).unwrap();
    assert_eq!(names, ["main", "dev"]);
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
fn branch_scope_is_empty_without_a_recorded_base() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.git(&["branch", "-m", "main", "master"]); // no `main` ref exists anymore
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("feature.rs", "x\n");
    r.commit_all("feature work");

    // base = None and nothing recorded → no base, and the scope lists nothing rather than
    // guessing (specs/review-model.md).
    let files = changed_files(r.path(), Scope::Branch, None).unwrap();
    assert!(files.is_empty(), "no source resolves, so the scope shows nothing");

    // A pick of `master` brings the scope back.
    write_base_pick(r.path(), "master").unwrap();
    let files = changed_files(r.path(), Scope::Branch, None).unwrap();
    assert!(files.iter().any(|f| f.path == "feature.rs"), "the pick names the base");
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
    assert!(read_baseline_ref(r.path()).is_none(), "no baseline initially");

    let tree = snapshot_worktree(r.path()).unwrap();
    write_baseline_ref(r.path(), &tree).unwrap();
    assert_eq!(read_baseline_ref(r.path()).as_deref(), Some(tree.as_str()));

    assert!(!r.git(&["branch", "-a"]).contains("reviewr"), "the baseline is not a branch");
    assert!(
        r.git(&["show-ref"]).contains("refs/worktree/reviewr/turn-base"),
        "the baseline lives under the private worktree namespace"
    );
}

#[test]
fn a_pick_in_one_worktree_is_invisible_in_its_sibling() {
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    let linked = r.add_worktree("feature");

    write_base_pick(r.path(), "main").unwrap();
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("main"));
    assert_eq!(read_base_pick(linked.path()).unwrap(), None, "main → linked");

    write_base_pick(linked.path(), "main").unwrap();
    assert_eq!(read_base_pick(linked.path()).unwrap().as_deref(), Some("main"));
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("main"));

    let other = r.add_worktree("other");
    write_base_pick(linked.path(), "feature").unwrap();
    assert_eq!(read_base_pick(other.path()).unwrap(), None, "linked → linked");
    assert_eq!(read_base_pick(linked.path()).unwrap().as_deref(), Some("feature"));
}

#[test]
fn a_turn_baseline_in_one_worktree_is_invisible_in_its_sibling() {
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let linked = r.add_worktree("feature");
    let tree = snapshot_worktree(r.path()).unwrap();

    write_baseline_ref(r.path(), &tree).unwrap();
    assert_eq!(read_baseline_ref(r.path()).as_deref(), Some(tree.as_str()));
    assert_eq!(read_baseline_ref(linked.path()), None, "main → linked");

    write_baseline_ref(linked.path(), &tree).unwrap();
    assert_eq!(read_baseline_ref(linked.path()).as_deref(), Some(tree.as_str()));
    let other = r.add_worktree("other");
    assert_eq!(read_baseline_ref(other.path()), None, "linked → linked");
}

#[test]
fn a_planted_shared_pick_is_not_this_worktrees_pick() {
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    let linked = r.add_worktree("feature");
    r.plant_legacy_base_pick("dev");

    assert_eq!(read_base_pick(linked.path()).unwrap(), None);
    let status = resolve_base(linked.path(), None).unwrap().status;
    assert_eq!(status.winner.as_ref().map(ResolvedBase::name), Some("main"));
}

#[test]
fn a_planted_hash_baseline_is_not_this_worktrees_last_turn() {
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let tree = snapshot_worktree(r.path()).unwrap();
    r.plant_legacy_turn_base(&tree);
    assert_eq!(read_baseline_ref(r.path()), None);
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

// --- commits scope (specs/review-model.md Commit pick) ---------------------------------------

/// `main` with three commits over the root, each touching its own file, plus `feature`
/// with one commit. Returns the shas of the four `main` commits, root first.
fn run_repo() -> (Repo, Vec<String>) {
    let r = Repo::init();
    r.write("root.rs", "r\n");
    r.commit_all("root");
    r.write("one.rs", "1\n");
    r.commit_all("one");
    r.write("two.rs", "2\n");
    r.write("root.rs", "r2\n");
    r.commit_all("two");
    r.write("three.rs", "3\n");
    r.commit_all("three");
    let log = r.git(&["rev-list", "--reverse", "HEAD"]);
    let shas: Vec<String> = log.lines().map(str::to_string).collect();
    (r, shas)
}

#[test]
fn a_run_of_three_diffs_its_oldest_parent_against_its_newest() {
    use herdr_reviewr::git::{changed_between, parent_or_empty};
    let (r, shas) = run_repo();
    // A dirty worktree and an untracked file stay out: both sides come from commits.
    r.write("root.rs", "dirty\n");
    r.write("untracked.rs", "u\n");
    let old = parent_or_empty(r.path(), &shas[1]).unwrap();
    assert_eq!(old, shas[0], "A^ is the root");
    let files = changed_between(r.path(), &old, &shas[3]).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["one.rs", "root.rs", "three.rs", "two.rs"]);
    let by = by_path(&files);
    assert_eq!(by["one.rs"].kind, ChangeKind::Added);
    assert_eq!(by["root.rs"].kind, ChangeKind::Modified);
    assert_eq!((by["root.rs"].additions, by["root.rs"].deletions), (1, 1));

    // A run of one is the commit alone.
    let one =
        changed_between(r.path(), &parent_or_empty(r.path(), &shas[2]).unwrap(), &shas[2]).unwrap();
    let paths: Vec<&str> = one.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["root.rs", "two.rs"]);
}

#[test]
fn a_root_commit_diffs_against_the_empty_tree() {
    use herdr_reviewr::git::{EMPTY_TREE, changed_between, parent_or_empty, run_length};
    let (r, shas) = run_repo();
    let old = parent_or_empty(r.path(), &shas[0]).unwrap();
    assert_eq!(old, EMPTY_TREE);
    let files = changed_between(r.path(), &old, &shas[0]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "root.rs");
    assert_eq!(files[0].kind, ChangeKind::Added);
    assert_eq!(run_length(r.path(), &shas[0], &shas[3]), Some(4), "a run from the root counts");
    assert_eq!(run_length(r.path(), &shas[1], &shas[3]), Some(3));
    assert_eq!(run_length(r.path(), &shas[2], &shas[2]), Some(1));
    assert_eq!(run_length(r.path(), &shas[3], &shas[1]), None, "a reversed run is no run");
}

#[test]
fn a_merge_commit_contributes_its_tree_change() {
    use herdr_reviewr::git::{CommitRef, changed_between, parent_or_empty};
    let (r, shas) = run_repo();
    r.git(&["checkout", "-q", "-b", "side", &shas[1]]);
    r.write("side.rs", "s\n");
    r.commit_all("side");
    r.git(&["checkout", "-q", "main"]);
    r.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    let merge = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    // The merge alone, against its first parent: the side branch's file arrives.
    let old = parent_or_empty(r.path(), &merge).unwrap();
    assert_eq!(old, shas[3]);
    let files = changed_between(r.path(), &old, &merge).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["side.rs"]);
    // The row knows it is a merge, its author, and the refs pointing at it, by kind.
    r.git(&["tag", "v1"]);
    r.git(&["branch", "other", &shas[3]]);
    r.git(&["update-ref", "refs/remotes/origin/main", &shas[2]]);
    let rows = herdr_reviewr::git::list_commits(r.path(), None).unwrap();
    assert!(rows[0].merge && !rows[1].merge);
    assert_eq!(rows[0].author, "Test");
    assert_eq!(rows[0].refs, [CommitRef::Tag("v1".into())], "HEAD and its branch are dropped");
    assert_eq!(rows[1].refs, [CommitRef::Branch("other".into())]);
    assert_eq!(rows[2].refs, [CommitRef::Remote("origin/main".into())]);
    // The universe is the first-parent walk: the side branch's commit is behind the merge
    // row, never a row of its own, so any contiguous run is one ancestor chain.
    let subjects: Vec<&str> = rows.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["merge side", "three", "two", "one", "root"]);
}

#[test]
fn a_shallow_cut_is_gone_not_a_root() {
    use herdr_reviewr::git::{EMPTY_TREE, commit_exists, parent_or_empty};
    let (r, shas) = run_repo();
    let shallow = tempfile::tempdir().unwrap();
    let url = format!("file://{}", r.path().display());
    let out = std::process::Command::new("git")
        .args(["clone", "-q", "--depth", "1", &url, "w"])
        .current_dir(shallow.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let w = shallow.path().join("w");
    // `HEAD`'s parent is named by the commit object even though the clone lacks it, so the
    // pick reads `gone` rather than diffing the whole tree against the empty tree.
    let parent = parent_or_empty(&w, &shas[3]).unwrap();
    assert_eq!(parent, shas[2]);
    assert_ne!(parent, EMPTY_TREE);
    assert!(!commit_exists(&w, &parent), "the cut parent is not in the clone");
    assert_eq!(parent_or_empty(&w, &shas[0]), None, "the root itself is not in the clone");
}

#[test]
fn a_rewritten_commit_still_diffs_and_a_pruned_one_is_missing() {
    use herdr_reviewr::git::{
        changed_between, commit_exists, is_reachable, list_commits, parent_or_empty,
    };
    let (r, shas) = run_repo();
    assert!(is_reachable(r.path(), &shas[2]));
    // Rewrite the tip: the old commits keep their objects but leave `HEAD`'s history.
    r.git(&["reset", "-q", "--hard", &shas[1]]);
    r.write("three.rs", "rewritten\n");
    r.commit_all("three again");
    assert!(commit_exists(r.path(), &shas[3]), "the rewritten commit is still an object");
    assert!(!is_reachable(r.path(), &shas[3]), "but it is off branch");
    let files =
        changed_between(r.path(), &parent_or_empty(r.path(), &shas[3]).unwrap(), &shas[3]).unwrap();
    assert_eq!(files[0].path, "three.rs", "an off-branch pick keeps diffing");

    // Prune it: the pick is gone.
    r.git(&["reflog", "expire", "--expire=now", "--all"]);
    r.git(&["gc", "-q", "--prune=now"]);
    assert!(!commit_exists(r.path(), &shas[3]));
    assert!(parent_or_empty(r.path(), &shas[3]).is_none(), "a pruned sha has no parent");
    assert!(!is_reachable(r.path(), &shas[3]));

    // The universe lists what `HEAD` holds, newest first.
    let rows = list_commits(r.path(), None).unwrap();
    let subjects: Vec<&str> = rows.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["three again", "one", "root"]);
    assert!(rows.iter().all(|c| c.time > 0));
}

#[test]
fn the_universe_is_the_branch_over_its_base_or_the_last_fifty() {
    use herdr_reviewr::git::list_commits;
    let (r, shas) = run_repo();
    r.set_origin_default("main", &shas[1]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("f.rs", "f\n");
    r.commit_all("feature");
    let base = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    let over = list_commits(r.path(), Some(base.oid())).unwrap();
    let subjects: Vec<&str> = over.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["feature", "three", "two"], "merge-base..HEAD over origin/main at `one`");
    let all = list_commits(r.path(), None).unwrap();
    assert_eq!(all.len(), 5, "without a base, everything reachable up to 50");
    assert_eq!(all[0].subject, "feature");
    assert_eq!(all[4].sha, shas[0]);

    let empty = Repo::init();
    assert!(list_commits(empty.path(), None).unwrap().is_empty(), "an unborn repo lists nothing");
}

#[test]
fn the_commit_scope_writes_nothing() {
    use herdr_reviewr::git::{changed_between, list_commits, parent_or_empty, run_length};
    let (r, shas) = run_repo();
    r.write("root.rs", "dirty\n");
    let before = (
        r.git(&["for-each-ref"]),
        r.git(&["status", "--porcelain"]),
        r.git(&["rev-parse", "HEAD"]),
        r.git(&["write-tree"]),
    );
    let old = parent_or_empty(r.path(), &shas[1]).unwrap();
    changed_between(r.path(), &old, &shas[3]).unwrap();
    list_commits(r.path(), None).unwrap();
    run_length(r.path(), &shas[1], &shas[3]);
    let after = (
        r.git(&["for-each-ref"]),
        r.git(&["status", "--porcelain"]),
        r.git(&["rev-parse", "HEAD"]),
        r.git(&["write-tree"]),
    );
    assert_eq!(before, after, "no ref, index, worktree, or HEAD change");
}
