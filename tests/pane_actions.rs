//! The plugin actions end to end: the real binary against a fake herdr, no live herdr reached.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Repo, fake_herdr, fixture, herdr_calls, herdr_error};
use serde_json::{Value, json};

fn reviewr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-reviewr")
}

/// One `pane process-info` answer for `pane` holding `processes`.
fn procinfo(dir: &Path, pane: &str, processes: &Value) {
    let answer = json!({"result": {"process_info": {
        "foreground_process_group_id": 7,
        "foreground_processes": processes,
        "pane_id": pane,
        "shell_pid": 1,
    }}});
    fs::write(fixture(dir, "procinfo", pane, ".json"), answer.to_string()).unwrap();
}

/// One foreground process, as herdr reports it.
fn process(argv0: &str, argv: &[&str]) -> Value {
    json!({"pid": 8, "name": "some-title", "argv0": argv0, "argv": argv, "cwd": "/w"})
}

/// The review UI as a plugin pane runs it.
fn review_ui() -> Value {
    process("herdr-reviewr", &["/plugin/bin/herdr-reviewr"])
}

/// One `pane list` answer holding `panes`.
fn panes(dir: &Path, panes: &Value) {
    fs::write(dir.join("panes.json"), json!({"result": {"panes": panes}}).to_string()).unwrap();
}

/// One `pane list` answer: a single pane whose entry carries a live `foreground_cwd`.
fn pane_with_cwd(dir: &Path, pane: &str, foreground_cwd: &Path) {
    panes(dir, &json!([{"pane_id": pane, "foreground_cwd": foreground_cwd}]));
}

/// Forget every call the fake logged and close the pane it opened, for the next run in `dir`.
fn reset(dir: &Path) {
    let _ = fs::remove_file(dir.join("herdr.log"));
    let _ = fs::remove_file(dir.join("opened"));
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Action `mode` as herdr runs it in `workspace-1`, every other herdr variable cleared.
fn action(mode: &str, dir: &Path) -> Command {
    let mut command = Command::new(reviewr_bin());
    command
        .args(["--action", mode])
        .env("HERDR_PLUGIN_CONFIG_DIR", dir)
        .env("HERDR_PLUGIN_STATE_DIR", dir)
        .env("HERDR_BIN_PATH", fake_herdr())
        .env("FAKE_HERDR_DIR", dir)
        .env("HERDR_WORKSPACE_ID", "workspace-1");
    for name in [
        "HERDR_PANE_ID",
        "HERDR_PLUGIN_ID",
        "HERDR_PLUGIN_ROOT",
        "HERDR_PLUGIN_CONTEXT_JSON",
        "HERDR_PLUGIN_EVENT_JSON",
    ] {
        command.env_remove(name);
    }
    command
}

fn run(mode: &str, dir: &Path) -> Output {
    action(mode, dir).output().unwrap()
}

/// An `open` with a focused pane's context.
fn run_open(dir: &Path) -> Output {
    run_with_context("open", dir, &repo_context())
}

/// The action context of a focused pane in this crate's repo, so an open can proceed.
fn repo_context() -> String {
    json!({"focused_pane_cwd": env!("CARGO_MANIFEST_DIR")}).to_string()
}

/// Any mode with a caller-shaped action context, invoked from pane `w1:p1`.
fn run_with_context(mode: &str, dir: &Path, context: &str) -> Output {
    with_context(mode, dir, context).output().unwrap()
}

fn with_context(mode: &str, dir: &Path, context: &str) -> Command {
    let mut command = action(mode, dir);
    command.env("HERDR_PANE_ID", "w1:p1").env("HERDR_PLUGIN_CONTEXT_JSON", context);
    command
}

/// The event hook, as herdr fires it: no workspace or pane of its own, only the payload.
fn run_auto_open(dir: &Path, event: &str, context: Option<&str>) -> Output {
    let mut command = action("auto-open", dir);
    command.env_remove("HERDR_WORKSPACE_ID").env("HERDR_PLUGIN_EVENT_JSON", event);
    if let Some(context) = context {
        command.env("HERDR_PLUGIN_CONTEXT_JSON", context);
    }
    command.output().unwrap()
}

/// A worktree event payload in the live shape (docs/herdr-api-notes.md).
fn worktree_event(
    name: &str,
    workspace: &str,
    checkout: &str,
    already_open: Option<bool>,
) -> String {
    let mut data = json!({
        "type": name,
        "workspace": {"workspace_id": workspace, "worktree": {"checkout_path": checkout}},
        "worktree": {"path": checkout, "open_workspace_id": workspace},
    });
    if let Some(already_open) = already_open {
        data["already_open"] = json!(already_open);
    }
    json!({"event": name, "data": data}).to_string()
}

/// The `plugin pane open` call a run made.
fn open_call(dir: &Path) -> String {
    herdr_calls(dir)
        .lines()
        .find(|line| line.starts_with("plugin pane open"))
        .expect("a plugin pane open call")
        .to_owned()
}

// --- Config: the whole file validates before any herdr call.

#[test]
fn invalid_config_refuses_manual_action_before_herdr_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "theme = \"not-a-theme\"\n").unwrap();

    for mode in ["open", "close", "toggle"] {
        let output = run(mode, dir.path());
        assert_eq!(output.status.code(), Some(1), "{mode}");
        assert!(stderr(&output).contains("config.toml"), "{mode}: {}", stderr(&output));
        assert!(stderr(&output).contains("`theme`"), "{mode}: {}", stderr(&output));
    }
    assert!(herdr_calls(dir.path()).is_empty(), "herdr was invoked before validation");
}

#[test]
fn invalid_config_refuses_event_loudly_before_herdr_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "auto_open = \"sometimes\"\n").unwrap();

    let output = run("auto-open", dir.path());

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("`auto_open`"), "{}", stderr(&output));
    assert!(herdr_calls(dir.path()).is_empty(), "herdr was invoked before validation");
}

#[test]
fn corrected_config_recovers_on_the_next_invocation() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, "unknown = true\n").unwrap();
    assert_eq!(run("close", dir.path()).status.code(), Some(1));
    assert!(herdr_calls(dir.path()).is_empty());

    fs::write(&config, "theme = \"gruvbox\"\n").unwrap();
    let output = run("close", dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: nothing open in workspace-1\n");
    assert!(herdr_calls(dir.path()).contains("pane list --workspace workspace-1"));
}

