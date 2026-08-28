#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn reviewr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-reviewr")
}

/// A fake herdr, answering in the live 0.7.5 envelope shapes (docs/herdr-api-notes.md).
/// `pane list` serves `panes.json` (else one plain pane), `pane process-info` serves the
/// per-pane `procinfo-<id>.json` (else a plain shell, which is not a reviewr pane) or fails
/// with `procfail-<id>.json` on stderr, `pane close` succeeds unless `closefail-<id>`
/// exists (whose content becomes the failure's stderr), `plugin config-dir` names the
/// fixture dir itself (after a 5s hang when `configdir-hang` exists), and everything else
/// answers as a successful `plugin pane open`.
fn fake_herdr(dir: &Path) -> (PathBuf, PathBuf) {
    let path = dir.join("herdr");
    let log = dir.join("herdr.log");
    fs::write(
        &path,
        format!(
            concat!(
                "#!/bin/sh\n",
                "dir='{dir}'\n",
                "printf '%s\\n' \"$*\" >> '{log}'\n",
                "case \"$*\" in\n",
                "  'pane list'*)\n",
                "    if [ -f \"$dir/panes.json\" ]; then cat \"$dir/panes.json\";\n",
                "    else printf '%s\\n' '{{\"result\":{{\"panes\":[{{\"pane_id\":\"w1:p1\"}}]}}}}'; fi ;;\n",
                "  'pane process-info --pane '*)\n",
                "    if [ -f \"$dir/procfail-$4.json\" ]; then cat \"$dir/procfail-$4.json\" >&2; exit 1; fi\n",
                "    if [ -f \"$dir/procinfo-$4.json\" ]; then cat \"$dir/procinfo-$4.json\";\n",
                "    else printf '%s\\n' '{{\"result\":{{\"process_info\":{{\"foreground_process_group_id\":7,\"foreground_processes\":[{{\"pid\":7,\"name\":\"zsh\",\"argv0\":\"zsh\",\"argv\":[\"-zsh\"],\"cwd\":\"/\"}}],\"pane_id\":\"'\"$4\"'\",\"shell_pid\":1}}}}}}'; fi ;;\n",
                "  'pane close '*)\n",
                "    if [ -f \"$dir/closefail-$3\" ]; then cat \"$dir/closefail-$3\" >&2; exit 1; fi\n",
                "    printf '%s\\n' '{{\"result\":{{}}}}' ;;\n",
                "  'plugin config-dir '*)\n",
                "    if [ -f \"$dir/configdir-hang\" ]; then sleep 5; fi\n",
                "    printf '%s\\n' \"$dir\" ;;\n",
                "  *) printf '%s\\n' '{{\"result\":{{\"plugin_pane\":{{\"pane\":{{\"pane_id\":\"w1:p9\",\"tab_id\":\"w1:t9\"}}}}}}}}' ;;\n",
                "esac\n",
            ),
            dir = dir.display(),
            log = log.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    (path, log)
}

/// One `pane process-info` answer for `pane`: a foreground process group holding `entries`,
/// in the live envelope shape (docs/herdr-api-notes.md).
fn procinfo(dir: &Path, pane: &str, entries: &str) {
    fs::write(
        dir.join(format!("procinfo-{pane}.json")),
        format!(
            r#"{{"result":{{"process_info":{{"foreground_process_group_id":7,"foreground_processes":[{entries}],"pane_id":"{pane}","shell_pid":1}}}}}}"#
        ),
    )
    .unwrap();
}

/// A fresh git repo at `dir/name`, for tests that need a real second repo beside the
/// crate's own.
fn init_repo(dir: &Path, name: &str) -> PathBuf {
    let repo = dir.join(name);
    fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .arg("-q")
            .arg("-b")
            .arg("main")
            .status()
            .unwrap()
            .success()
    );
    repo
}

/// One `pane list` answer: a single pane whose entry carries a live `foreground_cwd`,
/// in the live envelope shape (docs/herdr-api-notes.md).
fn pane_with_cwd(dir: &Path, pane: &str, foreground_cwd: &Path) {
    fs::write(
        dir.join("panes.json"),
        format!(
            r#"{{"result":{{"panes":[{{"pane_id":"{pane}","foreground_cwd":"{cwd}"}}]}}}}"#,
            cwd = foreground_cwd.display(),
        ),
    )
    .unwrap();
}

fn run(mode: &str, config_dir: &Path, herdr: &Path) -> Output {
    Command::new("bash")
        .arg("herdr/pane.sh")
        .arg(mode)
        .env("HERDR_REVIEWR_BIN", reviewr_bin())
        .env("HERDR_PLUGIN_CONFIG_DIR", config_dir)
        .env("HERDR_BIN_PATH", herdr)
        .env("HERDR_WORKSPACE_ID", "workspace-1")
        .output()
        .unwrap()
}

/// An `open` with the workspace context a focused pane provides, so the run reaches the
/// placement and `plugin pane open` stages.
fn run_open(config_dir: &Path, herdr: &Path) -> Output {
    let context = serde_json::json!({"focused_pane_cwd": env!("CARGO_MANIFEST_DIR")}).to_string();
    run_with_context("open", config_dir, herdr, &context)
}

/// Any mode with a caller-shaped action context.
fn run_with_context(mode: &str, config_dir: &Path, herdr: &Path, context: &str) -> Output {
    Command::new("bash")
        .arg("herdr/pane.sh")
        .arg(mode)
        .env("HERDR_REVIEWR_BIN", reviewr_bin())
        .env("HERDR_PLUGIN_CONFIG_DIR", config_dir)
        .env("HERDR_BIN_PATH", herdr)
        .env("HERDR_WORKSPACE_ID", "workspace-1")
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context)
        .output()
        .unwrap()
}

