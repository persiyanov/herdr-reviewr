//! Integration tests for `git.rs` against real repositories.

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::Repo;
use herdr_reviewr::git::{
    DiffSides, ResolvedBase, abbreviate_oid, all_files, changed_between, changed_from,
    checked_out_branch, default_branch_name, delete_base_pick, diff_sides, list_branches,
    merge_base as merge_base_oid, merge_base_checked, read_base_pick, read_baseline_ref,
    resolve_base, resolve_commit, snapshot_worktree, write_base_pick, write_baseline_ref,
};
use herdr_reviewr::model::{ChangeKind, ChangedFile, Scope};
use herdr_reviewr::world::{WorldInput, build_changed};

fn by_path(files: &[ChangedFile]) -> HashMap<&str, &ChangedFile> {
    files.iter().map(|f| (f.path.as_str(), f)).collect()
}

/// `scope`'s changeset as the world worker builds it, the `--base` flag `base`.
fn changed_files(
    repo: &Path,
    scope: Scope,
    base: Option<&str>,
) -> anyhow::Result<Vec<ChangedFile>> {
    Ok(build_changed(&world_input(repo, scope, base, None))?
        .changeset
        .files
        .into_values()
        .collect())
}

/// `last-turn`'s changeset against the baseline `tree`, as the world worker builds it.
fn changed_against_tree(repo: &Path, tree: &str) -> anyhow::Result<Vec<ChangedFile>> {
    let build = build_changed(&world_input(repo, Scope::LastTurn, None, Some(tree)))?;
    Ok(build.changeset.files.into_values().collect())
}

fn world_input(repo: &Path, scope: Scope, base: Option<&str>, turn: Option<&str>) -> WorldInput {
    WorldInput {
        repo: repo.to_path_buf(),
        tab: herdr_reviewr::app::Tab::Changes,
        scope,
        base: base.map(str::to_string),
        base_epoch: 0,
        turn_baseline: turn.map(str::to_string),
        commit_pick: None,
        toggled_dirs: std::collections::HashSet::default(),
    }
}

fn merge_base(repo: &Path, base: Option<&str>) -> Option<String> {
    let winner = resolve_base(repo, base).ok()?.status.winner?;
    merge_base_oid(repo, winner.oid())
}

#[test]
fn a_diffs_sides_are_the_committed_blob_and_the_text_git_would_store() {
    // git is the oracle: the committed blob against what `git add` would store.
    let contents =
        ["a\r\nb\r\n", "a\nb\n", "a\r\nb\nc\r\n", "a\r\nb\rc\r\n", "a\r\nb", "$Id: x $\r\nb\r\n"];
    // core.autocrlf, .gitattributes, and the content each file was committed with.
    let lf = "x\ny\n";
    let regimes = [
        ("true", "", lf),
        ("input", "", lf),
        ("false", "", lf),
        ("false", "* text\n", lf),
        ("false", "* text=auto\n", lf),
        ("false", "* eol=lf\n", lf),
        ("true", "* -text\n", lf),
        ("true", "* text eol=crlf\n", lf),
        ("true", "* ident\n", lf),
        ("true", "* filter=upper\n", lf),
        // `auto` keeps the CRLF of a file whose index blob already holds it.
        ("true", "", "x\r\ny\r\n"),
        ("false", "* text=auto\n", "x\r\ny\r\n"),
        // An index blob with a lone CR under `-text`, then read under autocrlf.
        ("true", "", "x\ry\n"),
    ];
    for (autocrlf, attributes, committed) in regimes {
        let r = Repo::init();
        r.git(&["config", "core.autocrlf", "false"]);
        r.git(&["config", "filter.upper.clean", "tr a-z A-Z"]);
        r.write(".gitattributes", "* -text\n");
        for i in 0..contents.len() {
            r.write(&format!("f{i}.txt"), committed);
        }
        r.commit_all("base");
        r.git(&["config", "core.autocrlf", autocrlf]);
        r.write(".gitattributes", attributes);
        for (i, content) in contents.iter().enumerate() {
            let path = format!("f{i}.txt");
            let case = format!("{content:?}, autocrlf={autocrlf}, {attributes:?}, {committed:?}");
            r.write(&path, content);
            let sides = diff_sides(r.path(), "HEAD", None, &path, None).expect(&case);
            r.git(&["add", &path]);
            let old = r.git(&["cat-file", "blob", &format!("HEAD:{path}")]);
            let new = r.git(&["cat-file", "blob", &format!(":{path}")]);
            assert_eq!(sides, DiffSides::Text { old, new }, "{case}");
        }
    }
}

#[test]
fn a_submodule_bump_reads_as_git_prints_it_and_an_empty_file_as_empty() {
    let r = Repo::init();
    r.write("seed.txt", "x\n");
    r.commit_all("init");
    let (a, b) = ("1".repeat(40), "2".repeat(40));
    r.git(&["update-index", "--add", "--cacheinfo", &format!("160000,{a},sub")]);
    r.git(&["commit", "-q", "-m", "sub at a"]);
    let at_a = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["update-index", "--cacheinfo", &format!("160000,{b},sub")]);
    r.git(&["commit", "-q", "-m", "sub at b"]);
    // A user's log format would print the bump with no hunk.
    r.git(&["config", "diff.submodule", "log"]);
    let text = |old: &str, new: &str| DiffSides::Text { old: old.into(), new: new.into() };

    let bump = diff_sides(r.path(), &at_a, Some("HEAD"), "sub", None).unwrap();
    assert_eq!(
        bump,
        text(&format!("Subproject commit {a}\n"), &format!("Subproject commit {b}\n"))
    );
    r.write("empty.txt", "");
    r.git(&["add", "empty.txt"]);
    assert_eq!(diff_sides(r.path(), "HEAD", None, "empty.txt", None).unwrap(), text("", ""));
    // A staged file unstaged since the listing: git sees it at neither end, so a stale row is empty.
    r.write("gone.txt", "x\n");
    assert_eq!(diff_sides(r.path(), "HEAD", None, "gone.txt", None).unwrap(), text("", ""));
}