#[test]
fn restore_relaunches_only_labelled_shells_that_no_agent_holds() {
    let dir = tempfile::tempdir().unwrap();
    panes(
        dir.path(),
        &json!([
            {"pane_id": "w1:p1", "label": "reviewr"},
            {"pane_id": "w1:p2", "label": "reviewr"},
            {"pane_id": "w2:p3", "label": "reviewr", "agent": "claude"},
            {"pane_id": "w2:p4", "label": "reviewr", "agent_session": {"value": "id"}},
            {"pane_id": "w2:p5"},
        ]),
    );
    procinfo(dir.path(), "w1:p2", &json!([review_ui()]));

    // A startup hook runs outside any workspace.
    let output = action("restore", dir.path()).env_remove("HERDR_WORKSPACE_ID").output().unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    let runs: Vec<String> = herdr_calls(dir.path())
        .lines()
        .filter(|l| l.starts_with("pane run"))
        .map(Into::into)
        .collect();
    assert_eq!(runs, [format!("pane run w1:p1 '{}'", reviewr_bin())]);
}

#[test]
fn an_unknown_action_refuses_before_any_herdr_call() {
    let dir = tempfile::tempdir().unwrap();

    let output = run("frobnicate", dir.path());

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "reviewr: unknown action 'frobnicate' (toggle | open | close | auto-open | restore)\n"
    );
    assert!(output.stdout.is_empty());
    assert!(herdr_calls(dir.path()).is_empty());

    // A bare `--action` names no action at all, and refuses the same way.
    let output = Command::new(reviewr_bin())
        .arg("--action")
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
        .env("HERDR_BIN_PATH", fake_herdr())
        .env("FAKE_HERDR_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("unknown action ''"), "{}", stderr(&output));
}

// --- The event: gated by policy, silent on a runtime refusal.

#[test]
fn disabled_auto_open_stops_after_successful_validation() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "auto_open = false\n").unwrap();

    let output = run("auto-open", dir.path());

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(herdr_calls(dir.path()).is_empty());
}

#[test]
fn auto_open_skips_placements_that_are_not_split_or_tab() {
    let dir = tempfile::tempdir().unwrap();
    let event = worktree_event("worktree_created", "workspace-9", env!("CARGO_MANIFEST_DIR"), None);

    for placement in ["overlay", "zoomed"] {
        fs::write(dir.path().join("config.toml"), format!("toggle_placement = \"{placement}\"\n"))
            .unwrap();
        let output = run_auto_open(dir.path(), &event, None);
        assert!(output.status.success(), "{placement}: {}", stderr(&output));
        assert!(output.stdout.is_empty(), "{placement}");
        assert!(output.stderr.is_empty(), "{placement}");
    }
    assert!(herdr_calls(dir.path()).is_empty(), "{}", herdr_calls(dir.path()));
}

#[test]
fn auto_open_opened_live_exits_before_herdr_calls() {
    let dir = tempfile::tempdir().unwrap();
    let event =
        worktree_event("worktree_opened", "workspace-9", env!("CARGO_MANIFEST_DIR"), Some(true));

    let output = run_auto_open(dir.path(), &event, None);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(herdr_calls(dir.path()).is_empty(), "opened-live inspected herdr before exiting");
}

#[test]
fn auto_open_birth_events_follow_shared_policy() {
    let dir = tempfile::tempdir().unwrap();
    let live_repo = Repo::init();
    pane_with_cwd(dir.path(), "w1:p1", live_repo.path());
    let context =
        json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": live_repo.path()}).to_string();

    for (event_name, already_open) in [("worktree_created", None), ("worktree_opened", Some(false))]
    {
        for placement in ["split", "tab"] {
            fs::write(
                dir.path().join("config.toml"),
                format!("toggle_placement = \"{placement}\"\n"),
            )
            .unwrap();
            reset(dir.path());
            let workspace = format!("workspace-{event_name}-{placement}");
            let event =
                worktree_event(event_name, &workspace, env!("CARGO_MANIFEST_DIR"), already_open);

            let output = run_auto_open(dir.path(), &event, Some(&context));

            assert!(output.status.success(), "{event_name}/{placement}: {}", stderr(&output));
            // The event reports nothing on success either.
            assert!(output.stdout.is_empty(), "{event_name}/{placement}: {}", stdout(&output));
            let calls = herdr_calls(dir.path());
            assert!(calls.contains(&format!("pane list --workspace {workspace}")), "{calls}");
            let open = open_call(dir.path());
            let tokens = open.split_whitespace().collect::<Vec<_>>();
            assert!(tokens.contains(&"--no-focus"), "{open}");
            assert!(!tokens.contains(&"--focus"), "{open}");
            assert!(open.contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))), "{open}");
            assert!(open.contains(&format!("--placement {placement}")), "{open}");
            assert!(!open.contains(live_repo.path().to_str().unwrap()), "{open}");
        }
    }
}

#[test]
fn auto_open_without_its_payload_refuses_silently_before_any_herdr_call() {
    let dir = tempfile::tempdir().unwrap();
    // Opening here would stack a pane into whatever the user is looking at.
    for payload in [None, Some("")] {
        let mut command = with_context("auto-open", dir.path(), &repo_context());
        match payload {
            Some(json) => command.env("HERDR_PLUGIN_EVENT_JSON", json),
            None => command.env_remove("HERDR_PLUGIN_EVENT_JSON"),
        };

        let output = command.output().unwrap();

        assert!(output.status.success(), "{payload:?}: {}", stderr(&output));
        assert!(output.stdout.is_empty(), "{payload:?}: {}", stdout(&output));
        assert!(output.stderr.is_empty(), "{payload:?}: {}", stderr(&output));
    }
    assert!(herdr_calls(dir.path()).is_empty(), "{}", herdr_calls(dir.path()));
}

#[test]
fn auto_open_reads_each_payload_field_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let repo = env!("CARGO_MANIFEST_DIR");
    let data = |extra: Value| {
        let mut data = json!({
            "type": "worktree_opened",
            "workspace": {"workspace_id": "workspace-9", "worktree": {"checkout_path": repo}},
        });
        data.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        json!({"event": "worktree_opened", "data": data}).to_string()
    };
    // A mistyped field reads as absent; only boolean `true` marks a workspace open.
    let payloads = [
        data(json!({"worktree": {"path": 42, "open_workspace_id": ["w"]}})),
        data(json!({"already_open": "true"})),
    ];
    for payload in payloads {
        reset(dir.path());

        let output = run_auto_open(dir.path(), &payload, None);

        assert!(output.status.success(), "{payload}: {}", stderr(&output));
        let open = open_call(dir.path());
        assert!(open.contains(&format!("--cwd {repo}")), "{payload}: {open}");
        assert!(
            herdr_calls(dir.path()).contains("pane list --workspace workspace-9"),
            "{payload}: {}",
            herdr_calls(dir.path())
        );
    }
}