#[test]
fn invalid_config_refuses_manual_action_before_herdr_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "theme = \"not-a-theme\"\n").unwrap();
    let (herdr, log) = fake_herdr(dir.path());

    for mode in ["open", "close", "toggle"] {
        let output = run(mode, dir.path(), &herdr);
        assert_eq!(output.status.code(), Some(1), "{mode}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("config.toml"), "{mode}: {stderr}");
        assert!(stderr.contains("`theme`"), "{mode}: {stderr}");
    }
    assert!(!log.exists(), "herdr was invoked before validation");
}

#[test]
fn invalid_config_refuses_event_loudly_before_herdr_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "auto_open = \"sometimes\"\n").unwrap();
    let (herdr, log) = fake_herdr(dir.path());

    let output = run("auto-open", dir.path(), &herdr);

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("`auto_open`"));
    assert!(!log.exists(), "herdr was invoked before validation");
}

#[test]
fn corrected_config_recovers_on_the_next_invocation() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, "unknown = true\n").unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    assert_eq!(run("close", dir.path(), &herdr).status.code(), Some(1));
    assert!(!log.exists());

    fs::write(&config, "theme = \"gruvbox\"\n").unwrap();
    let output = run("close", dir.path(), &herdr);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("close: nothing open"));
    assert!(fs::read_to_string(log).unwrap().contains("pane list --workspace workspace-1"));
}

#[test]
fn disabled_auto_open_stops_after_successful_validation() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "auto_open = false\n").unwrap();
    let (herdr, log) = fake_herdr(dir.path());

    let output = run("auto-open", dir.path(), &herdr);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!log.exists());
}

#[test]
fn valid_auto_open_runtime_refusal_remains_silent() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());

    let output = Command::new("bash")
        .arg("herdr/pane.sh")
        .arg("auto-open")
        .env("HERDR_REVIEWR_BIN", reviewr_bin())
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
        .env("HERDR_BIN_PATH", &herdr)
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_PLUGIN_CONTEXT_JSON")
        .env_remove("HERDR_PLUGIN_EVENT_JSON")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!log.exists());
}

// --- Pane identity: the foreground process decides, never the label.

