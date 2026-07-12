//! Integration tests for the `comment` and `skill-path` CLI subcommands, spawning the real
//! binary against a real git repo (`tests/common.rs`).

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::Repo;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-reviewr")
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin()).args(args).current_dir(dir).output().expect("spawn herdr-reviewr")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn add(dir: &Path, extra: &[&str]) -> String {
    let mut args = vec!["comment", "add"];
    args.extend_from_slice(extra);
    let out = run(dir, &args);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    stdout(&out).trim().to_string()
}

#[test]
fn add_prints_an_id_and_list_shows_it_human_and_json() {
    let r = Repo::init();

    let id = add(r.path(), &["--file", "src/a.rs", "--start", "3", "--text", "why not a const?"]);
    assert!(id.starts_with("c-"), "add stdout should be the new id, got {id:?}");

    let human = run(r.path(), &["comment", "list"]);
    assert!(human.status.success());
    let human_out = stdout(&human);
    assert!(human_out.contains(&id), "human list contains the id: {human_out}");
    assert!(human_out.contains("open"), "human list shows status: {human_out}");
    assert!(human_out.contains("agent"), "human list shows default author: {human_out}");
    assert!(human_out.contains("src/a.rs:3-3"), "human list shows file:start-end: {human_out}");
    assert!(human_out.contains("why not a const?"), "human list shows first line: {human_out}");

    let json = run(r.path(), &["comment", "list", "--json"]);
    assert!(json.status.success());
    let docs: serde_json::Value = serde_json::from_str(&stdout(&json)).expect("valid json");
    let docs = docs.as_array().expect("a json array");
    assert_eq!(docs.len(), 1);
    let doc = &docs[0];
    assert_eq!(doc["id"], id);
    assert_eq!(doc["author"], "agent");
    assert_eq!(doc["status"], "open");
    assert_eq!(doc["file"], "src/a.rs");
    assert_eq!(doc["side"], "new");
    assert_eq!(doc["start"], 3);
    assert_eq!(doc["end"], 3);
    assert_eq!(doc["text"], "why not a const?");
}

#[test]
fn add_defaults_end_to_start_side_to_new_and_author_to_agent() {
    let r = Repo::init();
    let id = add(r.path(), &["--file", "f.rs", "--start", "5", "--text", "t"]);

    let json = run(r.path(), &["comment", "list", "--json"]);
    let docs: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    let doc = &docs.as_array().unwrap()[0];
    assert_eq!(doc["id"], id);
    assert_eq!(doc["end"], 5, "end defaults to start");
    assert_eq!(doc["side"], "new", "side defaults to new");
    assert_eq!(doc["author"], "agent", "author defaults to agent");
    assert_eq!(doc["lines"], "", "lines default to empty");
}

#[test]
fn add_honors_explicit_end_side_author_and_lines() {
    let r = Repo::init();
    let id = add(
        r.path(),
        &[
            "--file",
            "f.rs",
            "--start",
            "5",
            "--end",
            "9",
            "--side",
            "old",
            "--author",
            "user",
            "--lines",
            "-old line",
            "--text",
            "t",
        ],
    );

    let json = run(r.path(), &["comment", "list", "--all", "--json"]);
    let docs: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    let doc = &docs.as_array().unwrap()[0];
    assert_eq!(doc["id"], id);
    assert_eq!(doc["end"], 9);
    assert_eq!(doc["side"], "old");
    assert_eq!(doc["author"], "user");
    assert_eq!(doc["lines"], "-old line");
}