#[test]
fn auto_open_falls_back_to_the_worktree_fields_of_the_payload() {
    let dir = tempfile::tempdir().unwrap();
    // Without `data.workspace` the hook targets `data.worktree`'s.
    let event = json!({"event": "worktree_created", "data": {
        "type": "worktree_created",
        "worktree": {"path": env!("CARGO_MANIFEST_DIR"), "open_workspace_id": "workspace-7"},
    }})
    .to_string();

    let output = run_auto_open(dir.path(), &event, None);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        herdr_calls(dir.path()).contains("pane list --workspace workspace-7"),
        "{}",
        herdr_calls(dir.path())
    );
    let open = open_call(dir.path());
    assert!(open.contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))), "{open}");
}

#[test]
fn a_failed_plugin_pane_open_refuses_an_action_and_stays_silent_for_the_event() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("openfail"), herdr_error("internal")).unwrap();

    let output = run_open(dir.path());
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "reviewr: herdr plugin pane open failed\n");

    reset(dir.path());
    let event = worktree_event("worktree_created", "workspace-9", env!("CARGO_MANIFEST_DIR"), None);
    let output = run_auto_open(dir.path(), &event, None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    // The event reached its open, which is what refused.
    assert!(herdr_calls(dir.path()).contains("plugin pane open"), "{}", herdr_calls(dir.path()));
}

#[test]
fn manifest_runs_the_binary_directly_for_every_pane_action_and_event() {
    let manifest: toml::Table =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin.toml"))
            .unwrap()
            .parse()
            .unwrap();
    // 0.9.3 adds the event socket; 0.9.0 already resolves a relative pane `command[0]` against the plugin root.
    assert_eq!(manifest["min_herdr_version"].as_str(), Some("0.9.3"));
    let commands = |section: &str| -> Vec<(toml::Table, Vec<String>)> {
        manifest[section]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let table = entry.as_table().unwrap().clone();
                let command = table["command"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|part| part.as_str().unwrap().to_owned())
                    .collect();
                (table, command)
            })
            .collect()
    };

    let panes = commands("panes");
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].1, ["bin/herdr-reviewr"]);

    // Each action runs itself by its own id, one binding on every OS.
    let mut ids = Vec::new();
    for (table, command) in commands("actions") {
        let id = table["id"].as_str().unwrap();
        assert_eq!(command, ["bin/herdr-reviewr", "--action", id]);
        ids.push(id.to_owned());
    }
    ids.sort_unstable();
    assert_eq!(ids, ["close", "open", "toggle"]);

    let mut auto_open_events = commands("events")
        .into_iter()
        .filter(|(_, command)| command == &["bin/herdr-reviewr", "--action", "auto-open"])
        .map(|(table, _)| table["on"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    auto_open_events.sort_unstable();
    assert_eq!(auto_open_events, ["worktree.created", "worktree.opened"]);

    // No runtime command starts a shell: only the build step may.
    for section in ["panes", "actions", "events"] {
        for (_, command) in commands(section) {
            assert!(
                !["bash", "sh", "powershell", "pwsh"].contains(&command[0].as_str()),
                "{section}: {command:?}"
            );
        }
    }
}

#[test]
fn manifest_builds_with_one_install_script_per_platform() {
    let manifest: toml::Table =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin.toml"))
            .unwrap()
            .parse()
            .unwrap();
    let strings = |value: &toml::Value| -> Vec<String> {
        value.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect()
    };
    assert_eq!(strings(&manifest["platforms"]), ["macos", "linux", "windows"]);

    let builds: Vec<(Vec<String>, Vec<String>)> = manifest["build"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| (strings(&entry["platforms"]), strings(&entry["command"])))
        .collect();
    let expected: [(&[&str], &[&str]); 2] = [
        (&["macos", "linux"], &["bash", "herdr/install.sh"]),
        (
            &["windows"],
            &[
                "powershell",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                "herdr/install.ps1",
            ],
        ),
    ];
    assert_eq!(builds.len(), expected.len());
    for ((platforms, command), (want_platforms, want_command)) in builds.iter().zip(expected) {
        assert_eq!(platforms, want_platforms);
        assert_eq!(command, want_command);
    }
    // Overlapping platform sets would run two installers.
    for platform in ["macos", "linux", "windows"] {
        let matching: Vec<_> = builds
            .iter()
            .filter(|(platforms, _)| platforms.iter().any(|p| p == platform))
            .collect();
        assert_eq!(matching.len(), 1, "{platform}");
        assert!(platform != "windows" || matching[0].1[0] != "bash");
    }
}

/// PowerShell 5.1 reads a BOM-less script as ANSI, so the scripts stay ASCII.
#[test]
fn every_powershell_script_is_ascii() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Every script PowerShell 5.1 runs, the VM's first-logon setup included.
    let mut scripts = vec![root.join("herdr/install.ps1")];
    let mut dirs = vec![root.join("scripts")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "ps1") {
                scripts.push(path);
            }
        }
    }
    assert!(scripts.iter().any(|s| s.ends_with("windows-vm/setup.ps1")), "{scripts:?}");
    for script in scripts {
        let bytes = fs::read(&script).unwrap();
        let offending: Vec<usize> = (0..bytes.len()).filter(|&at| !bytes[at].is_ascii()).collect();
        assert!(offending.is_empty(), "{}: non-ASCII bytes at {offending:?}", script.display());
    }
}

// --- Pane identity: the foreground process decides, never the label.

#[test]
fn a_pane_running_the_review_ui_counts_however_it_was_launched() {
    let dir = tempfile::tempdir().unwrap();
    // A wrapped launch: `cargo run`'s child is the review UI, identified by its executable.
    procinfo(
        dir.path(),
        "w1:p1",
        &json!([
            process("cargo", &["cargo", "run"]),
            process("target/debug/herdr-reviewr", &["target/debug/herdr-reviewr"]),
        ]),
    );

    let output = run("open", dir.path());
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: already open (w1:p1) in workspace-1\n");
    assert!(
        !herdr_calls(dir.path()).contains("plugin pane open"),
        "an open over a live pane must not stack another"
    );

    // `close` sweeps the same pane by the same live read, with a plain `pane close`.
    let output = run("close", dir.path());
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: closed w1:p1 in workspace-1\n");
    let calls = herdr_calls(dir.path());
    assert!(calls.lines().any(|l| l == "pane close w1:p1"), "{calls}");
}

#[test]
fn a_windows_pane_counts_through_its_one_reported_process() {
    let dir = tempfile::tempdir().unwrap();
    // herdr on Windows reports one process per pane, `.exe` in either case.
    for exe in [
        r"C:\Users\me\.config\herdr\plugins\github\persiyanov.reviewr-1a2b\bin\herdr-reviewr.exe",
        r"C:\Users\me\.config\herdr\plugins\github\persiyanov.reviewr-1a2b\bin\herdr-reviewr.EXE",
    ] {
        procinfo(dir.path(), "w1:p1", &json!([process(exe, &[exe])]));

        let output = run("open", dir.path());

        assert!(output.status.success(), "{exe}: {}", stderr(&output));
        assert_eq!(stdout(&output), "reviewr: already open (w1:p1) in workspace-1\n", "{exe}");
    }
}