#[test]
fn a_pane_running_the_review_ui_counts_however_it_was_launched() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // A wrapped launch: `cargo run` holds the group, its child is the review UI, and the
    // pane carries no `reviewr` label at all.
    procinfo(
        dir.path(),
        "w1:p1",
        concat!(
            r#"{"pid":7,"name":"cargo","argv0":"cargo","argv":["cargo","run"],"cwd":"/w"},"#,
            // The child's title (`name`) is rewritten, so only the executable identifies it.
            r#"{"pid":8,"name":"some-title","argv0":"target/debug/herdr-reviewr","argv":["target/debug/herdr-reviewr"],"cwd":"/w"}"#
        ),
    );

    let output = run("open", dir.path(), &herdr);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("already open (w1:p1)"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !fs::read_to_string(&log).unwrap().contains("plugin pane open"),
        "an open over a live pane must not stack another"
    );

    // `close` sweeps the same pane by the same live read, with a plain `pane close`.
    let output = run("close", dir.path(), &herdr);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("closed w1:p1"));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.lines().any(|l| l == "pane close w1:p1"), "{calls}");
}

#[test]
fn close_sweeps_every_reviewr_pane_and_a_close_that_lost_the_race_still_converges() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // w1:p2 is a plain shell wearing a stale `reviewr` label — a crashed binary's leftover.
    // The label is display only and never read, so the
    // sweep below must not touch it.
    fs::write(
        dir.path().join("panes.json"),
        r#"{"result":{"panes":[{"pane_id":"w1:p1"},{"pane_id":"w1:p2","label":"reviewr"},{"pane_id":"w1:p3"}]}}"#,
    )
    .unwrap();
    let ui = r#"{"pid":8,"name":"herdr-reviewr","argv0":"herdr-reviewr","argv":["/plugin/bin/herdr-reviewr"],"cwd":"/w"}"#;
    procinfo(dir.path(), "w1:p1", ui);
    procinfo(dir.path(), "w1:p3", ui);
    // w1:p3's close fails with the pane gone: it exited between the read and the close.
    // The sweep still exits 0 — the end state is the same.
    fs::write(
        dir.path().join("closefail-w1:p3"),
        r#"{"error":{"code":"pane_not_found","message":"pane w1:p3 not found"},"id":"cli:request"}"#,
    )
    .unwrap();

    let output = run("close", dir.path(), &herdr);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("closed w1:p1 w1:p3"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let calls = fs::read_to_string(&log).unwrap();
    // Whole log lines, so a `plugin pane close` could not satisfy the plain-`pane close`
    // contract these assert.
    assert!(calls.lines().any(|l| l == "pane close w1:p1"), "{calls}");
    assert!(calls.lines().any(|l| l == "pane close w1:p3"), "{calls}");
    assert!(
        !calls.contains("pane close w1:p2"),
        "a labeled plain shell must not be swept: {calls}"
    );
}

#[test]
fn a_close_that_fails_for_a_live_pane_sweeps_the_rest_then_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    fs::write(
        dir.path().join("panes.json"),
        r#"{"result":{"panes":[{"pane_id":"w1:p1"},{"pane_id":"w1:p3"}]}}"#,
    )
    .unwrap();
    let ui = r#"{"pid":8,"name":"herdr-reviewr","argv0":"herdr-reviewr","argv":["/plugin/bin/herdr-reviewr"],"cwd":"/w"}"#;
    procinfo(dir.path(), "w1:p1", ui);
    procinfo(dir.path(), "w1:p3", ui);
    // w1:p1's close fails with the pane still there — a wedged herdr, not the benign
    // exited-between-read-and-close race. Reporting it closed would leave a running pane
    // the user believes gone, so the sweep refuses.
    fs::write(
        dir.path().join("closefail-w1:p1"),
        r#"{"error":{"code":"internal","message":"boom"},"id":"cli:request"}"#,
    )
    .unwrap();

    let output = run("close", dir.path(), &herdr);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pane close failed for w1:p1"), "{stderr}");
    // The refusal comes after the sweep, so the panes herdr could close are closed.
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.lines().any(|l| l == "pane close w1:p3"), "{calls}");
}

