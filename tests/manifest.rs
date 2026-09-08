//! Pure `herdr-plugin.toml` shape assertions — no subprocess, no fake herdr, so (unlike
//! `tests/pane_actions.rs`) these run on every platform, including the Windows CI job.

use std::fs;
use std::path::Path;

fn read_manifest() -> toml::Table {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin.toml"))
        .unwrap()
        .parse()
        .unwrap()
}

const PANE_ACTION_AUTO_OPEN: [&str; 3] = ["bin/herdr-reviewr", "--pane-action", "auto-open"];

#[test]
fn manifest_auto_open_hooks_created_and_opened() {
    let manifest = read_manifest();
    let events = manifest.get("events").and_then(toml::Value::as_array).expect("manifest events");
    let mut auto_open_events = events
        .iter()
        .filter_map(|event| {
            let table = event.as_table()?;
            let command = table.get("command")?.as_array()?;
            let command = command.iter().map(toml::Value::as_str).collect::<Option<Vec<_>>>()?;
            (command == PANE_ACTION_AUTO_OPEN).then(|| table.get("on")?.as_str()).flatten()
        })
        .collect::<Vec<_>>();
    auto_open_events.sort_unstable();

    assert_eq!(auto_open_events, ["worktree.created", "worktree.opened"]);
}

#[test]
fn manifest_declares_windows_platform() {
    let manifest = read_manifest();
    let platforms = manifest.get("platforms").and_then(toml::Value::as_array).expect("platforms");
    let platforms: Vec<&str> = platforms.iter().filter_map(toml::Value::as_str).collect();
    assert!(platforms.contains(&"windows"), "{platforms:?}");
    assert!(platforms.contains(&"macos"), "{platforms:?}");
    assert!(platforms.contains(&"linux"), "{platforms:?}");
}

#[test]
fn manifest_actions_use_shared_relative_command_all_platforms() {
    // No `platforms` field on any action/event: Herdr resolves a relative `bin/herdr-reviewr`
    // against the plugin root and PATHEXT-completes it on Windows, so one declaration covers
    // every platform (confirmed empirically — spec.md's Open Questions).
    let manifest = read_manifest();
    let actions =
        manifest.get("actions").and_then(toml::Value::as_array).expect("manifest actions");
    for id in ["toggle", "open", "close"] {
        let action = actions
            .iter()
            .find(|a| a.get("id").and_then(toml::Value::as_str) == Some(id))
            .unwrap_or_else(|| panic!("missing action {id}"));
        assert!(action.get("platforms").is_none(), "{id} must not be platform-scoped");
        let command = action.get("command").and_then(toml::Value::as_array).unwrap();
        let command: Vec<&str> = command.iter().filter_map(toml::Value::as_str).collect();
        assert_eq!(command, ["bin/herdr-reviewr", "--pane-action", id]);
    }
    let events = manifest.get("events").and_then(toml::Value::as_array).expect("manifest events");
    for event in events {
        assert!(event.get("platforms").is_none(), "events must not be platform-scoped");
        let command = event.get("command").and_then(toml::Value::as_array).unwrap();
        let command: Vec<&str> = command.iter().filter_map(toml::Value::as_str).collect();
        assert_eq!(command, PANE_ACTION_AUTO_OPEN);
    }
}

#[test]
fn manifest_windows_pane_command_resolves_with_spaces_in_root() {
    // The launcher shape itself — `& "$env:HERDR_PLUGIN_ROOT\..."` inside one `-Command`
    // string — is what makes a spaced plugin root safe: PowerShell keeps the whole expanded
    // path as a single quoted token for the `&` call operator. Live end-to-end proof (a
    // plugin linked from a path containing a space, launched into a disposable workspace)
    // is recorded in spec.md; this test pins the manifest shape that proof depends on.
    let manifest = read_manifest();
    let panes = manifest.get("panes").and_then(toml::Value::as_array).expect("manifest panes");
    let windows_pane = panes
        .iter()
        .find(|p| p.get("id").and_then(toml::Value::as_str) == Some("pane-windows"))
        .expect("pane-windows entry");
    let command = windows_pane.get("command").and_then(toml::Value::as_array).unwrap();
    let command: Vec<&str> = command.iter().filter_map(toml::Value::as_str).collect();
    assert_eq!(command[0], "powershell");
    assert!(command.contains(&"-Command"), "{command:?}");
    let script = command.last().unwrap();
    assert!(script.starts_with("& \""), "must quote the whole expanded path: {script}");
    assert!(script.contains("$env:HERDR_PLUGIN_ROOT"), "{script}");
    assert!(script.ends_with("herdr-reviewr.exe\""), "{script}");

    let unix_pane = panes
        .iter()
        .find(|p| p.get("id").and_then(toml::Value::as_str) == Some("pane-unix"))
        .expect("pane-unix entry");
    assert_ne!(
        unix_pane.get("id").and_then(toml::Value::as_str),
        windows_pane.get("id").and_then(toml::Value::as_str),
        "Herdr rejects duplicate pane ids even under different `platforms` scopes"
    );
}

#[test]
fn manifest_no_remaining_pane_sh_references() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("herdr/pane.sh").exists(), "herdr/pane.sh must be deleted");
    let manifest_text = fs::read_to_string(root.join("herdr-plugin.toml")).unwrap();
    assert!(!manifest_text.contains("pane.sh"), "{manifest_text}");
    for doc in ["AGENTS.md", "README.md", "docs/qa-install.md"] {
        let path = root.join(doc);
        let Ok(text) = fs::read_to_string(&path) else { continue };
        assert!(!text.contains("pane.sh"), "{doc} still references pane.sh");
    }
}