#[test]
fn a_review_ui_started_with_ui_flags_still_counts() {
    let dir = tempfile::tempdir().unwrap();
    procinfo(
        dir.path(),
        "w1:p1",
        &json!([process("herdr-reviewr", &["herdr-reviewr", "--base", "main", "/repo"])]),
    );

    let output = run("close", dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: closed w1:p1 in workspace-1\n");
}

#[test]
fn a_flag_run_never_counts_as_the_review_ui() {
    let dir = tempfile::tempdir().unwrap();
    // A non-UI flag run is not the review UI, so `open` opens over it.
    let flag_runs: [&[&str]; 2] =
        [&["herdr-reviewr", "--action", "toggle"], &["herdr-reviewr", "/repo", "--action"]];
    for argv in flag_runs {
        reset(dir.path());
        procinfo(dir.path(), "w1:p1", &json!([process("herdr-reviewr", argv)]));

        let output = run_open(dir.path());

        assert!(output.status.success(), "{argv:?}: {}", stderr(&output));
        assert!(
            herdr_calls(dir.path()).contains("plugin pane open"),
            "{argv:?}: a flag run must not read as open: {}",
            herdr_calls(dir.path())
        );
    }
}

#[test]
fn the_flag_dispatch_matches_the_actions_anywhere_in_argv() {
    // The flag after a UI argument still dispatches to the action, never the review UI.
    let dir = tempfile::tempdir().unwrap();
    let mut close = action("close", dir.path());
    let output = close.args(["/some/repo"]).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: nothing open in workspace-1\n");
    let output = Command::new(reviewr_bin())
        .args(["/some/repo", "--action", "close"])
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
        .env("HERDR_PLUGIN_STATE_DIR", dir.path())
        .env("HERDR_BIN_PATH", fake_herdr())
        .env("FAKE_HERDR_DIR", dir.path())
        .env("HERDR_WORKSPACE_ID", "workspace-1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: nothing open in workspace-1\n");
}

#[test]
fn an_action_makes_a_state_dir_herdr_named_but_never_created() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let output =
        action("close", dir.path()).env("HERDR_PLUGIN_STATE_DIR", &state).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: nothing open in workspace-1\n");
    assert!(state.is_dir());
}

#[test]
fn close_sweeps_every_reviewr_pane_and_a_close_that_lost_the_race_still_converges() {
    let dir = tempfile::tempdir().unwrap();
    // A plain shell with a stale `reviewr` label: the label is never read.
    panes(
        dir.path(),
        &json!([{"pane_id": "w1:p1"}, {"pane_id": "w1:p2", "label": "reviewr"}, {"pane_id": "w1:p3"}]),
    );
    procinfo(dir.path(), "w1:p1", &json!([review_ui()]));
    procinfo(dir.path(), "w1:p3", &json!([review_ui()]));
    // A pane gone before its close still converges.
    fs::write(fixture(dir.path(), "closefail", "w1:p3", ""), herdr_error("pane_not_found"))
        .unwrap();

    let output = run("close", dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: closed w1:p1 w1:p3 in workspace-1\n");
    let calls = herdr_calls(dir.path());
    // Whole log lines, so `plugin pane close` cannot pass for `pane close`.
    assert!(calls.lines().any(|l| l == "pane close w1:p1"), "{calls}");
    assert!(calls.lines().any(|l| l == "pane close w1:p3"), "{calls}");
    assert!(
        !calls.contains("pane close w1:p2"),
        "a labeled plain shell must not be swept: {calls}"
    );
}

#[test]
fn open_over_several_reviewr_panes_names_them_all() {
    let dir = tempfile::tempdir().unwrap();
    panes(dir.path(), &json!([{"pane_id": "w1:p1"}, {"pane_id": "w1:p2"}, {"pane_id": "w1:p3"}]));
    procinfo(dir.path(), "w1:p1", &json!([review_ui()]));
    procinfo(dir.path(), "w1:p3", &json!([review_ui()]));

    let output = run("open", dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: already open (w1:p1 w1:p3) in workspace-1\n");
}

#[test]
fn a_close_that_fails_for_a_live_pane_sweeps_the_rest_then_refuses() {
    let dir = tempfile::tempdir().unwrap();
    panes(dir.path(), &json!([{"pane_id": "w1:p1"}, {"pane_id": "w1:p3"}]));
    procinfo(dir.path(), "w1:p1", &json!([review_ui()]));
    procinfo(dir.path(), "w1:p3", &json!([review_ui()]));
    // A close that fails with the pane still there must refuse, never report it closed.
    fs::write(fixture(dir.path(), "closefail", "w1:p1", ""), herdr_error("internal")).unwrap();

    let output = run("close", dir.path());

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "reviewr: herdr pane close failed for w1:p1 in workspace-1\n");
    // The refusal comes after the sweep, so the panes herdr could close are closed.
    let calls = herdr_calls(dir.path());
    assert!(calls.lines().any(|l| l == "pane close w1:p3"), "{calls}");
}

#[test]
fn a_gone_pane_skips_and_an_unreadable_read_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let procfail = fixture(dir.path(), "procfail", "w1:p1", ".json");
    // A pane gone before its read converges.
    fs::write(&procfail, herdr_error("pane_not_found")).unwrap();
    let output = run("close", dir.path());
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: nothing open in workspace-1\n");

    // Any other read failure refuses.
    fs::write(&procfail, herdr_error("internal")).unwrap();
    for mode in ["open", "close", "toggle"] {
        let output = run(mode, dir.path());
        assert_eq!(output.status.code(), Some(1), "{mode}");
        assert_eq!(
            stderr(&output),
            "reviewr: herdr pane process-info failed in workspace-1\n",
            "{mode}"
        );
    }
}

#[test]
fn a_process_info_answer_missing_its_shape_refuses() {
    let dir = tempfile::tempdir().unwrap();
    // An error envelope with exit 0 refuses like a failed pane list.
    fs::write(fixture(dir.path(), "procinfo", "w1:p1", ".json"), herdr_error("internal")).unwrap();
    for mode in ["open", "close", "toggle"] {
        let output = run(mode, dir.path());
        assert_eq!(output.status.code(), Some(1), "{mode}");
        assert!(stderr(&output).contains("process-info failed"), "{mode}: {}", stderr(&output));
    }
}

#[test]
fn a_failed_pane_list_refuses_rather_than_reading_as_no_pane() {
    let dir = tempfile::tempdir().unwrap();
    // An error envelope for `pane list` refuses, never reads as "no reviewr pane".
    fs::write(dir.path().join("panes.json"), herdr_error("internal")).unwrap();

    let output = run("close", dir.path());

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "reviewr: herdr pane list failed for workspace-1\n");
}