#[test]
fn list_defaults_to_open_only_and_all_includes_resolved() {
    let r = Repo::init();
    let open_id = add(r.path(), &["--file", "a.rs", "--start", "1", "--text", "open one"]);
    let resolved_id = add(r.path(), &["--file", "b.rs", "--start", "2", "--text", "resolved one"]);

    let resolve_out = run(r.path(), &["comment", "resolve", &resolved_id]);
    assert!(resolve_out.status.success(), "resolve failed: {}", stderr(&resolve_out));

    let default_list = stdout(&run(r.path(), &["comment", "list"]));
    assert!(default_list.contains(&open_id), "default list shows the open comment");
    assert!(!default_list.contains(&resolved_id), "default list hides the resolved comment");

    let all_list = stdout(&run(r.path(), &["comment", "list", "--all"]));
    assert!(all_list.contains(&open_id));
    assert!(all_list.contains(&resolved_id), "--all shows the resolved comment too");

    let all_json = run(r.path(), &["comment", "list", "--all", "--json"]);
    let docs: serde_json::Value = serde_json::from_str(&stdout(&all_json)).unwrap();
    let docs = docs.as_array().unwrap();
    let resolved_doc = docs.iter().find(|d| d["id"] == resolved_id).expect("resolved doc present");
    assert_eq!(
        resolved_doc["status"], "resolved",
        "resolve flips status, visible via --all --json"
    );
}

#[test]
fn rm_deletes_a_comment() {
    let r = Repo::init();
    let id = add(r.path(), &["--file", "a.rs", "--start", "1", "--text", "gone soon"]);

    let rm_out = run(r.path(), &["comment", "rm", &id]);
    assert!(rm_out.status.success(), "rm failed: {}", stderr(&rm_out));

    let all_list = stdout(&run(r.path(), &["comment", "list", "--all"]));
    assert!(!all_list.contains(&id), "rm removes the comment entirely: {all_list}");
}

