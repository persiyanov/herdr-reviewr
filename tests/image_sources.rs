//! Exact image provenance at real git and filesystem boundaries.
mod common;

use common::Repo;
use herdr_reviewr::app::Tab;
#[cfg(unix)]
use herdr_reviewr::git::image_path_is_exact;
use herdr_reviewr::git::{
    blob_bytes, changed_between, changed_from, guarded_worktree_bytes, snapshot_worktree,
};
use herdr_reviewr::model::{ChangeKind, CommitPick, Scope};
use herdr_reviewr::world::{Changeset, WorldInput, build_changed};

fn scope(r: &Repo, kind: Scope, baseline: Option<String>, pick: Option<CommitPick>) -> Changeset {
    build_changed(&WorldInput {
        repo: r.path_buf(),
        tab: Tab::Changes,
        scope: kind,
        base: Some("main".into()),
        base_epoch: 0,
        turn_baseline: baseline,
        commit_pick: pick,
        toggled_dirs: std::collections::HashSet::default(),
    })
    .unwrap()
    .changeset
}

fn bytes(r: &Repo, path: &str, bytes: &[u8]) {
    std::fs::write(r.path().join(path), bytes).unwrap();
}

#[test]
fn exact_bytes_and_landed_endpoints_across_all_scopes() {
    let r = Repo::init();
    let old = b"\x89PNG\r\n\x1a\n\0old\xff";
    let committed = b"\x89PNG\r\n\x1a\n\0committed\xfe";
    let raw = b"\x89PNG\r\n\x1a\n\0raw\xfd";
    bytes(&r, "f.png", old);
    r.commit_all("base");
    let base = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-b", "feature"]);
    bytes(&r, "f.png", committed);
    r.commit_all("feature");
    let head = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    let baseline = snapshot_worktree(r.path()).unwrap();
    bytes(&r, "f.png", raw);
    let index_before = std::fs::read(r.path().join(".git/index")).unwrap();
    for kind in [Scope::Uncommitted, Scope::Branch, Scope::LastTurn, Scope::Commits] {
        let set = scope(&r, kind, Some(baseline.clone()), Some(CommitPick::single(&head)));
        let ends = set.ends.as_ref().unwrap();
        let file = &set.files["f.png"];
        assert!(!file.conflicted);
        assert_eq!(file.old_mode, "100644");
        assert_eq!(file.new_mode, "100644");
        let expected_old: &[u8] = match kind {
            Scope::Branch | Scope::Commits => old,
            _ => committed,
        };
        assert_eq!(blob_bytes(r.path(), &file.old_oid, 1024).unwrap(), expected_old);
        match kind {
            Scope::Uncommitted | Scope::Branch => {
                assert!(ends.new.is_none());
                #[cfg(unix)]
                assert_eq!(guarded_worktree_bytes(r.path(), &file.path, 1024).unwrap(), raw);
                #[cfg(not(unix))]
                assert!(guarded_worktree_bytes(r.path(), &file.path, 1024).is_err());
            }
            Scope::LastTurn | Scope::Commits => {
                assert!(ends.new.is_some());
                let expected: &[u8] = if kind == Scope::Commits { committed } else { raw };
                assert_eq!(blob_bytes(r.path(), &file.new_oid, 1024).unwrap(), expected);
            }
        }
    }
    assert_eq!(std::fs::read(r.path().join(".git/index")).unwrap(), index_before);
    let range = changed_between(r.path(), &base, &head).unwrap();
    assert_eq!(blob_bytes(r.path(), &range[0].old_oid, 1024).unwrap(), old);
    assert_eq!(blob_bytes(r.path(), &range[0].new_oid, 1024).unwrap(), committed);
}

#[test]
fn worktree_endpoint_stays_raw_with_nonzero_index_oid_and_clean_filter() {
    let r = Repo::init();
    r.git(&["config", "filter.upper.clean", "tr a-z A-Z"]);
    r.write(".gitattributes", "*.png filter=upper -text\n");
    bytes(&r, "f.png", b"old\0lower\r\n");
    r.commit_all("base");
    bytes(&r, "f.png", b"new\0lower\r\n");
    r.git(&["add", "f.png"]);
    let rows = changed_from(r.path(), "HEAD").unwrap();
    let file = rows.iter().find(|f| f.path == "f.png").unwrap();
    assert!(file.new_oid.bytes().any(|b| b != b'0'));
    let index_before = std::fs::read(r.path().join(".git/index")).unwrap();
    r.git(&["config", "filter.upper.clean", "false"]);
    assert_eq!(blob_bytes(r.path(), &file.old_oid, 1024).unwrap(), b"OLD\0LOWER\r\n");
    assert_eq!(blob_bytes(r.path(), &file.new_oid, 1024).unwrap(), b"NEW\0LOWER\r\n");
    #[cfg(unix)]
    assert_eq!(guarded_worktree_bytes(r.path(), &file.path, 1024).unwrap(), b"new\0lower\r\n");
    #[cfg(not(unix))]
    assert!(guarded_worktree_bytes(r.path(), &file.path, 1024).is_err());
    assert_eq!(std::fs::read(r.path().join(".git/index")).unwrap(), index_before);
}