#[cfg(unix)]
#[test]
fn an_action_repoints_the_stable_launch_paths_at_the_live_plugin_root() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    // Every valid run re-points the stable links at the runtime root.
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("bin")).unwrap();
    fs::write(root.path().join("bin/herdr-reviewr"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(root.path().join("bin/herdr-reviewr"), fs::Permissions::from_mode(0o755))
        .unwrap();
    let run_close = || {
        action("close", dir.path())
            .env("HERDR_PLUGIN_ROOT", root.path())
            .env("HOME", home.path())
            .output()
            .unwrap()
    };

    let output = run_close();

    assert!(output.status.success(), "{}", stderr(&output));
    let state_link =
        home.path().join(".local/state/herdr/plugins/persiyanov.reviewr/bin/herdr-reviewr");
    assert_eq!(fs::read_link(&state_link).unwrap(), root.path().join("bin/herdr-reviewr"));
    let bin_link = home.path().join(".local/bin/herdr-reviewr");
    assert!(!bin_link.exists(), "~/.local/bin must not be created for the link");

    // With `~/.local/bin` present the second link lands, re-pointing a symlink.
    fs::create_dir_all(home.path().join(".local/bin")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/old", &bin_link).unwrap();
    let output = run_close();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read_link(&bin_link).unwrap(), root.path().join("bin/herdr-reviewr"));

    // A link that already names the live binary is left in place.
    let inode =
        |path: &Path| std::os::unix::fs::MetadataExt::ino(&fs::symlink_metadata(path).unwrap());
    let before = inode(&bin_link);
    let output = run_close();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(inode(&bin_link), before, "a current link was replaced");

    // A re-point swaps the link in one step and leaves nothing beside it.
    let other = tempfile::tempdir().unwrap();
    fs::create_dir_all(other.path().join("bin")).unwrap();
    fs::copy(root.path().join("bin/herdr-reviewr"), other.path().join("bin/herdr-reviewr"))
        .unwrap();
    let done = std::sync::atomic::AtomicBool::new(false);
    let missing = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut missing = 0;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                if fs::symlink_metadata(&bin_link).is_err() {
                    missing += 1;
                }
            }
            missing
        });
        for round in 0..20 {
            let live = if round % 2 == 0 { other.path() } else { root.path() };
            let output = action("close", dir.path())
                .env("HERDR_PLUGIN_ROOT", live)
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", stderr(&output));
            assert_eq!(fs::read_link(&bin_link).unwrap(), live.join("bin/herdr-reviewr"));
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap()
    });
    assert_eq!(missing, 0, "the link was missing mid-swap");
    let names: Vec<_> = fs::read_dir(home.path().join(".local/bin"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["herdr-reviewr"]);

    // Anything but a symlink at the path is a user's own, and survives.
    fs::remove_file(&bin_link).unwrap();
    fs::write(&bin_link, "mine").unwrap();
    let output = run_close();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(&bin_link).unwrap(), "mine");
}

// --- The config dir lookup the binary falls back to.

#[test]
fn the_cli_fallback_resolves_the_config_dir_when_the_env_names_none() {
    // With no `HERDR_PLUGIN_CONFIG_DIR`, an action asks herdr; its invalid config proves the read.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "unknown = true\n").unwrap();

    let output =
        action("close", dir.path()).env_remove("HERDR_PLUGIN_CONFIG_DIR").output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("config.toml"), "{}", stderr(&output));
}

#[test]
fn a_wedged_config_dir_lookup_degrades_to_the_defaults_inside_the_bound() {
    // A herdr that hangs past the bound names no directory, so the invalid file is never read.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "unknown = true\n").unwrap();
    fs::write(dir.path().join("configdir-hang"), "").unwrap();

    let output =
        action("close", dir.path()).env_remove("HERDR_PLUGIN_CONFIG_DIR").output().unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: nothing open in workspace-1\n");
}

// --- Placement: the shape of the `plugin pane open` call.

#[test]
fn an_open_reports_the_pane_it_opened() {
    let dir = tempfile::tempdir().unwrap();

    let output = run_open(dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: opened w1:p9 (split) in workspace-1\n");
    let open = open_call(dir.path());
    assert_eq!(
        open,
        format!(
            "plugin pane open --plugin persiyanov.reviewr --entrypoint pane --placement split \
             --target-pane w1:p1 --direction right --cwd {} --focus",
            env!("CARGO_MANIFEST_DIR")
        )
    );
}

#[test]
fn an_open_names_the_plugin_herdr_runs_it_as() {
    let dir = tempfile::tempdir().unwrap();
    let context = json!({"focused_pane_cwd": env!("CARGO_MANIFEST_DIR")}).to_string();

    let output = action("open", dir.path())
        .env("HERDR_PLUGIN_ID", "someone.reviewr-fork")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains("--plugin someone.reviewr-fork "),
        "{}",
        herdr_calls(dir.path())
    );
}

#[test]
fn valid_non_default_placement_and_direction_reach_herdr_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");

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
        reset(dir.path());
        let output = run_open(dir.path());
        assert!(output.status.success(), "{}", stderr(&output));
        let open = open_call(dir.path());
        assert!(open.contains(placement), "{open}");
        if let Some(direction) = direction {
            assert!(open.contains(direction), "{open}");
        }
    }
}

#[test]
fn zoomed_placement_attaches_to_the_focused_pane_else_the_first_pane() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "toggle_placement = \"zoomed\"\n").unwrap();
    panes(dir.path(), &json!([{"pane_id": "w1:p4"}, {"pane_id": "w1:p5"}]));
    let context = json!({"focused_pane_cwd": env!("CARGO_MANIFEST_DIR")}).to_string();

    // No focused pane (`HERDR_PANE_ID` unset): the workspace's first pane.
    let output =
        action("open", dir.path()).env("HERDR_PLUGIN_CONTEXT_JSON", &context).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let open = open_call(dir.path());
    assert!(open.contains("--placement zoomed --target-pane w1:p4 --cwd"), "{open}");
    assert!(!open.contains("--direction"), "only a split takes a direction: {open}");

    reset(dir.path());
    let output = action("open", dir.path())
        .env("HERDR_PANE_ID", "w1:p5")
        .env("HERDR_PLUGIN_CONTEXT_JSON", &context)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(open_call(dir.path()).contains("--target-pane w1:p5"), "{}", herdr_calls(dir.path()));
}

#[test]
fn a_split_with_no_pane_to_attach_to_refuses() {
    let dir = tempfile::tempdir().unwrap();
    panes(dir.path(), &json!([]));
    let context = json!({"focused_pane_cwd": env!("CARGO_MANIFEST_DIR")}).to_string();

    let output =
        action("open", dir.path()).env("HERDR_PLUGIN_CONTEXT_JSON", context).output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "reviewr: no pane to attach to in workspace-1\n");
    assert!(!herdr_calls(dir.path()).contains("plugin pane open"), "{}", herdr_calls(dir.path()));
}