#[test]
fn a_gone_pane_skips_and_an_unreadable_read_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, _log) = fake_herdr(dir.path());
    // The read reports the pane gone: it exited between the list and the read, so the
    // action converges — this close has nothing to sweep and exits 0.
    fs::write(
        dir.path().join("procfail-w1:p1.json"),
        r#"{"error":{"code":"pane_not_found","message":"pane w1:p1 not found"},"id":"cli:request"}"#,
    )
    .unwrap();
    let output = run("close", dir.path(), &herdr);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("close: nothing open"));

    // Any other read failure refuses, never reads as "no reviewr pane": an open would
    // stack a duplicate and a close would false-succeed.
    fs::write(
        dir.path().join("procfail-w1:p1.json"),
        r#"{"error":{"code":"internal","message":"boom"},"id":"cli:request"}"#,
    )
    .unwrap();
    for mode in ["open", "close", "toggle"] {
        let output = run(mode, dir.path(), &herdr);
        assert_eq!(output.status.code(), Some(1), "{mode}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("process-info failed"),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn an_action_repoints_the_stable_launch_paths_at_the_live_plugin_root() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, _log) = fake_herdr(dir.path());
    // The install's build step runs in a staging checkout herdr renames afterwards, so the
    // actions own the stable links: every valid invocation re-points them at the runtime
    // root. `~/.local/bin` only when it exists.
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("bin")).unwrap();
    fs::write(root.path().join("bin/herdr-reviewr"), "#!/bin/sh\n").unwrap();
    let mut permissions =
        fs::metadata(root.path().join("bin/herdr-reviewr")).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(root.path().join("bin/herdr-reviewr"), permissions).unwrap();
    let run_close = |home: &Path| {
        Command::new("bash")
            .arg("herdr/pane.sh")
            .arg("close")
            .env("HERDR_REVIEWR_BIN", reviewr_bin())
            .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
            .env("HERDR_BIN_PATH", &herdr)
            .env("HERDR_WORKSPACE_ID", "workspace-1")
            .env("HERDR_PLUGIN_ROOT", root.path())
            .env("HOME", home)
            .output()
            .unwrap()
    };

    let output = run_close(home.path());

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let state_link =
        home.path().join(".local/state/herdr/plugins/persiyanov.reviewr/bin/herdr-reviewr");
    assert_eq!(fs::read_link(&state_link).unwrap(), root.path().join("bin/herdr-reviewr"));
    let bin_link = home.path().join(".local/bin/herdr-reviewr");
    assert!(!bin_link.exists(), "~/.local/bin must not be created for the link");

    // With `~/.local/bin` present, the second link lands too — and an existing symlink
    // re-points rather than blocks.
    fs::create_dir_all(home.path().join(".local/bin")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/old", &bin_link).unwrap();
    let output = run_close(home.path());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read_link(&bin_link).unwrap(), root.path().join("bin/herdr-reviewr"));
}