#[test]
fn rename_add_delete_keep_original_paths_modes_and_oids() {
    let r = Repo::init();
    bytes(&r, "old.png", b"same\0image");
    bytes(&r, "gone.png", b"gone\0image");
    r.commit_all("base");
    let old = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["mv", "old.png", "new.png"]);
    std::fs::remove_file(r.path().join("gone.png")).unwrap();
    bytes(&r, "added.png", b"added\0image");
    r.commit_all("next");
    let rows = changed_between(r.path(), &old, "HEAD").unwrap();
    let renamed = rows.iter().find(|f| f.path == "new.png").unwrap();
    assert_eq!(renamed.kind, ChangeKind::Renamed);
    assert_eq!(renamed.previous_path.as_deref(), Some("old.png"));
    assert_eq!(blob_bytes(r.path(), &renamed.old_oid, 100).unwrap(), b"same\0image");
    assert_eq!(renamed.old_oid, renamed.new_oid);
    let added = rows.iter().find(|f| f.path == "added.png").unwrap();
    assert_eq!(added.old_mode, "000000");
    assert!(added.old_oid.bytes().all(|b| b == b'0'));
    assert_eq!(blob_bytes(r.path(), &added.new_oid, 100).unwrap(), b"added\0image");
    let gone = rows.iter().find(|f| f.path == "gone.png").unwrap();
    assert_eq!(gone.new_mode, "000000");
    assert!(gone.new_oid.bytes().all(|b| b == b'0'));
    assert_eq!(blob_bytes(r.path(), &gone.old_oid, 100).unwrap(), b"gone\0image");
}

#[test]
fn full_oid_blob_bounds_and_argument_rejection() {
    let r = Repo::init();
    bytes(&r, "f.png", b"\0\xffexact");
    r.commit_all("base");
    let oid = r.git(&["rev-parse", "HEAD:f.png"]).trim().to_string();
    assert_eq!(blob_bytes(r.path(), &oid, 7).unwrap(), b"\0\xffexact");
    assert!(blob_bytes(r.path(), &oid, 6).is_err());
    for invalid in ["HEAD:f.png", "--batch", "--filters", "HEAD", &oid[..12], &"0".repeat(40)] {
        assert!(blob_bytes(r.path(), invalid, 100).is_err(), "{invalid}");
    }
    let tree = r.git(&["rev-parse", "HEAD^{tree}"]).trim().to_string();
    assert!(blob_bytes(r.path(), &tree, 1000).is_err());
}

#[test]
fn empty_blob_respects_zero_byte_cap() {
    let r = Repo::init();
    bytes(&r, "empty.png", b"");
    r.commit_all("empty");
    let oid = r.git(&["rev-parse", "HEAD:empty.png"]).trim().to_string();
    assert!(blob_bytes(r.path(), &oid, 0).unwrap().is_empty());
    #[cfg(unix)]
    assert!(guarded_worktree_bytes(r.path(), "empty.png", 0).unwrap().is_empty());
    #[cfg(not(unix))]
    assert!(guarded_worktree_bytes(r.path(), "empty.png", 0).is_err());
}

#[test]
fn blob_reads_ignore_replacement_objects() {
    let r = Repo::init();
    bytes(&r, "f.png", b"original\0");
    bytes(&r, "other.png", b"replacement\0");
    r.commit_all("base");
    let oid = r.git(&["rev-parse", "HEAD:f.png"]).trim().to_string();
    let replacement = r.git(&["rev-parse", "HEAD:other.png"]).trim().to_string();
    r.git(&["replace", &oid, &replacement]);
    assert_eq!(blob_bytes(r.path(), &oid, 100).unwrap(), b"original\0");
}