#[test]
fn tab_placement_open_names_its_fresh_tab() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "toggle_placement = \"tab\"\n").unwrap();

    let output = run_open(dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(open_call(dir.path()).contains("--placement tab --workspace workspace-1"));
    let calls = herdr_calls(dir.path());
    assert!(calls.lines().any(|l| l == "tab rename w1:t9 reviewr"), "{calls}");
}

#[test]
fn split_placement_open_renames_no_tab() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config.toml"), "toggle_placement = \"split\"\n").unwrap();

    let output = run_open(dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!herdr_calls(dir.path()).contains("tab rename"), "{}", herdr_calls(dir.path()));
}

#[test]
fn a_manual_open_passes_focus() {
    let dir = tempfile::tempdir().unwrap();
    let context = json!({"focused_pane_cwd": env!("CARGO_MANIFEST_DIR")}).to_string();

    for mode in ["open", "toggle"] {
        reset(dir.path());
        let output = run_with_context(mode, dir.path(), &context);
        assert!(output.status.success(), "{mode}: {}", stderr(&output));
        let open = open_call(dir.path());
        let tokens: Vec<&str> = open.split_whitespace().collect();
        assert!(tokens.contains(&"--focus"), "{mode} must pass --focus: {open}");
        assert!(!tokens.contains(&"--no-focus"), "{mode} must not pass --no-focus: {open}");
    }
}

// --- An open returns once its pane reads as reviewr.

/// The reads of the opened pane `w1:p9` a run made.
fn opened_pane_reads(dir: &Path) -> usize {
    herdr_calls(dir).lines().filter(|l| *l == "pane process-info --pane w1:p9").count()
}

#[test]
fn an_open_waits_until_its_pane_reads_as_reviewr() {
    let dir = tempfile::tempdir().unwrap();
    // herdr's Windows process snapshot lags a fresh pane: its first reads come back empty.
    fs::write(dir.path().join("opened-empty-reads"), "2").unwrap();

    let output = run_open(dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: opened w1:p9 (split) in workspace-1\n");
    // Two empty reads, then the one that sees reviewr, and none after.
    assert_eq!(opened_pane_reads(dir.path()), 3, "{}", herdr_calls(dir.path()));
}

#[test]
fn an_open_whose_pane_never_reads_as_reviewr_succeeds_after_the_bound_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("opened-empty-reads"), "1000000").unwrap();

    let started = Instant::now();
    let output = run_open(dir.path());
    let elapsed = started.elapsed();

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "reviewr: opened w1:p9 (split) in workspace-1, not yet seen running\n"
    );
    assert!(elapsed >= Duration::from_millis(5900), "returned before the bound: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(15), "the wait is bounded: {elapsed:?}");
    assert!(opened_pane_reads(dir.path()) > 1, "{}", herdr_calls(dir.path()));
}

#[test]
fn an_open_whose_pane_dies_at_launch_refuses_at_once() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(fixture(dir.path(), "procfail", "w1:p9", ".json"), herdr_error("pane_not_found"))
        .unwrap();

    let started = Instant::now();
    let output = run_open(dir.path());

    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert_eq!(stderr(&output), "reviewr: pane w1:p9 exited at launch in workspace-1\n");
    assert!(started.elapsed() < Duration::from_secs(3), "waited out the bound");
}

// --- Actions serialize on the lock in the plugin state dir.

/// Workspace `ws`'s action lock, held by the test process as another action would hold it.
fn hold_lock(dir: &Path, ws: &str) -> fs::File {
    let file = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("action-{}.lock", hex::encode(ws))))
        .unwrap();
    file.lock().unwrap();
    file
}

/// Start `mode` with a focused pane in this crate's repo, its output captured.
fn start(mode: &str, dir: &Path) -> Child {
    with_context(mode, dir, &repo_context())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn two_concurrent_toggles_open_then_close() {
    let dir = tempfile::tempdir().unwrap();
    // herdr's Windows process snapshot lags a fresh pane, so the opened pane first reads empty.
    fs::write(dir.path().join("opened-empty-reads"), "2").unwrap();

    let first = start("toggle", dir.path());
    let second = start("toggle", dir.path());
    let mut lines = [first, second].map(|child| {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    });
    lines.sort();

    assert_eq!(
        lines,
        [
            "reviewr: closed w1:p9 in workspace-1\n".to_owned(),
            "reviewr: opened w1:p9 (split) in workspace-1\n".to_owned(),
        ]
    );
    let effects: Vec<_> = herdr_calls(dir.path())
        .lines()
        .filter(|line| line.starts_with("plugin pane open") || line.starts_with("pane close"))
        .map(|line| line.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(effects, ["plugin pane open", "pane close w1:p9"]);
}

#[test]
fn an_explicit_action_waits_for_a_held_lock_and_proceeds_once_released() {
    let dir = tempfile::tempdir().unwrap();
    let lock = hold_lock(dir.path(), "workspace-1");

    let mut child = start("toggle", dir.path());
    std::thread::sleep(Duration::from_millis(500));
    assert!(child.try_wait().unwrap().is_none(), "the toggle did not wait for the lock");
    assert!(herdr_calls(dir.path()).is_empty(), "{}", herdr_calls(dir.path()));
    drop(lock);
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: opened w1:p9 (split) in workspace-1\n");
}

#[test]
fn an_explicit_action_refuses_once_the_lock_stays_held_past_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let _lock = hold_lock(dir.path(), "workspace-1");

    let started = Instant::now();
    let children = ["toggle", "open", "close"].map(|mode| (mode, start(mode, dir.path())));
    for (mode, child) in children {
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{mode}");
        assert_eq!(
            stderr(&output),
            "reviewr: another reviewr action in workspace-1 is still running after 57s\n",
            "{mode}"
        );
        assert!(output.stdout.is_empty(), "{mode}");
    }
    let elapsed = started.elapsed();

    assert!(elapsed >= Duration::from_millis(56500), "refused before the bound: {elapsed:?}");
    assert!(herdr_calls(dir.path()).is_empty(), "{}", herdr_calls(dir.path()));
}

#[test]
fn auto_open_gives_way_to_a_held_lock() {
    // The holder is the user's own action in the new workspace, so its outcome stands.
    let dir = tempfile::tempdir().unwrap();
    let _lock = hold_lock(dir.path(), "workspace-9");
    let event = worktree_event("worktree_created", "workspace-9", env!("CARGO_MANIFEST_DIR"), None);

    let started = Instant::now();
    let output = run_auto_open(dir.path(), &event, None);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stdout.is_empty(), "{}", stdout(&output));
    assert!(started.elapsed() < Duration::from_secs(5), "auto-open waited on the lock");
    assert!(herdr_calls(dir.path()).is_empty(), "{}", herdr_calls(dir.path()));
}