#[test]
fn a_process_info_answer_missing_its_shape_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, _log) = fake_herdr(dir.path());
    // Exit 0 with an error envelope — no `.result.process_info.foreground_processes`. A
    // shape failure must refuse like a failed pane list, never read as "no reviewr pane".
    fs::write(
        dir.path().join("procinfo-w1:p1.json"),
        r#"{"error":{"code":"internal","message":"boom"},"id":"cli:request"}"#,
    )
    .unwrap();
    for mode in ["open", "close", "toggle"] {
        let output = run(mode, dir.path(), &herdr);
        assert_eq!(output.status.code(), Some(1), "{mode}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("process-info failed"),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn a_failed_pane_list_refuses_rather_than_reading_as_no_pane() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, _log) = fake_herdr(dir.path());
    // `pane list` answering an error envelope (exit 0, no `.result.panes`) must refuse:
    // read as "no reviewr pane", an open would stack a duplicate and a close would
    // false-succeed with panes still running.
    fs::write(
        dir.path().join("panes.json"),
        r#"{"error":{"code":"internal","message":"boom"},"id":"cli:request"}"#,
    )
    .unwrap();

    let output = run("close", dir.path(), &herdr);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("pane list failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_cli_fallback_resolves_the_config_dir_when_the_env_names_none() {
    // The launcher-blind half of config resolution: with no
    // `HERDR_PLUGIN_CONFIG_DIR`, the binary asks `herdr plugin config-dir` and reads the
    // directory it names. This is the one test that exercises the real herdr-CLI path —
    // the unit tests drive the resolver with an injected closure.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "theme = \"gruvbox\"\n").unwrap();
    let (herdr, _log) = fake_herdr(dir.path());

    let output = Command::new(reviewr_bin())
        .arg("--resolve-plugin-config")
        .env_remove("HERDR_PLUGIN_CONFIG_DIR")
        .env("HERDR_BIN_PATH", &herdr)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("gruvbox"), "expected the CLI-named dir's config: {stdout}");
}

#[test]
fn a_wedged_config_dir_lookup_degrades_to_the_defaults_inside_the_bound() {
    // A herdr that does not answer resolves no directory, the missing-file outcome
    // The fake hangs 5s, well past the binary's
    // bound, so a success here can only come from giving the lookup up.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "theme = \"gruvbox\"\n").unwrap();
    fs::write(dir.path().join("configdir-hang"), "").unwrap();
    let (herdr, _log) = fake_herdr(dir.path());

    let output = Command::new(reviewr_bin())
        .arg("--resolve-plugin-config")
        .env_remove("HERDR_PLUGIN_CONFIG_DIR")
        .env("HERDR_BIN_PATH", &herdr)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("gruvbox"), "a hung lookup must name no directory: {stdout}");
    assert!(stdout.contains("\"theme\""), "the defaults still print in full: {stdout}");
}

#[test]
fn a_flag_run_never_counts_as_the_review_ui() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // The review binary run for its config flag is not the review UI (Pane identity), so
    // `open` opens a fresh pane over it. The fake's default answer — a plain shell — is the
    // not-a-reviewr-pane baseline every placement test below relies on.
    procinfo(
        dir.path(),
        "w1:p1",
        r#"{"pid":7,"name":"herdr-reviewr","argv0":"herdr-reviewr","argv":["herdr-reviewr","--resolve-plugin-config"],"cwd":"/w"}"#,
    );

    let output = run_open(dir.path(), &herdr);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains("plugin pane open"), "a flag run must not read as open: {calls}");
}

#[test]
fn the_flag_dispatch_matches_the_actions_anywhere_in_argv() {
    // The other half of the flag-run contract, pinned in the binary itself: `pane.sh`
    // excludes `--resolve-plugin-config` wherever it sits in argv, so `main.rs` must
    // recognize it there too — or a flag run would start the review UI while the actions
    // refuse to count it.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "theme = \"gruvbox\"\n").unwrap();

    let output = Command::new(reviewr_bin())
        .args(["--some-future-arg", "--resolve-plugin-config"])
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"theme\""), "expected resolved config JSON, got: {stdout}");
}

#[test]
fn valid_non_default_placement_and_direction_reach_herdr_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let (herdr, log) = fake_herdr(dir.path());

    let cases = [
        ("toggle_placement = \"overlay\"\n", "--placement overlay", None),
        (
            "toggle_placement = \"split\"\ntoggle_direction = \"down\"\n",
            "--placement split",
            Some("--direction down"),
        ),
    ];
    for (text, placement, direction) in cases {
        fs::write(&config, text).unwrap();
        let _ = fs::remove_file(&log);
        let output = run_open(dir.path(), &herdr);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let calls = fs::read_to_string(&log).unwrap();
        assert!(calls.contains(placement), "{calls}");
        if let Some(direction) = direction {
            assert!(calls.contains(direction), "{calls}");
        }
    }
}

#[test]
fn tab_placement_open_names_its_fresh_tab() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "toggle_placement = \"tab\"\n").unwrap();
    let (herdr, log) = fake_herdr(dir.path());

    let output = run_open(dir.path(), &herdr);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains("tab rename w1:t9 reviewr"), "{calls}");
}