#[test]
fn missing_promisor_blob_does_not_fetch_or_write_objects() {
    let r = Repo::init();
    bytes(&r, "f.png", b"promised\0image");
    r.commit_all("base");
    let oid = r.git(&["rev-parse", "HEAD:f.png"]).trim().to_string();
    let remote = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["clone", "--bare"])
            .arg(r.path())
            .arg(remote.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    r.git(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    r.git(&["config", "remote.origin.promisor", "true"]);
    r.git(&["config", "core.repositoryformatversion", "1"]);
    r.git(&["config", "extensions.partialClone", "origin"]);
    let object = r.path().join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    std::fs::remove_file(&object).unwrap();
    let index = std::fs::read(r.path().join(".git/index")).unwrap();
    assert!(blob_bytes(r.path(), &oid, 100).is_err());
    assert!(!object.exists());
    assert_eq!(std::fs::read(r.path().join(".git/index")).unwrap(), index);
    assert!(!r.path().join(".git/FETCH_HEAD").exists());
}

#[cfg(unix)]
#[test]
fn raw_worktree_rejects_links_fifo_devices_bad_paths_and_bytecaps() {
    use std::os::unix::fs::symlink;
    let r = Repo::init();
    r.write("f.png", "exact");
    std::fs::create_dir(r.path().join("dir")).unwrap();
    r.write("dir/f.png", "inside");
    symlink("f.png", r.path().join("link.png")).unwrap();
    symlink("dir", r.path().join("alias")).unwrap();
    symlink("/dev/zero", r.path().join("device.png")).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(r.path().join("fifo.png"))
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(guarded_worktree_bytes(r.path(), "f.png", 5).unwrap(), b"exact");
    assert!(guarded_worktree_bytes(r.path(), "f.png", 4).is_err());
    for path in [
        "link.png",
        "device.png",
        "alias/f.png",
        "fifo.png",
        "dir",
        "",
        "/etc/passwd",
        "../f.png",
        "dir/../f.png",
        "./f.png",
        "dir//f.png",
        "f\0.png",
        "\u{fffd}.png",
    ] {
        assert!(guarded_worktree_bytes(r.path(), path, 100).is_err(), "{path:?}");
    }
    assert!(image_path_is_exact("dir/f.png"));
}

#[cfg(unix)]
#[test]
fn untracked_and_tracked_symlinks_never_claim_regular_modes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let r = Repo::init();
    r.write("base", "base");
    r.commit_all("base");
    r.write("regular.png", "regular");
    r.write("executable.png", "executable");
    std::fs::set_permissions(
        r.path().join("executable.png"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("regular.png", r.path().join("link.png")).unwrap();
    let rows = changed_from(r.path(), "HEAD").unwrap();
    assert_eq!(rows.iter().find(|f| f.path == "link.png").unwrap().new_mode, "120000");
    assert_eq!(rows.iter().find(|f| f.path == "regular.png").unwrap().new_mode, "100644");
    assert_eq!(rows.iter().find(|f| f.path == "executable.png").unwrap().new_mode, "100755");
    r.git(&["add", "link.png"]);
    let rows = changed_from(r.path(), "HEAD").unwrap();
    assert_eq!(rows.iter().find(|f| f.path == "link.png").unwrap().new_mode, "120000");
}

#[cfg(unix)]
#[test]
fn lossy_paths_are_refused_even_when_the_utf8_alias_exists() {
    use std::os::unix::ffi::OsStringExt;
    let r = Repo::init();
    r.write("base", "base");
    r.commit_all("base");
    std::fs::write(r.path().join(std::ffi::OsString::from_vec(b"bad\xff.png".to_vec())), b"wrong")
        .unwrap();
    r.write("bad\u{fffd}.png", "alias");
    let rows = changed_from(r.path(), "HEAD").unwrap();
    let path = &rows.iter().find(|f| f.path.contains('\u{fffd}')).unwrap().path;
    assert!(!image_path_is_exact(path));
    assert!(guarded_worktree_bytes(r.path(), path, 100).is_err());
}

#[test]
fn unmerged_rows_have_an_explicit_conflict_marker() {
    let r = Repo::init();
    r.write("f.png", "base\n");
    r.commit_all("base");
    r.git(&["checkout", "-b", "other"]);
    r.write("f.png", "other\n");
    r.commit_all("other");
    r.git(&["checkout", "main"]);
    r.write("f.png", "main\n");
    r.commit_all("main");
    let merge = std::process::Command::new("git")
        .arg("-C")
        .arg(r.path())
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "merge",
            "other",
        ])
        .output()
        .unwrap();
    assert!(!merge.status.success());
    let rows = changed_from(r.path(), "HEAD").unwrap();
    assert!(rows.iter().find(|f| f.path == "f.png").unwrap().conflicted);
}