#[test]
fn a_renamed_files_sides_read_the_old_path_and_an_unchanged_one_reads_its_blob() {
    let r = Repo::init();
    r.write("a.txt", "one\ntwo\nthree\nfour\n");
    r.write("same.txt", "same\n");
    r.write("[x].txt", "glob\n");
    r.write("x.txt", "not me\n");
    r.commit_all("init");
    r.git(&["mv", "a.txt", "b.txt"]);
    r.write("b.txt", "one\ntwo\nthree\nFOUR\n");
    r.git(&["mv", "same.txt", "moved.txt"]);
    r.write("x.txt", "edited\n");
    let text = |old: &str, new: &str| DiffSides::Text { old: old.into(), new: new.into() };

    let renamed = diff_sides(r.path(), "HEAD", None, "b.txt", Some("a.txt")).unwrap();
    assert_eq!(renamed, text("one\ntwo\nthree\nfour\n", "one\ntwo\nthree\nFOUR\n"));
    let moved = diff_sides(r.path(), "HEAD", None, "moved.txt", Some("same.txt")).unwrap();
    assert_eq!(moved, text("same\n", "same\n"), "a pure rename: both sides are the blob");
    // A re-created, staged source never joins the rename's sides.
    r.write("a.txt", "fresh\n");
    r.git(&["add", "a.txt"]);
    let renamed = diff_sides(r.path(), "HEAD", None, "b.txt", Some("a.txt")).unwrap();
    assert_eq!(renamed, text("one\ntwo\nthree\nfour\n", "one\ntwo\nthree\nFOUR\n"));
    r.git(&["rm", "-q", "--cached", "a.txt"]);
    std::fs::remove_file(r.path().join("a.txt")).unwrap();
    // A copy's source is unchanged, so the old side is its committed content.
    r.write("copy.txt", "one\ntwo\nTHREE\nfour\n");
    r.git(&["add", "copy.txt"]);
    let copy = diff_sides(r.path(), "HEAD", None, "copy.txt", Some("x.txt")).unwrap();
    assert_eq!(copy, text("not me\n", "one\ntwo\nTHREE\nfour\n"));
    // A path is literal: `[x].txt` is not a glob that reaches the edited `x.txt`.
    let literal = diff_sides(r.path(), "HEAD", None, "[x].txt", None).unwrap();
    assert_eq!(literal, text("glob\n", "glob\n"));
    // Tree to tree, the way `commits` and `last-turn` read.
    r.commit_all("second");
    let between = diff_sides(r.path(), "HEAD~1", Some("HEAD"), "x.txt", None).unwrap();
    assert_eq!(between, text("not me\n", "edited\n"));
}

#[test]
fn a_file_that_replaced_a_directory_reads_only_its_own_sides() {
    let r = Repo::init();
    r.write("foo/a", "inner1\ninner2\n");
    r.write("bar", "plain\n");
    r.commit_all("init");
    r.remove("foo/a");
    std::fs::remove_dir(r.path().join("foo")).unwrap();
    r.write("foo", "file1\n");
    r.remove("bar");
    r.write("bar/b", "nested\n");
    r.commit_all("swap");
    let text = |old: &str, new: &str| DiffSides::Text { old: old.into(), new: new.into() };
    // `-- foo` also matches `foo/a`, which must never join `foo`'s sides.
    assert_eq!(
        diff_sides(r.path(), "HEAD~1", Some("HEAD"), "foo", None).unwrap(),
        text("", "file1\n")
    );
    assert_eq!(
        diff_sides(r.path(), "HEAD~1", Some("HEAD"), "bar", None).unwrap(),
        text("plain\n", "")
    );
    // A body line spelled like a header is still body.
    r.write("q.sql", "-- /dev/null\nkeep\n");
    r.commit_all("q");
    r.write("q.sql", "keep\n++ /dev/null\n");
    let sides = diff_sides(r.path(), "HEAD", None, "q.sql", None).unwrap();
    assert_eq!(sides, text("-- /dev/null\nkeep\n", "keep\n++ /dev/null\n"));
    // Names git quotes or ends with a tab still find their own section; Windows forbids `"`.
    let names: &[&str] = if cfg!(unix) {
        &["say \"hi\".txt", "back\\slash.txt", "two words.txt"]
    } else {
        &["two words.txt"]
    };
    for &name in names {
        r.write(name, "one\n");
        r.commit_all("add");
        r.write(name, "two\n");
        assert_eq!(diff_sides(r.path(), "HEAD", None, name, None).unwrap(), text("one\n", "two\n"));
    }
}

#[cfg(unix)]
#[test]
fn a_file_that_became_a_symlink_reads_both_its_sections() {
    let r = Repo::init();
    r.write("t", "body\n");
    r.commit_all("init");
    r.remove("t");
    std::os::unix::fs::symlink("elsewhere", r.path().join("t")).unwrap();
    let sides = diff_sides(r.path(), "HEAD", None, "t", None).unwrap();
    assert_eq!(sides, DiffSides::Text { old: "body\n".into(), new: "elsewhere".into() });
}

#[test]
fn a_path_the_diff_attribute_unsets_carries_gits_no_text_diff_verdict() {
    // `-diff` text is binary to git, and the changeset carries that verdict.
    let r = Repo::init();
    r.write(".gitattributes", "lock.txt -diff\n");
    r.write("lock.txt", "one\ntwo\n");
    r.write("plain.txt", "one\ntwo\n");
    r.commit_all("init");

    r.write("lock.txt", "one\nTWO\n");
    r.write("plain.txt", "one\nTWO\n");

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let files = by_path(&files);

    assert!(files["lock.txt"].binary, "-diff is git's no-text-diff verdict");
    assert_eq!((files["lock.txt"].additions, files["lock.txt"].deletions), (0, 0));
    assert!(!files["plain.txt"].binary, "an ordinary text file still diffs");
    assert_eq!((files["plain.txt"].additions, files["plain.txt"].deletions), (1, 1));
}

