use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn reviewr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-reviewr")
}

/// A fake `herdr` that logs its argv and answers `pane list` / `plugin pane open` with
/// canned JSON. A shell script on unix; a `.cmd` shim delegating to `PowerShell` on Windows
/// (Rust's `Command` runs `.cmd` files via `cmd.exe`).
fn fake_herdr(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("herdr.log");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("herdr");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  'pane list'*) printf '%s\\n' '{{\"result\":{{\"panes\":[{{\"pane_id\":\"agent-1\",\"label\":\"agent\"}}]}}}}' ;;\n  *) printf '%s\\n' '{{\"result\":{{\"plugin_pane\":{{\"pane\":{{\"pane_id\":\"reviewr-1\"}}}}}}}}' ;;\nesac\n",
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        (path, log)
    }
    #[cfg(windows)]
    {
        let script = dir.join("fake_herdr.ps1");
        fs::write(
            &script,
            format!(
                "$args -join ' ' | Add-Content -Path '{log}'\nif (($args -join ' ') -like 'pane list*') {{\n  Write-Output '{{\"result\":{{\"panes\":[{{\"pane_id\":\"agent-1\",\"label\":\"agent\"}}]}}}}'\n}} else {{\n  Write-Output '{{\"result\":{{\"plugin_pane\":{{\"pane\":{{\"pane_id\":\"reviewr-1\"}}}}}}}}'\n}}\n",
                log = log.display()
            ),
        )
        .unwrap();
        let path = dir.join("herdr.cmd");
        fs::write(
            &path,
            format!(
                "@powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\" %*\r\n",
                script.display()
            ),
        )
        .unwrap();
        (path, log)
    }
}

fn run(mode: &str, config_dir: &Path, herdr: &Path) -> Output {
    Command::new(reviewr_bin())
        .arg("sidebar")
        .arg(mode)
        .env("HERDR_PLUGIN_CONFIG_DIR", config_dir)
        .env("HERDR_BIN_PATH", herdr)
        .env("HERDR_WORKSPACE_ID", "workspace-1")
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

    let output = Command::new(reviewr_bin())
        .arg("sidebar")
        .arg("auto-open")
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

#[test]
fn valid_non_default_placement_and_direction_reach_herdr_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let (herdr, log) = fake_herdr(dir.path());
    let cwd = env!("CARGO_MANIFEST_DIR");
    let context = serde_json::json!({"focused_pane_cwd": cwd}).to_string();

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
        let output = Command::new(reviewr_bin())
            .arg("sidebar")
            .arg("open")
            .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
            .env("HERDR_BIN_PATH", &herdr)
            .env("HERDR_WORKSPACE_ID", "workspace-1")
            .env("HERDR_PANE_ID", "agent-1")
            .env("HERDR_PLUGIN_CONTEXT_JSON", &context)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let calls = fs::read_to_string(&log).unwrap();
        assert!(calls.contains(placement), "{calls}");
        if let Some(direction) = direction {
            assert!(calls.contains(direction), "{calls}");
        }
    }
}