// --- Open cwd: the focused pane's live foreground cwd, then the context's launch cwd.

#[test]
fn open_prefers_the_focused_panes_live_foreground_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // The launch cwd is not a git repo: an open that trusted it would refuse. The pane's
    // live foreground cwd is the reviewed repo — the `claude -w <worktree>` shape, where
    // the agent chdirs into the worktree only inside its own process after launching
    // from the main checkout. The live cwd comes from the pane-list snapshot, so the
    // open pays no extra herdr call.
    let context = serde_json::json!({
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": dir.path().to_str().unwrap(),
    })
    .to_string();
    pane_with_cwd(dir.path(), "w1:p1", Path::new(env!("CARGO_MANIFEST_DIR")));

    let output = run_with_context("open", dir.path(), &herdr, &context);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the open must use the live foreground cwd: {calls}"
    );
    // The live cwd comes from the run's one snapshot; a second listing would put a herdr
    // round-trip back on the keypress path.
    assert_eq!(
        calls.matches("pane list --workspace").count(),
        1,
        "the open must reuse the held pane-list snapshot: {calls}"
    );
}

#[test]
fn open_prefers_the_live_cwd_when_the_launch_cwd_is_also_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // The motivating shape exactly: the launch cwd is itself a valid repo (the main
    // checkout `claude -w <worktree>` was launched from), so a fallback-only read would
    // pass every other test and still review the wrong repo. The live cwd must win.
    let launch_repo = init_repo(dir.path(), "main-checkout");
    let context = serde_json::json!({
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": launch_repo.to_str().unwrap(),
    })
    .to_string();
    pane_with_cwd(dir.path(), "w1:p1", Path::new(env!("CARGO_MANIFEST_DIR")));

    let output = run_with_context("open", dir.path(), &herdr, &context);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the live cwd must win over a launch cwd that is also a repo: {calls}"
    );
}

#[test]
fn open_keeps_the_context_cwd_without_a_live_foreground_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // The fake's default pane-list entry carries no foreground cwd — a pane whose live
    // read has nothing to add keeps the context cwd instead of losing it.
    let context = serde_json::json!({
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": env!("CARGO_MANIFEST_DIR"),
    })
    .to_string();

    let output = run_with_context("open", dir.path(), &herdr, &context);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the open must fall back to the context cwd: {calls}"
    );
}

#[test]
fn a_toggle_open_falls_back_when_the_live_cwd_is_not_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // The live foreground cwd sits outside any git repo — a shell that wandered off. The
    // live cwd wins only inside a repo; here it yields to the context cwd rather than
    // refusing an open the context alone could place. Run as a toggle, so the opening toggle exercises the same block.
    let context = serde_json::json!({
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": env!("CARGO_MANIFEST_DIR"),
    })
    .to_string();
    pane_with_cwd(dir.path(), "w1:p1", dir.path());

    let output = run_with_context("toggle", dir.path(), &herdr, &context);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "a non-repo live cwd must fall back to the context cwd: {calls}"
    );
}

#[test]
fn open_takes_the_focused_panes_cwd_not_another_panes() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // Two panes, both in repos: a decoy listed first and the focused pane after it. The
    // lookup must key on the focused pane's id — a first-entry read would review the
    // decoy's repo.
    let decoy_repo = init_repo(dir.path(), "decoy-repo");
    fs::write(
        dir.path().join("panes.json"),
        format!(
            r#"{{"result":{{"panes":[{{"pane_id":"w1:p0","foreground_cwd":"{decoy}"}},{{"pane_id":"w1:p1","foreground_cwd":"{focused}"}}]}}}}"#,
            decoy = decoy_repo.display(),
            focused = env!("CARGO_MANIFEST_DIR"),
        ),
    )
    .unwrap();
    let context = serde_json::json!({
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": dir.path().to_str().unwrap(),
    })
    .to_string();

    let output = run_with_context("open", dir.path(), &herdr, &context);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the open must use the focused pane's cwd, not the decoy's: {calls}"
    );
}