#[test]
fn the_binary_macro_and_real_binary_content_both_carry_the_verdict() {
    let r = Repo::init();
    r.write(".gitattributes", "packed.dat binary\n");
    r.write("packed.dat", "text bytes\n");
    r.write("logo.png", "\u{0}\u{1}png\n");
    r.write("empty.txt", "");
    r.commit_all("init");

    r.write("packed.dat", "other text\n");
    r.write("logo.png", "\u{0}\u{2}png\n");
    r.write("empty.txt", "\n"); // a real change of one countable line

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let files = by_path(&files);

    assert!(files["packed.dat"].binary, "the `binary` macro unsets `diff`");
    assert!(files["logo.png"].binary, "NUL bytes are git's own verdict too");
    assert!(!files["empty.txt"].binary, "a countable change is never the verdict");
}

#[test]
fn an_untracked_file_the_diff_attribute_unsets_carries_the_verdict_too() {
    // A fresh `-diff` lockfile, untracked: the verdict must not depend on being tracked.
    let r = Repo::init();
    r.write("keep.rs", "fn a() {}\n");
    r.commit_all("init");
    r.write(".gitattributes", "flake.lock -diff\n");
    r.write("flake.lock", "one\ntwo\n");
    r.write("notes.txt", "one\ntwo\n");

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let files = by_path(&files);

    assert_eq!(files["flake.lock"].kind, ChangeKind::Untracked);
    assert!(files["flake.lock"].binary, "-diff holds for an untracked path");
    assert_eq!(files["flake.lock"].additions, 0, "no countable lines, as when tracked");
    assert!(!files["notes.txt"].binary, "an ordinary untracked file still diffs");
    assert_eq!(files["notes.txt"].additions, 2);
}

#[test]
fn an_untracked_binary_file_carries_the_verdict_from_its_content() {
    // An untracked path has no numstat, so its content decides.
    let r = Repo::init();
    r.write("keep.rs", "fn a() {}\n");
    r.commit_all("init");
    r.write("blob.bin", "\u{0}\u{1}\u{2}");
    r.write("notes.txt", "one\ntwo\n");

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let files = by_path(&files);

    assert!(files["blob.bin"].binary);
    assert_eq!(files["blob.bin"].additions, 0);
    assert!(!files["notes.txt"].binary);
    assert_eq!(files["notes.txt"].additions, 2);
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
fn an_untracked_file_counts_its_lines_as_additions() {
    let r = Repo::init();
    r.write("seed.rs", "x\n");
    r.commit_all("init");
    r.write("fresh.rs", "line one\nline two\n");
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

    // Default branch alone: `origin/HEAD` names `main`.
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
    r.git(&["branch", "-m", "main", "trunk"]); // no `main`/`master`: no default to fall back on
    let branch_point = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    // No flag, no origin, no `main`: only a recorded pick names the base.
    assert_eq!(merge_base(r.path(), None), None);
    write_base_pick(r.path(), "trunk").unwrap();
    assert_eq!(merge_base(r.path(), None), Some(branch_point));
}

#[test]
fn picking_the_default_name_deletes_the_pick_so_a_re_default_is_followed() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.set_origin_default("main", "HEAD");
    r.git(&["branch", "dev"]);
    r.git(&["branch", "trunk"]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("base.rs", "2\n");
    r.commit_all("diverge");

    // A pick holds; writing the default's own name is the way back: the ref is gone.
    write_base_pick(r.path(), "dev").unwrap();
    assert_eq!(resolve_base(r.path(), None).unwrap().status.winner.unwrap().name(), "dev");
    write_base_pick(r.path(), "main").unwrap();
    assert_eq!(read_base_pick(r.path()).unwrap(), None, "the default's name is no pick");
    assert_eq!(resolve_base(r.path(), None).unwrap().status.winner.unwrap().name(), "main");

    // So the repo's next re-default is followed, where a recorded `main` would have stuck.
    r.set_origin_default("trunk", "HEAD~1");
    let status = resolve_base(r.path(), None).unwrap().status;
    assert_eq!(status.winner.unwrap().name(), "trunk");
    assert_eq!(status.skipped, None);

    // An old pick naming the default changes nothing.
    r.write_raw_base_pick("trunk");
    let status = resolve_base(r.path(), None).unwrap().status;
    assert_eq!(status.winner.unwrap().name(), "trunk");
    assert_eq!(status.skipped, None);
    r.set_origin_default("main", "HEAD~1");
    assert_eq!(
        resolve_base(r.path(), None).unwrap().status.winner.unwrap().name(),
        "trunk",
        "the legacy ref is a pick until the next Enter: a re-default does not move it"
    );
    write_base_pick(r.path(), "dev").unwrap();
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("dev"));
}