#[test]
fn workspace_ids_differing_only_in_case_hold_separate_locks() {
    let dir = tempfile::tempdir().unwrap();
    let _lock = hold_lock(dir.path(), "wA");
    let event = worktree_event("worktree_created", "wa", env!("CARGO_MANIFEST_DIR"), None);

    let started = Instant::now();
    let output = run_auto_open(dir.path(), &event, None);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(started.elapsed() < Duration::from_secs(5), "`wa` waited on `wA`'s lock");
    assert!(herdr_calls(dir.path()).contains("pane list --workspace wa"));
}

#[test]
fn a_wedged_herdr_call_frees_the_lock_for_the_next_action() {
    let dir = tempfile::tempdir().unwrap();
    // The first toggle takes the lock, then its pane listing hangs past the call bound.
    fs::write(dir.path().join("list-hang"), "").unwrap();
    let mut wedged = start("toggle", dir.path());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !herdr_calls(dir.path()).contains("pane list") {
        assert!(Instant::now() < deadline, "the first toggle never listed panes");
        std::thread::sleep(Duration::from_millis(20));
    }

    let output = run_with_context("toggle", dir.path(), &repo_context());

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!wedged.wait().unwrap().success(), "the wedged toggle refuses");
}

#[test]
fn a_held_lock_holds_back_only_its_own_workspace() {
    // An action in another workspace never waits on this lock.
    let dir = tempfile::tempdir().unwrap();
    let _lock = hold_lock(dir.path(), "workspace-2");
    let event = worktree_event("worktree_created", "workspace-9", env!("CARGO_MANIFEST_DIR"), None);

    let started = Instant::now();
    let toggle = run_with_context("toggle", dir.path(), &repo_context());
    let born = run_auto_open(dir.path(), &event, None);

    assert!(started.elapsed() < Duration::from_secs(5), "an action waited on another workspace");
    assert!(toggle.status.success(), "{}", stderr(&toggle));
    assert_eq!(stdout(&toggle), "reviewr: opened w1:p9 (split) in workspace-1\n");
    assert!(born.status.success(), "{}", stderr(&born));
    // The event read its own workspace instead of yielding to the held lock.
    let log = herdr_calls(dir.path());
    assert!(log.contains("pane list --workspace workspace-9"), "{log}");
}

#[test]
fn auto_open_over_an_open_reviewr_pane_does_nothing_and_says_nothing() {
    // The birth event neither stacks a second pane nor closes the first.
    let dir = tempfile::tempdir().unwrap();
    procinfo(dir.path(), "w1:p1", &json!([review_ui()]));
    let event = worktree_event("worktree_created", "workspace-9", env!("CARGO_MANIFEST_DIR"), None);

    let output = run_auto_open(dir.path(), &event, None);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stdout.is_empty(), "{}", stdout(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    let log = herdr_calls(dir.path());
    assert!(!log.contains("plugin pane open") && !log.contains("pane close"), "{log}");
    // It got as far as seeing the open pane.
    assert!(log.contains("pane list --workspace workspace-9"), "{log}");
    assert!(log.contains("pane process-info"), "{log}");
}

#[test]
fn a_lock_held_by_a_crashed_action_frees_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    // The first toggle takes the lock, then hangs in its pane listing until it is killed.
    fs::write(dir.path().join("list-hang"), "").unwrap();
    let mut crashed = start("toggle", dir.path());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !herdr_calls(dir.path()).contains("pane list") {
        assert!(Instant::now() < deadline, "the first toggle never listed panes");
        std::thread::sleep(Duration::from_millis(20));
    }
    crashed.kill().unwrap();
    crashed.wait().unwrap();

    let output = run_with_context("toggle", dir.path(), &repo_context());

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "reviewr: opened w1:p9 (split) in workspace-1\n");
}

#[test]
fn an_action_without_a_plugin_state_dir_refuses_before_any_herdr_call() {
    let dir = tempfile::tempdir().unwrap();

    let output = with_context("toggle", dir.path(), &repo_context())
        .env_remove("HERDR_PLUGIN_STATE_DIR")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "reviewr: no plugin state dir (invoke as a herdr plugin action)\n");
    assert!(herdr_calls(dir.path()).is_empty(), "{}", herdr_calls(dir.path()));
}

// --- Open cwd: the focused pane's live foreground cwd, then the context's launch cwd.

#[test]
fn open_prefers_the_focused_panes_live_foreground_cwd() {
    let dir = tempfile::tempdir().unwrap();
    // The launch cwd is no repo; the pane's live cwd is (the `claude -w` shape).
    let context = json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": dir.path()}).to_string();
    pane_with_cwd(dir.path(), "w1:p1", Path::new(env!("CARGO_MANIFEST_DIR")));

    let output = run_with_context("open", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    let calls = herdr_calls(dir.path());
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the open must use the live foreground cwd: {calls}"
    );
    // The live cwd comes from the run's one listing.
    assert_eq!(
        calls.matches("pane list --workspace").count(),
        1,
        "the open must reuse the held pane-list snapshot: {calls}"
    );
}

#[test]
fn open_prefers_the_live_cwd_when_the_launch_cwd_is_also_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    // The launch cwd is a repo too, so only a live-cwd read picks the right one.
    let launch_repo = Repo::init();
    let context =
        json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": launch_repo.path()}).to_string();
    pane_with_cwd(dir.path(), "w1:p1", Path::new(env!("CARGO_MANIFEST_DIR")));

    let output = run_with_context("open", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the live cwd must win over a launch cwd that is also a repo: {}",
        herdr_calls(dir.path())
    );
}

#[test]
fn open_keeps_the_context_cwd_without_a_live_foreground_cwd() {
    let dir = tempfile::tempdir().unwrap();
    // No live cwd keeps the context cwd.
    let context =
        json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": env!("CARGO_MANIFEST_DIR")})
            .to_string();

    let output = run_with_context("open", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the open must fall back to the context cwd: {}",
        herdr_calls(dir.path())
    );
}

#[test]
fn open_falls_back_to_the_workspace_cwd_without_a_focused_pane_cwd() {
    let dir = tempfile::tempdir().unwrap();
    // A workspace-context invocation carries no focused pane cwd, only the workspace's.
    let context = json!({"workspace_cwd": env!("CARGO_MANIFEST_DIR")}).to_string();

    let output = run_with_context("open", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "{}",
        herdr_calls(dir.path())
    );
}

#[test]
fn a_toggle_open_falls_back_when_the_live_cwd_is_not_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    // A live cwd outside any repo yields to the context cwd.
    let context =
        json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": env!("CARGO_MANIFEST_DIR")})
            .to_string();
    pane_with_cwd(dir.path(), "w1:p1", dir.path());

    let output = run_with_context("toggle", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "a non-repo live cwd must fall back to the context cwd: {}",
        herdr_calls(dir.path())
    );
}