#[test]
fn a_refusal_names_the_rejected_live_cwd_too() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, _log) = fake_herdr(dir.path());
    // No context cwd and a non-repo live cwd: the open refuses, and the one stderr line
    // names the live directory it inspected and rejected — a refusal that hid it would
    // read as if no directory was ever tried.
    let context = serde_json::json!({"focused_pane_id": "w1:p1"}).to_string();
    pane_with_cwd(dir.path(), "w1:p1", dir.path());

    let output = run_with_context("open", dir.path(), &herdr, &context);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not a git repo"), "{stderr}");
    assert!(
        stderr.contains(dir.path().to_str().unwrap()),
        "the refusal must name the rejected live cwd: {stderr}"
    );
}

#[test]
fn auto_open_takes_the_event_payload_cwd_over_the_live_one() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    // The focused pane's live cwd is a real git repo, so only the mode guard keeps it
    // from winning: the event open takes its directory from the payload alone
    let live_repo = init_repo(dir.path(), "live-repo");
    pane_with_cwd(dir.path(), "w1:p1", &live_repo);
    let context = serde_json::json!({
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": dir.path().to_str().unwrap(),
    })
    .to_string();
    let event = serde_json::json!({
        "data": {"workspace": {
            "workspace_id": "workspace-9",
            "worktree": {"checkout_path": env!("CARGO_MANIFEST_DIR")},
        }},
    })
    .to_string();

    let output = Command::new("bash")
        .arg("herdr/pane.sh")
        .arg("auto-open")
        .env("HERDR_REVIEWR_BIN", reviewr_bin())
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
        .env("HERDR_BIN_PATH", &herdr)
        .env("HERDR_PLUGIN_CONTEXT_JSON", &context)
        .env("HERDR_PLUGIN_EVENT_JSON", &event)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the event open must use the payload cwd: {calls}"
    );
}

#[test]
fn a_manual_open_passes_focus() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    let context = serde_json::json!({"focused_pane_cwd": env!("CARGO_MANIFEST_DIR")}).to_string();

    for mode in ["open", "toggle"] {
        let _ = fs::remove_file(&log);
        let output = run_with_context(mode, dir.path(), &herdr, &context);
        assert!(output.status.success(), "{mode}: {}", String::from_utf8_lossy(&output.stderr));
        let calls = fs::read_to_string(&log).unwrap();
        let tokens: Vec<&str> = calls.split_whitespace().collect();
        assert!(tokens.contains(&"--focus"), "{mode} must pass --focus: {calls}");
        assert!(!tokens.contains(&"--no-focus"), "{mode} must not pass --no-focus: {calls}");
    }
}

#[test]
fn auto_open_passes_no_focus() {
    let dir = tempfile::tempdir().unwrap();
    let (herdr, log) = fake_herdr(dir.path());
    let event = serde_json::json!({
        "data": {"workspace": {
            "workspace_id": "workspace-9",
            "worktree": {"checkout_path": env!("CARGO_MANIFEST_DIR")},
        }},
    })
    .to_string();

    let output = Command::new("bash")
        .arg("herdr/pane.sh")
        .arg("auto-open")
        .env("HERDR_REVIEWR_BIN", reviewr_bin())
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
        .env("HERDR_BIN_PATH", &herdr)
        .env("HERDR_PLUGIN_EVENT_JSON", &event)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    let tokens: Vec<&str> = calls.split_whitespace().collect();
    assert!(tokens.contains(&"--no-focus"), "auto-open must pass --no-focus: {calls}");
    assert!(!tokens.contains(&"--focus"), "auto-open must not pass --focus: {calls}");
}

#[test]
fn split_placement_open_renames_no_tab() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "toggle_placement = \"split\"\n").unwrap();
    let (herdr, log) = fake_herdr(dir.path());

    let output = run_open(dir.path(), &herdr);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("tab rename"), "{calls}");
}