#[test]
fn deleting_the_pick_returns_to_the_default_and_touches_nothing_else() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.set_origin_default("main", "HEAD");
    r.git(&["branch", "dev"]);
    let before = ref_names(r.path(), "refs/");

    // Deleting an absent pick is a no-op, not an error.
    delete_base_pick(r.path()).unwrap();
    assert_eq!(read_base_pick(r.path()).unwrap(), None);
    assert_eq!(ref_names(r.path(), "refs/"), before);

    write_base_pick(r.path(), "dev").unwrap();
    assert_eq!(resolve_base(r.path(), None).unwrap().status.winner.unwrap().name(), "dev");
    delete_base_pick(r.path()).unwrap();
    assert_eq!(read_base_pick(r.path()).unwrap(), None);
    assert_eq!(resolve_base(r.path(), None).unwrap().status.winner.unwrap().name(), "main");
    assert_eq!(ref_names(r.path(), "refs/"), before, "only the pick ref ever changes");
    assert_eq!(r.git(&["status", "--porcelain"]).trim(), "");
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
    r.git(&["branch", "picked", "main"]);
    write_base_pick(r.path(), "picked").unwrap();

    // A dead `--base` is skipped, not an error, and the header can name it.
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

    // `--base origin/main` resolves verbatim but carries the bare name.
    let winner = resolve_base(r.path(), Some("origin/main")).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "main");

    // A prefixed non-branch keeps its spelling, so `origin/HEAD` never reads as `HEAD`.
    let winner = resolve_base(r.path(), Some("origin/HEAD")).unwrap().status.winner.unwrap();
    assert_eq!(winner.name(), "origin/HEAD");

    let status = resolve_base(r.path(), Some("origin/HEAD~99")).unwrap().status;
    assert_eq!(status.skipped.as_deref(), Some("origin/HEAD~99"));

    // A dead prefixed spelling is skipped under its bare name.
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

    // A pick blob with control bytes is no pick, so it can't smuggle escapes into the header.
    r.write_raw_base_pick("dev\u{1b}]0;pwned\u{7}");
    assert_eq!(read_base_pick(r.path()).unwrap(), None);

    // An expression is a spelling: skipped when it does not resolve, never discarded.
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

    // A deleted pick is kept and skipped, and the default wins.
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
    r.git(&["branch", "-m", "main", "trunk"]); // no `main`/`master`: no default to fall back on
    write_base_pick(r.path(), "gone").unwrap();

    // With nothing resolving, the skip still reports.
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
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("main"), "local fallback");
    r.set_origin_default("trunk", "HEAD");
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("trunk"), "origin wins");
}

#[test]
fn without_origin_head_the_default_falls_back_to_the_configured_then_conventional_name() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");

    // No remote: `init.defaultBranch` names the trunk when that branch exists...
    r.git(&["branch", "trunk"]);
    r.git(&["config", "init.defaultBranch", "trunk"]);
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("trunk"));
    // ...and is ignored when it names nothing.
    r.git(&["config", "init.defaultBranch", "nope"]);
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("main"));
    r.git(&["config", "init.defaultBranch", "no-such-default"]);

    // `main` before `master`, `master` alone, then nothing.
    r.git(&["branch", "master"]);
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("main"));
    r.git(&["checkout", "-q", "master"]);
    r.git(&["branch", "-D", "main"]);
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("master"));
    r.git(&["branch", "-m", "master", "other"]);
    assert_eq!(default_branch_name(r.path()).unwrap(), None);

    // The name must spell a ref exactly, case included.
    r.git(&["branch", "-m", "other", "Main"]);
    assert_eq!(default_branch_name(r.path()).unwrap(), None);
    r.git(&["branch", "-m", "Main", "other"]);
    r.git(&["branch", "-m", "other", "main/foo"]);
    assert_eq!(default_branch_name(r.path()).unwrap(), None, "a pattern prefix is no match");
    r.git(&["branch", "-m", "main/foo", "other"]);

    // A fallback name qualifies through the origin-then-local lookup.
    let oid = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["update-ref", "refs/remotes/origin/main", &oid]);
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("main"));
    r.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/pruned"]);
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("main"));
    let winner = resolve_base(r.path(), None).unwrap().status.winner.unwrap();
    assert_eq!((winner.name(), winner.oid()), ("main", oid.as_str()));
}

#[test]
fn a_dangling_origin_head_symref_names_no_default() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.git(&["branch", "-m", "main", "trunk"]); // no `main`/`master`: no default to fall back on
    r.set_origin_default("master", "HEAD");

    // A dangling `origin/HEAD` names no default.
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

    // A plain-ref `origin/HEAD` names the origin tip at its commit.
    r.git(&["update-ref", "refs/remotes/origin/HEAD", &oid]);
    assert_eq!(default_branch_name(r.path()).unwrap().as_deref(), Some("trunk"));
}

fn names(rows: &[herdr_reviewr::git::BranchRow]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

#[test]
fn list_branches_merges_names_newest_first_and_lists_the_checked_out() {
    let r = Repo::init();
    r.write("a.rs", "1\n");
    r.git(&["add", "-A"]);
    r.git_env(&["commit", "-q", "-m", "one"], &[("GIT_COMMITTER_DATE", "2026-01-01T00:00:00")]);
    r.git(&["branch", "older"]);
    // Distinct commit dates, so the order never rests on git's tie-break.
    r.write("a.rs", "1b\n");
    r.git(&["add", "-A"]);
    r.git_env(&["commit", "-q", "-m", "middle"], &[("GIT_COMMITTER_DATE", "2026-02-01T00:00:00")]);
    r.set_origin_default("main", "HEAD");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("a.rs", "2\n");
    r.commit_all("two");
    r.git(&["branch", "newer"]);

    // Local and origin names merge, newest first, the checked-out branch included.
    let rows = list_branches(r.path()).unwrap();
    assert_eq!(names(&rows), ["feature", "newer", "main", "older"]);
    let by_name = |n: &str| rows.iter().find(|r| r.name == n).unwrap().tip_secs;
    assert!(by_name("feature") >= by_name("newer"), "same tip, listed in ref order");
    assert!(by_name("main") > by_name("older"), "the tip's committer date is the age source");
    let committed: u64 = r.git(&["log", "-1", "--format=%ct", "older"]).trim().parse().unwrap();
    assert_eq!(by_name("older"), committed, "the tip's committer date, as git reports it");

    // A name on both sides keeps origin's tip: the one the chain resolves it to.
    r.git(&["branch", "-f", "main", "HEAD"]); // local main moves to the newest commit
    let rows = list_branches(r.path()).unwrap();
    assert_eq!(names(&rows), ["feature", "newer", "main", "older"]);
    assert_eq!(rows.iter().find(|r| r.name == "main").unwrap().tip_secs, by_name("main"));
    assert_eq!(checked_out_branch(r.path()).unwrap().as_deref(), Some("feature"));
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
    assert_eq!(names(&list_branches(r.path()).unwrap()), ["main", "dev"]);
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
    // HEAD on the base: `branch` shows the worktree's changes.
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.write("base.rs", "1\nchanged\n"); // uncommitted edit to a tracked file

    let branch = changed_files(r.path(), Scope::Branch, Some("main")).unwrap();
    assert!(branch.iter().any(|f| f.path == "base.rs"), "branch is not empty at the base");
}

#[test]
fn branch_scope_propagates_a_failed_merge_base_query() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");

    let err = merge_base_checked(r.path(), "not-a-commit").unwrap_err();
    assert!(err.to_string().contains("git merge-base failed"), "{err:#}");
}