#[test]
fn open_takes_the_focused_panes_cwd_not_another_panes() {
    let dir = tempfile::tempdir().unwrap();
    // The lookup keys on the focused pane's id, not the first entry.
    let decoy_repo = Repo::init();
    panes(
        dir.path(),
        &json!([
            {"pane_id": "w1:p0", "foreground_cwd": decoy_repo.path()},
            {"pane_id": "w1:p1", "foreground_cwd": env!("CARGO_MANIFEST_DIR")},
        ]),
    );
    let context = json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": dir.path()}).to_string();

    let output = run_with_context("open", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the open must use the focused pane's cwd, not the decoy's: {}",
        herdr_calls(dir.path())
    );
}

#[test]
fn a_refusal_names_the_rejected_live_cwd_too() {
    let dir = tempfile::tempdir().unwrap();
    // The refusal names the live directory it rejected.
    let context = json!({"focused_pane_id": "w1:p1"}).to_string();
    pane_with_cwd(dir.path(), "w1:p1", dir.path());

    let output = run_with_context("open", dir.path(), &context);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!("reviewr: no repo to review: '{}' (not a git repo)\n", dir.path().display())
    );
}

// --- Open cwd past a plugin's checkout (#130): herdr's managed installs are never reviewed.

/// One `plugin list --json` answer: a GitHub install managed at each of `managed`, and a linked
/// plugin rooted at each of `linked`.
fn plugins(dir: &Path, managed: &[&Path], linked: &[&Path]) {
    let managed = managed.iter().map(|path| {
        json!({"plugin_id": "a.managed", "plugin_root": path,
               "source": {"kind": "github", "managed_path": path}})
    });
    let linked = linked.iter().map(
        |path| json!({"plugin_id": "a.linked", "plugin_root": path, "source": {"kind": "local"}}),
    );
    let all: Vec<Value> = managed.chain(linked).collect();
    fs::write(dir.join("plugins.json"), json!({"result": {"plugins": all}}).to_string()).unwrap();
}

#[test]
fn open_skips_a_focused_plugin_checkout_for_the_agents_repo() {
    let dir = tempfile::tempdir().unwrap();
    // The focused pane sits in a managed checkout; a plain pane listed first loses to the agent.
    let plugin = Repo::init();
    let shell_repo = Repo::init();
    plugins(dir.path(), &[plugin.path()], &[]);
    panes(
        dir.path(),
        &json!([
            {"pane_id": "w1:p0", "foreground_cwd": shell_repo.path()},
            {"pane_id": "w1:p1", "foreground_cwd": plugin.path()},
            {"pane_id": "w1:p2", "agent": "claude", "foreground_cwd": env!("CARGO_MANIFEST_DIR")},
        ]),
    );
    let context =
        json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": plugin.path()}).to_string();

    let output = run_with_context("toggle", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "the open must skip the plugin's checkout for the agent's repo: {}",
        herdr_calls(dir.path())
    );
}

#[test]
fn open_skips_a_plugin_checkout_for_another_panes_repo() {
    let dir = tempfile::tempdir().unwrap();
    // No agent pane: a plain pane's repo is next, as the workspace's root shell.
    let plugin = Repo::init();
    plugins(dir.path(), &[plugin.path()], &[]);
    panes(
        dir.path(),
        &json!([
            {"pane_id": "w1:p0", "foreground_cwd": env!("CARGO_MANIFEST_DIR")},
            {"pane_id": "w1:p1", "foreground_cwd": plugin.path()},
        ]),
    );
    let context =
        json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": plugin.path()}).to_string();

    let output = run_with_context("toggle", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "{}",
        herdr_calls(dir.path())
    );
}

#[test]
fn open_skips_a_plugin_checkout_for_the_workspace_cwd() {
    let dir = tempfile::tempdir().unwrap();
    // No agent pane: the workspace's cwd is the last candidate.
    let plugin = Repo::init();
    plugins(dir.path(), &[plugin.path()], &[]);
    pane_with_cwd(dir.path(), "w1:p1", &plugin.path().join("sub"));
    fs::create_dir(plugin.path().join("sub")).unwrap();
    let context = json!({
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": plugin.path(),
        "workspace_cwd": env!("CARGO_MANIFEST_DIR"),
    })
    .to_string();

    let output = run_with_context("open", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "a folder inside the checkout is the plugin's too: {}",
        herdr_calls(dir.path())
    );
}

#[test]
fn open_keeps_a_linked_plugins_folder() {
    let dir = tempfile::tempdir().unwrap();
    // A linked plugin's root is the developer's own checkout, so it is reviewed.
    plugins(dir.path(), &[], &[Path::new(env!("CARGO_MANIFEST_DIR"))]);

    let output = run_open(dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "{}",
        herdr_calls(dir.path())
    );
}

#[test]
fn open_skips_a_non_repo_focused_pane_for_the_agents_repo() {
    let dir = tempfile::tempdir().unwrap();
    // A shell in a non-repo folder yields to the agent's repo, with no plugin involved.
    panes(
        dir.path(),
        &json!([
            {"pane_id": "w1:p0", "agent": "claude", "foreground_cwd": env!("CARGO_MANIFEST_DIR")},
            {"pane_id": "w1:p1", "foreground_cwd": dir.path()},
        ]),
    );
    let context = json!({"focused_pane_id": "w1:p1", "focused_pane_cwd": dir.path()}).to_string();

    let output = run_with_context("open", dir.path(), &context);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "{}",
        herdr_calls(dir.path())
    );
}

#[test]
fn a_refusal_names_a_skipped_plugin_checkout() {
    let dir = tempfile::tempdir().unwrap();
    // herdr reports the checkout as the focused pane's cwd and the workspace's: named once.
    let plugin = Repo::init();
    plugins(dir.path(), &[plugin.path()], &[]);
    let context = json!({
        "focused_pane_id": "w1:p1",
        "focused_pane_cwd": plugin.path(),
        "workspace_cwd": plugin.path(),
    })
    .to_string();

    let output = run_with_context("open", dir.path(), &context);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "reviewr: no repo to review: '{}' (a plugin's checkout)\n",
            plugin.path().display()
        )
    );
    assert!(!herdr_calls(dir.path()).contains("plugin pane open"), "{}", herdr_calls(dir.path()));
}

#[test]
fn a_failed_plugin_list_still_opens_on_the_focused_pane() {
    let dir = tempfile::tempdir().unwrap();
    // The filter only guards a misplaced open, so a failed listing never blocks one.
    fs::write(dir.path().join("pluginsfail"), herdr_error("internal")).unwrap();

    let output = run_open(dir.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        open_call(dir.path()).contains(&format!("--cwd {}", env!("CARGO_MANIFEST_DIR"))),
        "{}",
        herdr_calls(dir.path())
    );
}