#[test]
fn resolve_on_unknown_id_exits_1_naming_the_id() {
    let r = Repo::init();
    let out = run(r.path(), &["comment", "resolve", "c-no-such-id"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("c-no-such-id"), "stderr names the id: {}", stderr(&out));
}

#[test]
fn rm_on_unknown_id_exits_1_naming_the_id() {
    let r = Repo::init();
    let out = run(r.path(), &["comment", "rm", "c-no-such-id"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("c-no-such-id"), "stderr names the id: {}", stderr(&out));
}

#[test]
fn any_comment_command_outside_a_git_repo_exits_1_with_one_stderr_line() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["comment", "list"],
        vec!["comment", "add", "--file", "a.rs", "--start", "1", "--text", "t"],
        vec!["comment", "resolve", "c-1"],
        vec!["comment", "rm", "c-1"],
    ] {
        let out = run(dir.path(), &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert_eq!(err.lines().count(), 1, "{args:?}: exactly one stderr line, got {err:?}");
    }
}

#[test]
fn add_with_a_missing_text_exits_2_with_usage_on_stderr() {
    let r = Repo::init();
    let out = run(r.path(), &["comment", "add", "--file", "a.rs", "--start", "1"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "usage printed on stderr: {}", stderr(&out));
}

#[test]
fn add_with_an_unknown_flag_exits_2_with_usage_on_stderr() {
    let r = Repo::init();
    let out = run(
        r.path(),
        &["comment", "add", "--file", "a.rs", "--start", "1", "--text", "t", "--bogus", "x"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "usage printed on stderr: {}", stderr(&out));
}

#[test]
fn add_rejects_zero_start() {
    let r = Repo::init();
    let out = run(r.path(), &["comment", "add", "--file", "a.rs", "--start", "0", "--text", "t"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("reviewr: --start must be >= 1"),
        "stderr names the problem: {}",
        stderr(&out)
    );
}

#[test]
fn add_rejects_end_before_start() {
    let r = Repo::init();
    let out = run(
        r.path(),
        &["comment", "add", "--file", "a.rs", "--start", "5", "--end", "3", "--text", "t"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("reviewr: --end must be >= --start"),
        "stderr names the problem: {}",
        stderr(&out)
    );
}

#[test]
fn skill_path_finds_the_dev_checkout_from_the_repo_root() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out = run(Path::new(manifest_dir), &["skill-path"]);
    assert!(out.status.success(), "skill-path failed: {}", stderr(&out));
    let path = stdout(&out).trim().to_string();
    assert!(
        path.ends_with("skills/reviewr-comments/SKILL.md")
            || path.ends_with("skills\\reviewr-comments\\SKILL.md"),
        "path is the skill file: {path}"
    );
}

#[test]
fn skill_path_exits_1_naming_both_candidates_when_neither_exists() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["skill-path"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("SKILL.md"), "names the missing file: {err}");
}

/// `skill-path`'s dev-checkout fallback is cwd-relative, so `skill-install` must be driven
/// with cwd = the repo root for the source to resolve; `--target` is the seam that redirects
/// the destination into a tempdir instead of touching the real `~/.claude/skills`.
fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn skill_install(target: &Path, extra: &[&str]) -> Output {
    let target_str = target.display().to_string();
    let mut args = vec!["skill-install", "--target", target_str.as_str()];
    args.extend_from_slice(extra);
    run(manifest_dir(), &args)
}

#[test]
fn skill_install_creates_the_file_with_installed_hint() {
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("SKILL.md");

    let out = skill_install(target.path(), &[]);
    assert!(out.status.success(), "skill-install failed: {}", stderr(&out));
    let out_stdout = stdout(&out);
    assert!(out_stdout.contains("installed:"), "prints installed path: {out_stdout}");
    assert!(
        out_stdout.contains("herdr-reviewr comment list"),
        "prints the CLAUDE.md hint: {out_stdout}"
    );

    #[cfg(unix)]
    {
        let meta = std::fs::symlink_metadata(&dest).expect("dest exists");
        assert!(meta.file_type().is_symlink(), "default install is a symlink");
    }
    #[cfg(windows)]
    {
        assert!(dest.exists(), "dest exists");
        let source = stdout(&run(manifest_dir(), &["skill-path"])).trim().to_string();
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            std::fs::read(&source).unwrap(),
            "copied content matches source"
        );
    }
}

#[test]
fn skill_install_twice_is_idempotent() {
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("SKILL.md");

    let first = skill_install(target.path(), &[]);
    assert!(first.status.success());
    let before = std::fs::symlink_metadata(&dest).unwrap().modified().ok();

    let second = skill_install(target.path(), &[]);
    assert!(second.status.success());
    assert!(
        stdout(&second).contains("already installed"),
        "second run reports already installed: {}",
        stdout(&second)
    );
    let after = std::fs::symlink_metadata(&dest).unwrap().modified().ok();
    assert_eq!(before, after, "file is unchanged by the second run");
}

#[test]
fn skill_install_refuses_to_clobber_a_conflicting_file_without_force() {
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("SKILL.md");
    std::fs::create_dir_all(target.path()).unwrap();
    std::fs::write(&dest, "not the skill").unwrap();

    let out = skill_install(target.path(), &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains(&dest.display().to_string()),
        "stderr names the conflicting path: {}",
        stderr(&out)
    );
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "not the skill", "file left untouched");

    let forced = skill_install(target.path(), &["--force"]);
    assert!(forced.status.success(), "--force replaces: {}", stderr(&forced));
    assert_ne!(
        std::fs::read_to_string(&dest).unwrap(),
        "not the skill",
        "file replaced by --force"
    );
}

#[test]
fn skill_install_copy_forces_a_regular_byte_identical_file() {
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("SKILL.md");

    let out = skill_install(target.path(), &["--copy"]);
    assert!(out.status.success(), "skill-install --copy failed: {}", stderr(&out));

    let meta = std::fs::symlink_metadata(&dest).expect("dest exists");
    assert!(!meta.file_type().is_symlink(), "--copy installs a regular file");

    let source = stdout(&run(manifest_dir(), &["skill-path"])).trim().to_string();
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        std::fs::read(&source).unwrap(),
        "copy is byte-identical to source"
    );
}

#[test]
fn skill_install_with_an_unknown_flag_exits_2_with_usage_on_stderr() {
    let target = tempfile::tempdir().unwrap();
    let out = skill_install(target.path(), &["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "usage printed on stderr: {}", stderr(&out));
}