#[test]
fn branch_scope_is_empty_when_histories_have_no_common_ancestor() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    let base = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "--orphan", "island"]);
    r.git(&["commit", "-q", "--allow-empty", "-m", "island"]);

    assert_eq!(merge_base_checked(r.path(), &base).unwrap(), None);
}

#[test]
fn ignored_paths_never_enter_changes() {
    let r = Repo::init();
    r.write(".gitignore", "ignored/\nbuild/\n");
    r.commit_all("init");
    r.write("ignored/note.md", "scratch\n");
    r.write("build/out.o", "junk\n");

    // An ignored path is no change in any scope.
    let has_ignored = |files: &[ChangedFile]| {
        files.iter().any(|f| f.path.starts_with("ignored/") || f.path.starts_with("build/"))
    };
    assert!(
        !has_ignored(&changed_files(r.path(), Scope::Uncommitted, None).unwrap()),
        "uncommitted"
    );
    assert!(!has_ignored(&changed_files(r.path(), Scope::Branch, Some("main")).unwrap()), "branch");

    // In `last-turn` too: both snapshots honor .gitignore.
    let base = snapshot_worktree(r.path()).unwrap();
    r.write("ignored/note.md", "scratch v2\n");
    assert!(!has_ignored(&changed_against_tree(r.path(), &base).unwrap()), "last-turn");
}

#[test]
fn branch_scope_is_empty_without_a_recorded_base() {
    let r = Repo::init();
    r.write("base.rs", "1\n");
    r.commit_all("base");
    r.git(&["branch", "-m", "main", "trunk"]); // no `main`/`master`: no default to fall back on
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("feature.rs", "x\n");
    r.commit_all("feature work");

    // No base: the scope lists nothing rather than guessing.
    let files = changed_files(r.path(), Scope::Branch, None).unwrap();
    assert!(files.is_empty(), "no source resolves, so the scope shows nothing");

    // A pick of `trunk` brings the scope back.
    write_base_pick(r.path(), "trunk").unwrap();
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

/// An untracked link to a device has no lines: counting would read it without end.
#[cfg(unix)]
#[test]
fn an_untracked_link_to_a_device_lists_without_reading_it() {
    let r = Repo::init();
    r.write("a.txt", "a\n");
    r.commit_all("init");
    std::os::unix::fs::symlink("/dev/zero", r.path().join("zero")).unwrap();
    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let zero = files.iter().find(|f| f.path == "zero").expect("the link lists");
    assert_eq!((zero.kind, zero.additions), (ChangeKind::Untracked, 0));
    // A link to a FIFO would block the open itself. git lists the link, never a bare FIFO.
    let elsewhere = tempfile::tempdir().unwrap();
    let fifo = elsewhere.path().join("pipe");
    assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    std::os::unix::fs::symlink(&fifo, r.path().join("pipe")).unwrap();
    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    assert!(files.iter().any(|f| f.path == "pipe" && f.additions == 0), "{files:?}");
}

/// Past git's default threshold an untracked file is binary, unread; sparse files.
#[cfg(unix)]
#[test]
fn an_untracked_file_past_the_big_file_threshold_is_binary() {
    let r = Repo::init();
    r.write("a.txt", "a\n");
    r.commit_all("init");
    for (name, len) in [("at.txt", 512 << 20), ("past.txt", (512 << 20) + 1)] {
        let file = std::fs::File::create(r.path().join(name)).unwrap();
        std::io::Write::write_all(&mut &file, &b"a\n".repeat(4096)).unwrap();
        file.set_len(len).unwrap();
    }
    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let verdict = |path: &str| {
        let f = files.iter().find(|f| f.path == path).unwrap();
        (f.binary, f.additions)
    };
    assert_eq!(verdict("at.txt"), (false, 4097));
    assert_eq!(verdict("past.txt"), (true, 0));
}

/// A copy a killed process left behind holds no lock, and the sweep removes it; a live one stays.
#[test]
fn a_dead_processs_index_copy_is_swept() {
    let r = Repo::init();
    let home = r.path().join(".git/reviewr");
    std::fs::create_dir_all(&home).unwrap();
    let copy = || tempfile::Builder::new().prefix("index-").tempdir_in(&home).unwrap();
    let dead = copy().keep();
    std::fs::write(dead.join("lock"), "").unwrap();
    std::fs::write(dead.join("index"), "stale").unwrap();
    let live = copy();
    let held = std::fs::File::create(live.path().join("lock")).unwrap();
    held.lock().unwrap();
    // A copy still being made has no lock yet, and is young.
    let making = copy();
    std::fs::write(making.path().join("index"), "seeding").unwrap();
    herdr_reviewr::git::sweep_dead_copies(r.path());
    assert!(!dead.exists(), "{} survived", dead.display());
    assert!(live.path().exists(), "a live copy was swept");
    assert!(making.path().exists(), "a copy being made was swept");
}

#[test]
fn index_copies_live_in_the_git_dir_and_a_seeded_snapshot_is_the_worktree() {
    let r = Repo::init();
    r.write("a.txt", "one\n");
    r.write("b.txt", "same\n");
    r.commit_all("init");
    changed_from(r.path(), "HEAD").unwrap();
    // Touched without a change, then one file edited and one added.
    std::fs::File::options()
        .write(true)
        .open(r.path().join("b.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
    r.write("a.txt", "two\n");
    r.write("c.txt", "new\n");
    let snapshot = herdr_reviewr::git::snapshot_worktree(r.path()).unwrap();

    let home = r.path().join(".git/reviewr");
    let copies = std::fs::read_dir(&home).unwrap().flatten();
    assert!(copies.into_iter().any(|e| e.file_name().to_string_lossy().starts_with("index-")));
    // git's own tree of the same worktree, from a throwaway index.
    let scratch = tempfile::tempdir().unwrap();
    let own = scratch.path().join("index");
    let env = [("GIT_INDEX_FILE", own.to_str().unwrap())];
    r.git_env(&["add", "-A"], &env);
    assert_eq!(snapshot, r.git_env(&["write-tree"], &env).trim());
    // Quitting takes the session copies with it.
    herdr_reviewr::git::end_sessions();
    let left = std::fs::read_dir(&home).unwrap().flatten();
    let left: Vec<_> = left.map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_worktree_re_added_at_its_path_lists_against_its_own_index() {
    let r = Repo::init();
    r.write("a.txt", "one\n");
    r.commit_all("init");
    let out = tempfile::tempdir().unwrap();
    let (first, other) = (out.path().join("a/wt"), out.path().join("b/wt"));
    let add = |branch: &str, at: &Path| {
        r.git(&["worktree", "add", "-q", "-b", branch, at.to_str().unwrap()]);
    };
    add("one", &first);
    assert!(changed_from(&first, "HEAD").unwrap().is_empty());
    r.git(&["worktree", "remove", "--force", first.to_str().unwrap()]);
    // The other worktree takes the freed admin dir and stages a change there.
    add("two", &other);
    git_in(&other, &["rm", "-q", "--cached", "a.txt"]);
    add("three", &first);
    let listed = changed_from(&first, "HEAD").unwrap();
    assert!(listed.is_empty(), "{listed:?}");
    // Re-added under the same admin name, its copy went with the old admin dir.
    r.git(&["worktree", "remove", "--force", first.to_str().unwrap()]);
    add("four", &first);
    assert!(changed_from(&first, "HEAD").unwrap().is_empty());
    // A pruned admin dir stays gone: reviewr never builds one back to hold its copy.
    let out = std::process::Command::new("git")
        .current_dir(&first)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .unwrap();
    let admin = std::path::PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    std::fs::remove_dir_all(&admin).unwrap();
    let _ = changed_from(&first, "HEAD");
    assert!(!admin.exists(), "reviewr re-created {}", admin.display());
}

/// Run git in `dir`, asserting success.
fn git_in(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git").current_dir(dir).args(args).status().unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn a_split_index_gets_no_new_shared_index_from_reviewr() {
    let r = Repo::init();
    r.git(&["config", "core.splitIndex", "true"]);
    for name in ["a.txt", "b.txt", "c.txt"] {
        r.write(name, "one\n");
    }
    r.commit_all("init");
    let shared = || -> Vec<String> {
        let names = std::fs::read_dir(r.path().join(".git")).unwrap().flatten();
        let mut names: Vec<String> = names
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("sharedindex."))
            .collect();
        names.sort();
        names
    };
    let before = shared();
    // Touched and edited, so every refresh has stat info to write.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    r.write("a.txt", "one\n");
    r.write("b.txt", "two\n");
    changed_from(r.path(), "HEAD").unwrap();
    herdr_reviewr::git::snapshot_worktree(r.path()).unwrap();
    changed_from(r.path(), "HEAD").unwrap();
    assert_eq!(shared(), before, "reviewr wrote a shared index into .git");
}

#[test]
fn a_turn_snapshot_holds_through_a_merge_conflict() {
    // With the session copy already made by a listing, and with the snapshot making it.
    for listed_first in [true, false] {
        let r = Repo::init();
        r.write("f.txt", "base\n");
        r.commit_all("init");
        r.git(&["checkout", "-q", "-b", "side"]);
        r.write("f.txt", "side\n");
        r.commit_all("side");
        r.git(&["checkout", "-q", "main"]);
        r.write("f.txt", "main\n");
        r.commit_all("main");
        // The merge stops on the conflict, leaving `f.txt` unmerged in the index.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(r.path())
            .args(["merge", "-q", "side"])
            .output();
        if listed_first {
            changed_from(r.path(), "HEAD").unwrap();
        }
        let snapshot = herdr_reviewr::git::snapshot_worktree(r.path());
        let snapshot = snapshot.expect("an agent mid-merge stops turn tracking");
        // The tree holds the worktree's conflict-marked file, as git's own `add -A` records it.
        let scratch = tempfile::tempdir().unwrap();
        let own = scratch.path().join("index");
        let env = [("GIT_INDEX_FILE", own.to_str().unwrap())];
        r.git_env(&["add", "-A"], &env);
        assert_eq!(snapshot, r.git_env(&["write-tree"], &env).trim(), "{listed_first}");
    }
}

#[test]
fn every_changed_file_carries_the_size_of_each_side_git_stores() {
    let r = Repo::init();
    // Windows forbids a newline, and a colon, in a file name.
    let odd = if cfg!(unix) { "line\nbreak: odd.txt" } else { "odd name.txt" };
    r.write("a.txt", "four\n");
    r.write(odd, "seven\n");
    r.commit_all("init");
    r.write("a.txt", "twelve bytes\n");
    r.write(odd, "ten bytes\n");
    r.commit_all("edit");

    // Tree to tree, sized by id, so a newline in a path sizes like any other.
    let between = changed_between(r.path(), "HEAD~1", "HEAD").unwrap();
    let sizes: Vec<_> = between.iter().map(|f| (f.path.as_str(), f.old_size, f.new_size)).collect();
    assert_eq!(sizes, [("a.txt", 5, Some(13)), (odd, 6, Some(10))]);
    // Against the worktree, the new side is the file itself, sized when it is read.
    r.write("a.txt", "x\n");
    let from = changed_from(r.path(), "HEAD").unwrap();
    let sizes: Vec<_> = from.iter().map(|f| (f.path.as_str(), f.old_size, f.new_size)).collect();
    assert_eq!(sizes, [("a.txt", 13, None)]);
}

#[test]
fn a_copy_is_reported_as_a_copy_and_reads_its_source() {
    let r = Repo::init();
    r.git(&["config", "diff.renames", "copies"]);
    r.write("orig.rs", "one\ntwo\nthree\nfour\nfive\n");
    r.commit_all("init");
    r.write("orig.rs", "one\ntwo\nthree\nfour\nfive\nsix\n");
    r.write("copy.rs", "one\ntwo\nTHREE\nfour\nfive\n");
    r.git(&["add", "-A"]);

    let files = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let copy = files.iter().find(|f| f.path == "copy.rs").expect("the copy");
    assert_eq!((copy.kind, copy.previous_path.as_deref()), (ChangeKind::Copied, Some("orig.rs")));
    // The source edited in its own right stays out of the copy's sides.
    let sides = diff_sides(r.path(), "HEAD", None, "copy.rs", Some("orig.rs")).unwrap();
    let want = DiffSides::Text {
        old: "one\ntwo\nthree\nfour\nfive\n".into(),
        new: "one\ntwo\nTHREE\nfour\nfive\n".into(),
    };
    assert_eq!(sides, want);
}

#[test]
fn a_directory_removing_rename_keeps_its_stats() {
    // `a/b/f.rs -> a/f.rs` once keyed as `a//f.rs`.
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
    // A new directory lists each file, not one `dir/` entry.
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
fn an_untracked_files_count_follows_its_edits() {
    let r = Repo::init();
    r.write("seed.rs", "x\n");
    r.commit_all("init");
    let count = || {
        let files = changed_from(r.path(), "HEAD").unwrap();
        files.iter().find(|f| f.path == "notes.txt").map(|f| f.additions)
    };
    r.write("notes.txt", "a\nb\n");
    assert_eq!(count(), Some(2));
    // Rewritten at the same size within its mtime's tick: never the remembered count.
    let at = r.path().join("notes.txt");
    let stamp = std::fs::metadata(&at).unwrap().modified().unwrap();
    r.write("notes.txt", "abc\n");
    std::fs::File::options().write(true).open(&at).unwrap().set_modified(stamp).unwrap();
    assert_eq!(count(), Some(1), "a fresh file's count was remembered");
    r.write("notes.txt", "a\nb\nc\n");
    assert_eq!(count(), Some(3));
    // A settled file is counted once, then still matches after its edit.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30);
    std::fs::File::options().write(true).open(&at).unwrap().set_modified(old).unwrap();
    assert_eq!(count(), Some(3));
    r.write("notes.txt", "a\nb\nc\nd\n");
    assert_eq!(count(), Some(4));
    // Settled, then rewritten at its size with the old mtime put back, as `cp -p` does.
    std::fs::File::options().write(true).open(&at).unwrap().set_modified(old).unwrap();
    assert_eq!(count(), Some(4));
    r.write("notes.txt", "abcdefg\n");
    std::fs::File::options().write(true).open(&at).unwrap().set_modified(old).unwrap();
    // Git keys on ctime too, which unix keeps and no write can set back.
    if cfg!(unix) {
        assert_eq!(count(), Some(1), "a restored mtime hid the rewrite");
    }
}

#[test]
fn a_repo_with_no_commits_lists_untracked_without_erroring() {
    // No commits: diff against the empty tree.
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
    let _ = diff_sides(r.path(), "HEAD", None, "a.rs", None).unwrap();
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

    // The turn edits, creates, and deletes; the old untracked file stays put.
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
    // A turn that only creates a file is a change, which promotion relies on.
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let base = snapshot_worktree(r.path()).unwrap();
    r.write("fresh.rs", "x\n");
    let files = changed_against_tree(r.path(), &base).unwrap();
    assert_eq!(by_path(&files)["fresh.rs"].kind, ChangeKind::Added);
}

#[test]
fn a_snapshot_sees_a_same_size_edit_made_in_the_index_writes_own_tick() {
    // A same-size rewrite in the index's own tick: only the index mtime makes git compare content.
    let r = Repo::init();
    r.git(&["config", "core.trustctime", "false"]);
    let tick = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let set_mtime = |rel: &str| {
        let file = std::fs::File::options().write(true).open(r.path().join(rel)).unwrap();
        file.set_modified(tick).unwrap();
    };
    r.write("a.txt", "one\n");
    set_mtime("a.txt");
    r.commit_all("init");
    let base = snapshot_worktree(r.path()).unwrap();
    r.write("a.txt", "ONE\n");
    set_mtime("a.txt");
    set_mtime(".git/index");
    assert_eq!(r.git(&["diff", "--name-only", "HEAD"]), "a.txt\n", "git sees the edit");

    let files = changed_against_tree(r.path(), &base).unwrap();
    assert_eq!(by_path(&files)["a.txt"].kind, ChangeKind::Modified);
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
    assert!(!git_dir.join("reviewr-turn-index").exists(), "no index lands in the git dir");
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
    r.git(&["branch", "dev"]);
    let linked = r.add_worktree("feature");

    write_base_pick(r.path(), "dev").unwrap();
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("dev"));
    assert_eq!(read_base_pick(linked.path()).unwrap(), None, "main → linked");

    write_base_pick(linked.path(), "dev").unwrap();
    assert_eq!(read_base_pick(linked.path()).unwrap().as_deref(), Some("dev"));
    assert_eq!(read_base_pick(r.path()).unwrap().as_deref(), Some("dev"));

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

// --- commits scope ---------------------------------------

/// `main` with four commits and `feature` with one; returns `main`'s shas, root first.
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
    // The first-parent walk: a side branch's commit is no row of its own.
    let subjects: Vec<&str> = rows.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["merge side", "three", "two", "one", "root"]);
}

#[test]
fn a_shallow_cut_is_gone_not_a_root() {
    use herdr_reviewr::git::{EMPTY_TREE, commit_exists, parent_or_empty};
    let (r, shas) = run_repo();
    let shallow = tempfile::tempdir().unwrap();
    // `file:///C:/…` on Windows, `file:///…` elsewhere.
    let path = r.path().to_string_lossy().replace('\\', "/");
    let url =
        if path.starts_with('/') { format!("file://{path}") } else { format!("file:///{path}") };
    let out = std::process::Command::new("git")
        .args(["clone", "-q", "--depth", "1", &url, "w"])
        .current_dir(shallow.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let w = shallow.path().join("w");
    // A shallow clone's missing parent reads `gone`, never the empty tree.
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

/// Reading never writes the repository, a stat-dirty file included.
#[test]
fn reading_a_touched_file_never_rewrites_the_index() {
    let r = Repo::init();
    r.write("a.txt", "one\n");
    r.write("run.sh", "x\n");
    r.write("b.bin", "\0\u{1}binary\n");
    r.commit_all("init");
    let index = r.path().join(".git/index");
    let stamp = || std::fs::metadata(&index).unwrap().modified().unwrap();
    // A later mtime on unchanged content, past the index's own stamp.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    r.write("a.txt", "one\n");
    r.write("b.bin", "\0\u{1}binary\n");
    // A real mode change still lists, as an empty change.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = r.path().join("run.sh");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let before = stamp();

    let changed = changed_files(r.path(), Scope::Uncommitted, None).unwrap();
    let paths: Vec<&str> = changed.iter().map(|f| f.path.as_str()).collect();
    let expected: &[&str] = if cfg!(unix) { &["run.sh"] } else { &[] };
    assert_eq!(paths, expected, "a touched file with the same content is no change");
    diff_sides(r.path(), "HEAD", None, "a.txt", None).unwrap();
    snapshot_worktree(r.path()).unwrap();

    assert_eq!(stamp(), before, ".git/index was rewritten");
    // reviewr's only files in .git are its index copies, in their own dir.
    let ours: Vec<_> = std::fs::read_dir(r.path().join(".git"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("reviewr"))
        .collect();
    assert_eq!(ours, ["reviewr"], "reviewr left {ours:?} in .git");
    let copies = std::fs::read_dir(r.path().join(".git/reviewr")).unwrap().flatten();
    assert!(copies.into_iter().all(|e| e.file_name().to_string_lossy().starts_with("index-")));
}

/// A file ignored since the last snapshot leaves the next one: `add -A` never unstages.
#[test]
fn a_file_ignored_since_the_last_snapshot_leaves_the_next() {
    let r = Repo::init();
    r.write("a.txt", "a\n");
    r.commit_all("init");
    r.write("build.out", "x\n");
    let listed = |tree: &str| r.git(&["ls-tree", "--name-only", tree]);
    assert!(listed(&snapshot_worktree(r.path()).unwrap()).contains("build.out"));
    r.write(".gitignore", "build.out\n");
    assert!(!listed(&snapshot_worktree(r.path()).unwrap()).contains("build.out"));
}

/// An index rewritten in the same mtime tick, at the same size, is still a new index.
#[test]
fn an_index_rewritten_within_one_tick_is_read_again() {
    let r = Repo::init();
    r.write("a.txt", "one\ntwo\nthree\n");
    r.commit_all("init");
    r.git(&["mv", "a.txt", "y.txt"]);
    let index = r.path().join(".git/index");
    let stamp = std::fs::metadata(&index).unwrap().modified().unwrap();
    let names = |files: Vec<ChangedFile>| files.into_iter().map(|f| f.path).collect::<Vec<_>>();
    assert_eq!(names(changed_files(r.path(), Scope::Uncommitted, None).unwrap()), ["y.txt"]);
    r.git(&["mv", "y.txt", "z.txt"]);
    let file = std::fs::File::options().write(true).open(&index).unwrap();
    file.set_modified(stamp).unwrap();
    assert_eq!(names(changed_files(r.path(), Scope::Uncommitted, None).unwrap()), ["z.txt"]);
}

/// Concurrent snapshots of one worktree all land the same tree.
#[test]
fn concurrent_snapshots_of_one_worktree_all_land_the_same_tree() {
    let r = Repo::init();
    for i in 0..50 {
        r.write(&format!("f{i}.txt"), &format!("{i}\n"));
    }
    r.commit_all("init");
    r.write("f0.txt", "edited\n");
    let path = r.path_buf();
    let snap = |path: std::path::PathBuf| {
        std::thread::spawn(move || (0..20).map(|_| snapshot_worktree(&path)).collect::<Vec<_>>())
    };
    let (a, b) = (snap(path.clone()), snap(path.clone()));
    let trees: Vec<String> = a
        .join()
        .unwrap()
        .into_iter()
        .chain(b.join().unwrap())
        .map(|tree| tree.expect("a snapshot failed"))
        .collect();
    assert!(trees.windows(2).all(|w| w[0] == w[1]), "snapshots disagree: {trees:?}");
}
