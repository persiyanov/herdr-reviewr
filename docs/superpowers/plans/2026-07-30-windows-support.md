# Windows Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `herdr plugin install dcieslak19973/herdr-reviewr` works on native Windows: binary downloads, sidebar pane and all actions work — per the approved spec `docs/superpowers/specs/2026-07-30-windows-support-design.md`.

**Architecture:** The `sidebar.sh` orchestration moves into the binary as `herdr-reviewr sidebar <mode>` (pure decision functions + a thin herdr-CLI shell-out layer), so every platform runs one implementation and the `jq`/`bash` runtime dependency disappears. The manifest grows per-item platform variants (herdr: item-level `platforms` override the top-level list); Windows gets `herdr/install.ps1` and a `x86_64-pc-windows-msvc` release artifact.

**Tech Stack:** Rust 1.90 (pinned via rust-toolchain.toml, edition 2024), serde_json (existing dep — no new dependencies), PowerShell 5.1 for install.ps1, GitHub Actions (taiki-e/upload-rust-binary-action).

## Global Constraints

- No new Cargo dependencies. `serde_json`, `anyhow`, `toml` are already available; `tempfile` is a dev-dependency.
- Clippy runs with `pedantic` and `-D warnings` (see `[lints]` in Cargo.toml); code must pass `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --all --check`.
- All user-facing refusal/success strings must match `herdr/sidebar.sh` verbatim (existing tests assert on them; parity is a spec requirement).
- Windows manifest commands use TOML **literal strings** (single quotes) so backslashes and `$env:` survive unescaped.
- `min_herdr_version` becomes `"0.7.1"` (the version verified to honor `"windows"` + item-level platforms).
- The manifest `version` field stays `0.13.1` in this plan; release/version bump follows `docs/RELEASING.md` separately.
- Commit messages follow the repo's conventional style (`feat:`, `fix:`, `docs:`, `ci:`).

---

### Task 1: Sidebar decision core (`src/sidebar.rs` pure functions)

**Files:**
- Create: `src/sidebar.rs` (pure functions + unit tests only in this task)
- Modify: `src/lib.rs:21` (add `pub mod sidebar;` to the module list, alphabetical: after `pub mod proc;`)

**Interfaces:**
- Consumes: `crate::config::{PluginConfig, TogglePlacement, ToggleDirection}` — `TogglePlacement::{Split, Overlay, Zoomed, Tab}` with `as_str()`; `ToggleDirection::{Right, Down}` with `as_str()`.
- Produces (used by Task 2's runtime in the same file):
  - `enum Mode { Toggle, Open, Close, AutoOpen }` with `fn parse(arg: Option<&str>) -> Result<Mode, String>` and `fn is_auto(self) -> bool`
  - `struct Context { workspace: Option<String>, pane: Option<String>, cwd: Option<String> }`
  - `fn context_cwd(json: &str) -> Option<String>`
  - `fn event_context(json: &str) -> Context`
  - `fn sidebar_panes(panes_json: &str) -> Vec<String>`
  - `fn first_pane_id(panes_json: &str) -> Option<String>`
  - `fn open_args(placement: TogglePlacement, direction: ToggleDirection, workspace: &str, target: Option<&str>) -> Result<Vec<String>, String>`
  - `fn focus_flag(is_auto: bool, placement: TogglePlacement) -> &'static str`
  - `fn auto_open_gated(auto_open: bool, placement: TogglePlacement) -> bool`

- [ ] **Step 1: Create `src/sidebar.rs` with module doc and the failing unit tests**

```rust
//! `herdr-reviewr sidebar <toggle|open|close|auto-open>` — the sidebar actions and event hook.
//!
//! Rust port of the former `herdr/sidebar.sh` (`specs/herdr-host.md`, contracts A3/A5/A7,
//! P5/P6): one implementation for every platform, no `bash` or `jq` at runtime. The
//! workspace's sidebar is any pane labeled "reviewr" in the live pane list; there is no
//! state file. Actions refuse loudly (exit 1, one stderr line) and report successes on
//! stdout; the `auto-open` event refuses silently (exit 0), except a config error, which
//! reports through stderr for herdr's plugin log.

use crate::config::{ToggleDirection, TogglePlacement};

/// The invocation mode, defaulting to `toggle` exactly like the script's `${1:-toggle}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Toggle,
    Open,
    Close,
    AutoOpen,
}

impl Mode {
    fn parse(arg: Option<&str>) -> Result<Self, String> {
        match arg.unwrap_or("toggle") {
            "toggle" => Ok(Self::Toggle),
            "open" => Ok(Self::Open),
            "close" => Ok(Self::Close),
            "auto-open" => Ok(Self::AutoOpen),
            other => Err(format!("unknown mode '{other}' (toggle | open | close | auto-open)")),
        }
    }

    fn is_auto(self) -> bool {
        matches!(self, Self::AutoOpen)
    }
}

/// Workspace/pane/cwd context, read from the action env or the event payload.
#[derive(Debug, Default, PartialEq, Eq)]
struct Context {
    workspace: Option<String>,
    pane: Option<String>,
    cwd: Option<String>,
}

/// A non-empty string at `value`, or `None`.
fn non_empty_str(value: Option<&serde_json::Value>) -> Option<String> {
    value.and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).map(String::from)
}

/// `.focused_pane_cwd // .workspace_cwd` from the action context payload.
fn context_cwd(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    non_empty_str(value.get("focused_pane_cwd")).or_else(|| non_empty_str(value.get("workspace_cwd")))
}

/// The event fires without a focused pane; target the fresh workspace from its payload
/// (`worktree.created` shape: `.data.workspace.workspace_id` and
/// `.data.workspace.worktree.checkout_path`, with `.data.worktree.{open_workspace_id, path}`
/// as fallbacks).
fn event_context(json: &str) -> Context {
    let value: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    Context {
        workspace: non_empty_str(value.pointer("/data/workspace/workspace_id"))
            .or_else(|| non_empty_str(value.pointer("/data/worktree/open_workspace_id"))),
        pane: None,
        cwd: non_empty_str(value.pointer("/data/workspace/worktree/checkout_path"))
            .or_else(|| non_empty_str(value.pointer("/data/worktree/path"))),
    }
}

/// Every pane id labeled "reviewr" in a `pane list` snapshot — the workspace's sidebar,
/// any tab, any placement (spec A5). A parse failure reads as "no sidebar", matching the
/// script's `jq … 2>/dev/null` behavior.
fn sidebar_panes(panes_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(panes_json)
        .ok()
        .and_then(|v| v.pointer("/result/panes").and_then(serde_json::Value::as_array).cloned())
        .map(|panes| {
            panes
                .iter()
                .filter(|p| p.get("label").and_then(serde_json::Value::as_str) == Some("reviewr"))
                .filter_map(|p| non_empty_str(p.get("pane_id")))
                .collect()
        })
        .unwrap_or_default()
}

/// The workspace's first pane id, the split/zoomed attach fallback when no pane is focused.
fn first_pane_id(panes_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(panes_json)
        .ok()
        .and_then(|v| non_empty_str(v.pointer("/result/panes/0/pane_id")))
}

/// The `plugin pane open` placement argument tail (spec: Sidebar placement). A split or
/// zoomed open attaches to `target` (the acting pane, else the workspace's first pane);
/// tab targets the workspace; overlay needs neither.
fn open_args(
    placement: TogglePlacement,
    direction: ToggleDirection,
    workspace: &str,
    target: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = vec!["--placement".into(), placement.as_str().into()];
    match placement {
        TogglePlacement::Split | TogglePlacement::Zoomed => {
            let Some(target) = target else {
                return Err(format!("no pane to attach to in {workspace}"));
            };
            args.push("--target-pane".into());
            args.push(target.into());
            if placement == TogglePlacement::Split {
                args.push("--direction".into());
                args.push(direction.as_str().into());
            }
        }
        TogglePlacement::Tab => {
            args.push("--workspace".into());
            args.push(workspace.into());
        }
        TogglePlacement::Overlay => {}
    }
    Ok(args)
}

/// Focus follows the placement on a manual open; the event never takes it (A3, P5, P6).
fn focus_flag(is_auto: bool, placement: TogglePlacement) -> &'static str {
    if !is_auto && placement != TogglePlacement::Split { "--focus" } else { "--no-focus" }
}

/// Event policy gates the event alone: explicit actions ignore it. Runs after config
/// validation but before workspace or pane inspection, so a disabled event does no work.
fn auto_open_gated(auto_open: bool, placement: TogglePlacement) -> bool {
    !auto_open || !matches!(placement, TogglePlacement::Split | TogglePlacement::Tab)
}

#[cfg(test)]
mod tests {
    use super::{
        Context, Mode, auto_open_gated, context_cwd, event_context, first_pane_id, focus_flag,
        open_args, sidebar_panes,
    };
    use crate::config::{ToggleDirection, TogglePlacement};

    #[test]
    fn mode_defaults_to_toggle_and_rejects_unknowns() {
        assert_eq!(Mode::parse(None), Ok(Mode::Toggle));
        assert_eq!(Mode::parse(Some("auto-open")), Ok(Mode::AutoOpen));
        let error = Mode::parse(Some("bogus")).unwrap_err();
        assert_eq!(error, "unknown mode 'bogus' (toggle | open | close | auto-open)");
    }

    #[test]
    fn context_cwd_prefers_focused_pane_then_workspace() {
        let both = r#"{"focused_pane_cwd":"/a","workspace_cwd":"/b"}"#;
        assert_eq!(context_cwd(both), Some("/a".to_string()));
        let fallback = r#"{"focused_pane_cwd":"","workspace_cwd":"/b"}"#;
        assert_eq!(context_cwd(fallback), Some("/b".to_string()));
        assert_eq!(context_cwd("not json"), None);
    }

    #[test]
    fn event_context_reads_workspace_payload_with_worktree_fallback() {
        let primary = r#"{"data":{"workspace":{"workspace_id":"w1","worktree":{"checkout_path":"/wt"}}}}"#;
        assert_eq!(
            event_context(primary),
            Context { workspace: Some("w1".into()), pane: None, cwd: Some("/wt".into()) }
        );
        let fallback = r#"{"data":{"worktree":{"open_workspace_id":"w2","path":"/p"}}}"#;
        assert_eq!(
            event_context(fallback),
            Context { workspace: Some("w2".into()), pane: None, cwd: Some("/p".into()) }
        );
        assert_eq!(event_context("{}"), Context::default());
    }

    #[test]
    fn sidebar_panes_selects_reviewr_labels_only() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"p1","label":"agent"},
            {"pane_id":"p2","label":"reviewr"},
            {"pane_id":"p3","label":"reviewr"}
        ]}}"#;
        assert_eq!(sidebar_panes(json), vec!["p2".to_string(), "p3".to_string()]);
        assert!(sidebar_panes("garbage").is_empty());
        assert_eq!(first_pane_id(json), Some("p1".to_string()));
        assert_eq!(first_pane_id("garbage"), None);
    }

    #[test]
    fn open_args_split_takes_target_and_direction() {
        let args =
            open_args(TogglePlacement::Split, ToggleDirection::Right, "w1", Some("p9")).unwrap();
        assert_eq!(
            args,
            ["--placement", "split", "--target-pane", "p9", "--direction", "right"]
        );
    }

    #[test]
    fn open_args_zoomed_takes_target_without_direction() {
        let args =
            open_args(TogglePlacement::Zoomed, ToggleDirection::Down, "w1", Some("p9")).unwrap();
        assert_eq!(args, ["--placement", "zoomed", "--target-pane", "p9"]);
    }

    #[test]
    fn open_args_split_without_target_refuses() {
        let error =
            open_args(TogglePlacement::Split, ToggleDirection::Right, "w7", None).unwrap_err();
        assert_eq!(error, "no pane to attach to in w7");
    }

    #[test]
    fn open_args_tab_and_overlay_need_no_target() {
        let tab = open_args(TogglePlacement::Tab, ToggleDirection::Right, "w1", None).unwrap();
        assert_eq!(tab, ["--placement", "tab", "--workspace", "w1"]);
        let overlay =
            open_args(TogglePlacement::Overlay, ToggleDirection::Right, "w1", None).unwrap();
        assert_eq!(overlay, ["--placement", "overlay"]);
    }

    #[test]
    fn focus_follows_placement_on_manual_open_only() {
        assert_eq!(focus_flag(false, TogglePlacement::Split), "--no-focus");
        assert_eq!(focus_flag(false, TogglePlacement::Tab), "--focus");
        assert_eq!(focus_flag(true, TogglePlacement::Tab), "--no-focus");
    }

    #[test]
    fn auto_open_gates_on_flag_and_placement() {
        assert!(!auto_open_gated(true, TogglePlacement::Split));
        assert!(!auto_open_gated(true, TogglePlacement::Tab));
        assert!(auto_open_gated(true, TogglePlacement::Overlay));
        assert!(auto_open_gated(true, TogglePlacement::Zoomed));
        assert!(auto_open_gated(false, TogglePlacement::Split));
    }
}
```

Note: the items are intentionally private (`fn`, not `pub fn`) — Task 2's runtime lives in this same file. No `dead_code` warnings fire in this task even though nothing outside the module calls these yet: the unit tests reference every item, and `cfg(test)` usage counts. Verify with clippy in Step 3; do not add any `#[allow]`.

- [ ] **Step 2: Register the module and run the tests**

In `src/lib.rs`, after `pub mod proc;` add:

```rust
pub mod sidebar;
```

Run: `cargo test sidebar`
Expected: all 9 new tests PASS (the pure functions are implemented in the same step as their tests here because the file is new; the red-green cycle for this task is the clippy/dead-code gate plus the assertion content, which was written before the bodies were finalized).

- [ ] **Step 3: Full gate**

Run: `cargo fmt --all --check; if ($?) { cargo clippy --all-targets --all-features -- -D warnings }; if ($?) { cargo test }`
Expected: clean fmt, no clippy warnings (in particular no `dead_code`), all tests pass.

- [ ] **Step 4: Commit**

```
git add src/sidebar.rs src/lib.rs
git commit -m "feat: sidebar decision core in Rust (port of sidebar.sh logic)"
```

---

### Task 2: Sidebar runtime + `main` dispatch + cross-platform integration tests

**Files:**
- Modify: `src/sidebar.rs` (append the runtime below the pure functions)
- Modify: `src/main.rs:12` (dispatch `sidebar` before the TUI fallthrough)
- Modify: `src/cli.rs:17` (USAGE gains the sidebar line)
- Rewrite: `tests/sidebar.rs` (drive the binary instead of `bash herdr/sidebar.sh`; drop `#![cfg(unix)]`)

**Interfaces:**
- Consumes: every Task 1 item; `crate::config::plugin_config()` → `Result<PluginConfig, PluginConfigError>`; `PluginConfig::{toggle_placement(), toggle_direction(), auto_open()}`.
- Produces: `pub fn run(args: &[String]) -> std::process::ExitCode` in `crate::sidebar` — `args` is argv *after* the `sidebar` word (so `args.first()` is the mode). Environment contract (same as sidebar.sh): reads `HERDR_BIN_PATH`, `HERDR_WORKSPACE_ID`, `HERDR_PANE_ID`, `HERDR_PLUGIN_ID`, `HERDR_PLUGIN_CONFIG_DIR` (via `plugin_config`), `HERDR_PLUGIN_CONTEXT_JSON`, `HERDR_PLUGIN_EVENT_JSON`.

- [ ] **Step 1: Append the runtime to `src/sidebar.rs`**

Add `use std::process::{Command, ExitCode};` to the imports, then below the pure functions:

```rust
/// Entry point called from `main` with argv after the `sidebar` word.
pub fn run(args: &[String]) -> ExitCode {
    let mode = match Mode::parse(args.first().map(String::as_str)) {
        Ok(mode) => mode,
        Err(message) => return refuse(false, &message),
    };

    // Validate the whole plugin config before reading workspace state or taking any action.
    // A config error is loud for every mode, including the event — herdr's plugin log is
    // where a broken config.toml gets noticed (tests/sidebar.rs pins this).
    let cfg = match crate::config::plugin_config() {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("reviewr: {error}");
            return ExitCode::from(1);
        }
    };
    if mode.is_auto() && auto_open_gated(cfg.auto_open(), cfg.toggle_placement()) {
        return ExitCode::SUCCESS;
    }

    let mut ctx = Context {
        workspace: env_non_empty("HERDR_WORKSPACE_ID"),
        pane: env_non_empty("HERDR_PANE_ID"),
        cwd: env_non_empty("HERDR_PLUGIN_CONTEXT_JSON").and_then(|json| context_cwd(&json)),
    };
    if mode.is_auto()
        && let Some(event) = env_non_empty("HERDR_PLUGIN_EVENT_JSON")
    {
        ctx = event_context(&event);
    }
    let Some(workspace) = ctx.workspace.clone() else {
        return refuse(mode.is_auto(), "no workspace context (invoke from inside herdr)");
    };

    // One pane-list snapshot serves the whole run. A failed listing must not read as
    // "no sidebar" — that would stack a duplicate on toggle and false-succeed a close.
    let Some(panes_json) =
        herdr(&["pane", "list", "--workspace", &workspace]).filter(|out| !out.is_empty())
    else {
        return refuse(mode.is_auto(), &format!("herdr pane list failed for {workspace}"));
    };
    let existing = sidebar_panes(&panes_json);

    match mode {
        Mode::Close => {
            if existing.is_empty() {
                println!("close: nothing open in {workspace}");
                return ExitCode::SUCCESS;
            }
            close_all(&existing, &workspace, mode)
        }
        Mode::Toggle if !existing.is_empty() => close_all(&existing, &workspace, mode),
        Mode::Open | Mode::AutoOpen if !existing.is_empty() => {
            if mode == Mode::Open {
                println!("open: already open ({}) in {workspace}", existing.join(" "));
            }
            ExitCode::SUCCESS
        }
        Mode::Toggle | Mode::Open | Mode::AutoOpen => {
            open_sidebar(mode, &cfg, &ctx, &workspace, &panes_json)
        }
    }
}

/// A refusal: exit 1 with one stderr line for actions, a silent success for the event.
fn refuse(is_auto: bool, message: &str) -> ExitCode {
    if is_auto {
        return ExitCode::SUCCESS;
    }
    eprintln!("reviewr: {message}");
    ExitCode::from(1)
}

/// A non-empty environment variable, or `None`.
fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Run `herdr <args>` via `$HERDR_BIN_PATH`, returning stdout on success. Spawn failures
/// and non-zero exits both yield `None`; herdr's stderr is not surfaced (the script ran
/// every herdr call under `2>/dev/null`).
fn herdr(args: &[&str]) -> Option<String> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let out = Command::new(bin).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Close every listed sidebar via plain `pane close` — not `plugin pane close`, whose
/// registry does not survive a herdr restart and would strand the pane (spec A7).
fn close_all(existing: &[String], workspace: &str, mode: Mode) -> ExitCode {
    let (mut closed, mut failed) = (String::new(), String::new());
    for pane in existing {
        let bucket =
            if herdr(&["pane", "close", pane]).is_some() { &mut closed } else { &mut failed };
        bucket.push(' ');
        bucket.push_str(pane);
    }
    if !failed.is_empty() {
        return refuse(mode.is_auto(), &format!("failed to close{failed} in {workspace}"));
    }
    println!("closed{closed} in {workspace}");
    ExitCode::SUCCESS
}

/// Whether `cwd` is inside a git repository — the open-path gate.
fn is_git_repo(cwd: &str) -> bool {
    Command::new("git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel"])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// The opening path: gate on a git repo, assemble the placement, run `plugin pane open`.
fn open_sidebar(
    mode: Mode,
    cfg: &crate::config::PluginConfig,
    ctx: &Context,
    workspace: &str,
    panes_json: &str,
) -> ExitCode {
    let cwd = ctx.cwd.as_deref().unwrap_or("");
    if cwd.is_empty() || !is_git_repo(cwd) {
        let shown = if cwd.is_empty() { "<no cwd>" } else { cwd };
        return refuse(mode.is_auto(), &format!("not a git repo: '{shown}'"));
    }

    let target = ctx.pane.clone().or_else(|| first_pane_id(panes_json));
    let tail = match open_args(
        cfg.toggle_placement(),
        cfg.toggle_direction(),
        workspace,
        target.as_deref(),
    ) {
        Ok(tail) => tail,
        Err(message) => return refuse(mode.is_auto(), &message),
    };

    let plugin =
        env_non_empty("HERDR_PLUGIN_ID").unwrap_or_else(|| "dcieslak19973.reviewr".to_string());
    let mut args: Vec<String> = vec![
        "plugin".into(),
        "pane".into(),
        "open".into(),
        "--plugin".into(),
        plugin,
        "--entrypoint".into(),
        "sidebar".into(),
    ];
    args.extend(tail);
    args.push("--cwd".into());
    args.push(cwd.into());
    args.push(focus_flag(mode.is_auto(), cfg.toggle_placement()).into());

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let new_pane = herdr(&arg_refs)
        .and_then(|out| serde_json::from_str::<serde_json::Value>(&out).ok())
        .and_then(|v| non_empty_str(v.pointer("/result/plugin_pane/pane/pane_id")));
    let Some(new_pane) = new_pane else {
        return refuse(mode.is_auto(), "herdr plugin pane open failed");
    };
    if !mode.is_auto() {
        println!("opened {new_pane} ({}) in {workspace}", cfg.toggle_placement().as_str());
    }
    ExitCode::SUCCESS
}
```

Parity notes for the implementer (each mirrors a `sidebar.sh` line — do not "improve"):
- Unknown mode refuses **loudly** even though `refuse()` would silence auto-open — the mode itself failed to parse, so it is never auto (`refuse(false, …)`).
- `focus_flag`'s manual/split matrix: script line 133-134 (`--no-focus` default; manual + non-split → `--focus`).
- `close: nothing open` / `open: already open (…)` / `closed …` / `opened …` strings are stdout, refusals are stderr with the `reviewr: ` prefix.

- [ ] **Step 2: Dispatch in `src/main.rs`**

After the `comment | skill-path | skill-install` block (line 12-14), add:

```rust
    if args.get(1).map(String::as_str) == Some("sidebar") {
        return herdr_reviewr::sidebar::run(&args[2..]);
    }
```

- [ ] **Step 3: Extend `USAGE` in `src/cli.rs`**

In the `USAGE` const (line 17), add before the `skill-path` line:

```text
       herdr-reviewr sidebar [toggle|open|close|auto-open]\n
```

(i.e. the string gains `"       herdr-reviewr sidebar [toggle|open|close|auto-open]\n"` between the `comment rm` and `skill-path` entries.)

- [ ] **Step 4: Rewrite `tests/sidebar.rs` to drive the binary cross-platform**

Replace the file's preamble and helpers; **keep every existing `#[test]` body byte-for-byte** (they assert on stderr/exit codes that the port preserves). New preamble and helpers:

```rust
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn reviewr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-reviewr")
}

/// A fake `herdr` that logs its argv and answers `pane list` / `plugin pane open` with
/// canned JSON. A shell script on unix; a `.cmd` shim delegating to PowerShell on Windows
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
```

Adjustments to the rest of the file:
- Delete the `#![cfg(unix)]` first line and the now-unused `HERDR_REVIEWR_BIN` env (the binary resolves its own config; the script needed that env to find the binary).
- The Windows PowerShell fake writes the log with `Add-Content` — the format-string `{{`/`}}` doubling is for Rust's `format!`, exactly as the existing unix helper does.
- If any existing test below line 80 shells `bash` directly or checks script-only behavior, port its *intent* against the binary and note the change in the commit message. (Read the whole file first; the tests above line 80 need no body changes.)

- [ ] **Step 5: Run the integration tests**

Run: `cargo test --test sidebar`
Expected: PASS on Windows (this machine). The old file would not even compile here (`std::os::unix`), so passing proves the rewrite.

- [ ] **Step 6: Full gate**

Run: `cargo fmt --all --check; if ($?) { cargo clippy --all-targets --all-features -- -D warnings }; if ($?) { cargo test }`
Expected: all clean.

- [ ] **Step 7: Commit**

```
git add src/sidebar.rs src/main.rs src/cli.rs tests/sidebar.rs
git commit -m "feat: herdr-reviewr sidebar subcommand replaces sidebar.sh at runtime"
```

---

### Task 3: Manifest per-platform variants, delete `sidebar.sh`

**Files:**
- Modify: `herdr-plugin.toml` (full rewrite below)
- Delete: `herdr/sidebar.sh`

**Interfaces:**
- Consumes: `herdr-reviewr sidebar <mode>` (Task 2), `herdr/install.ps1` (Task 5 — referenced here, created there; `plugin link` skips build so the ordering is safe).
- Produces: the manifest all later verification runs against.

- [ ] **Step 1: Probe — does herdr accept duplicate item ids across platform variants?**

Before rewriting, verify the risk called out in the spec. Write the new manifest (Step 2), then:

Run: `herdr plugin link .` (from the repo root), then `herdr plugin list`, then `herdr plugin unlink dcieslak19973.reviewr`
Expected: link succeeds and `plugin list` shows the plugin with 4 actions / 1 pane (Windows variants active, unix variants filtered out). **If herdr rejects duplicate ids**, STOP — report back; the fallback design (distinct ids like `sidebar-win` plus env-driven entrypoint selection) needs a human decision.

- [ ] **Step 2: Rewrite `herdr-plugin.toml`**

```toml
id = "dcieslak19973.reviewr"
name = "reviewr"
version = "0.13.1"
min_herdr_version = "0.7.1"
platforms = ["macos", "linux", "windows"]
description = "Native terminal code-review sidebar for herdr."

# On `herdr plugin install`, download the prebuilt `herdr-reviewr` binary for this platform
# from the matching GitHub Release into $HERDR_PLUGIN_ROOT/bin (no Rust toolchain needed).
# Skipped by `herdr plugin link` — for a local checkout, build it yourself with
# `cargo install --path .`. Item-level `platforms` override the top-level list, so each
# platform runs its native script.
[[build]]
platforms = ["macos", "linux"]
command = ["bash", "herdr/install.sh"]

[[build]]
platforms = ["windows"]
command = ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "herdr/install.ps1"]

# The sidebar pane runs the downloaded binary by absolute path under the plugin root, since
# the pane's cwd is the repo under review (not the plugin root) and the binary isn't on
# PATH. Manifest commands are direct argv — the shell (sh / powershell) exists only to
# expand $HERDR_PLUGIN_ROOT.
[[panes]]
id = "sidebar"
title = "reviewr"
placement = "split"
platforms = ["macos", "linux"]
command = ["sh", "-c", "exec \"$HERDR_PLUGIN_ROOT/bin/herdr-reviewr\""]

[[panes]]
id = "sidebar"
title = "reviewr"
placement = "split"
platforms = ["windows"]
command = ["powershell", "-NoProfile", "-Command", '& "$env:HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe"']

# Actions receive $HERDR_PLUGIN_ROOT, so they run the binary by absolute path and work
# regardless of PATH. Sidebar orchestration lives in `herdr-reviewr sidebar <mode>`
# (formerly herdr/sidebar.sh).
[[actions]]
id = "skill-install"
title = "reviewr: install agent skill"
contexts = ["pane", "workspace"]
platforms = ["macos", "linux"]
command = ["bash", "-c", 'exec "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr" skill-install']

[[actions]]
id = "skill-install"
title = "reviewr: install agent skill"
contexts = ["pane", "workspace"]
platforms = ["windows"]
command = ["powershell", "-NoProfile", "-Command", '& "$env:HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe" skill-install']

[[actions]]
id = "toggle"
title = "reviewr: toggle sidebar"
contexts = ["pane", "workspace"]
platforms = ["macos", "linux"]
command = ["bash", "-c", 'exec "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr" sidebar toggle']

[[actions]]
id = "toggle"
title = "reviewr: toggle sidebar"
contexts = ["pane", "workspace"]
platforms = ["windows"]
command = ["powershell", "-NoProfile", "-Command", '& "$env:HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe" sidebar toggle']

[[actions]]
id = "open"
title = "reviewr: open sidebar"
contexts = ["pane", "workspace"]
platforms = ["macos", "linux"]
command = ["bash", "-c", 'exec "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr" sidebar open']

[[actions]]
id = "open"
title = "reviewr: open sidebar"
contexts = ["pane", "workspace"]
platforms = ["windows"]
command = ["powershell", "-NoProfile", "-Command", '& "$env:HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe" sidebar open']

[[actions]]
id = "close"
title = "reviewr: close sidebar"
contexts = ["pane", "workspace"]
platforms = ["macos", "linux"]
command = ["bash", "-c", 'exec "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr" sidebar close']

[[actions]]
id = "close"
title = "reviewr: close sidebar"
contexts = ["pane", "workspace"]
platforms = ["windows"]
command = ["powershell", "-NoProfile", "-Command", '& "$env:HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe" sidebar close']

# Auto-open the sidebar for a freshly created worktree (gated by auto_open; see
# `herdr-reviewr sidebar auto-open`).
[[events]]
on = "worktree.created"
platforms = ["macos", "linux"]
command = ["bash", "-c", 'exec "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr" sidebar auto-open']

[[events]]
on = "worktree.created"
platforms = ["windows"]
command = ["powershell", "-NoProfile", "-Command", '& "$env:HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe" sidebar auto-open']
```

- [ ] **Step 3: Delete the script**

```
git rm herdr/sidebar.sh
```

- [ ] **Step 4: Verify the manifest parses and links (repeat of the Step 1 probe on the final file)**

Run: `herdr plugin link .` then `herdr plugin list` then `herdr plugin unlink dcieslak19973.reviewr`
Expected: clean link, 4 actions, 1 pane, 1 event listed.

- [ ] **Step 5: Commit**

```
git add herdr-plugin.toml
git commit -m "feat: per-platform manifest commands; sidebar.sh retired"
```

---

### Task 4: Windows-aware tool probe + `clip` clipboard

**Files:**
- Modify: `src/proc.rs` (whole-file replacement below)
- Modify: `src/export.rs:45-53` (tool table + doc comment)

**Interfaces:**
- Consumes: nothing new.
- Produces: `crate::proc::on_path(name: &str) -> bool` (same signature, now PATHEXT-aware on Windows) — existing callers `export.rs` and `browser.rs` need no changes.

- [ ] **Step 1: Write the failing test**

Append to `src/proc.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::on_path;

    /// `clip.exe` ships in System32 on every Windows; `on_path("clip")` must find it even
    /// though the file on disk is `clip.exe`, not `clip`.
    #[test]
    #[cfg(windows)]
    fn bare_name_resolves_windows_executables() {
        assert!(on_path("clip"));
        assert!(!on_path("definitely-not-a-real-tool-name"));
    }

    #[test]
    #[cfg(unix)]
    fn bare_name_resolves_unix_executables() {
        assert!(on_path("sh"));
        assert!(!on_path("definitely-not-a-real-tool-name"));
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib proc`
Expected: FAIL on Windows — `on_path("clip")` is false because only `clip.exe` exists on disk.

- [ ] **Step 3: Replace the implementation**

```rust
//! Small helpers for locating external command-line tools.

/// Whether `name` resolves to an executable on `PATH` — a dependency-free `which`. On unix a
/// file in a `PATH` directory is the executable; on Windows the on-disk name usually carries
/// a `PATHEXT` extension (`clip` → `clip.exe`), so each extension is tried too. Shared by the
/// clipboard probe (`export.rs`) and the URL-opener probe (`browser.rs`).
#[must_use]
pub fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        if dir.join(name).is_file() {
            return true;
        }
        if cfg!(windows) {
            let pathext = std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            return pathext
                .split(';')
                .filter(|ext| !ext.is_empty())
                .any(|ext| dir.join(format!("{name}{}", ext.to_lowercase())).is_file()
                    || dir.join(format!("{name}{ext}")).is_file());
        }
        false
    })
}
```

- [ ] **Step 4: Run the test again**

Run: `cargo test --lib proc`
Expected: PASS.

- [ ] **Step 5: Add `clip` to the clipboard table in `src/export.rs`**

Replace lines 45-53:

```rust
/// A clipboard tool and the args that make it read stdin into the system clipboard. Tried in
/// order — the first one present on `PATH` wins. macOS ships `pbcopy`; Linux needs one of these
/// installed (Wayland `wl-copy`, or X11 `xclip`/`xsel`); Windows ships `clip`. OSC 52 is
/// roadmap.
const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
    ("clip", &[]),
];
```

- [ ] **Step 6: Manually verify the clipboard end-to-end on this machine**

Run: `"reviewr clipboard test" | clip; Get-Clipboard`
Expected: `Get-Clipboard` prints `reviewr clipboard test` (proves the tool contract `clip` reads stdin). The in-app path is covered by Task 8's E2E.

- [ ] **Step 7: Full gate + commit**

Run: `cargo fmt --all --check; if ($?) { cargo clippy --all-targets --all-features -- -D warnings }; if ($?) { cargo test }`

```
git add src/proc.rs src/export.rs
git commit -m "feat: Windows clipboard via clip.exe; PATHEXT-aware tool probe"
```

---

### Task 5: `herdr/install.ps1` + Windows release target

**Files:**
- Create: `herdr/install.ps1`
- Modify: `.github/workflows/release.yml:34-38` (matrix)

**Interfaces:**
- Consumes: GitHub release assets named `herdr-reviewr-x86_64-pc-windows-msvc.zip` + `herdr-reviewr-x86_64-pc-windows-msvc.sha256` (this task's release.yml change produces them; taiki-e uses `.zip` on Windows and the checksum sidecar drops the archive extension, mirroring install.sh's comment).
- Produces: `$HERDR_PLUGIN_ROOT/bin/herdr-reviewr.exe` on Windows installs. Test hook: `HERDR_REVIEWR_BASE_URL` env var overrides the download base (a local directory path works — assets are `Copy-Item`ed when the source exists on disk).

- [ ] **Step 1: Create `herdr/install.ps1`**

```powershell
# herdr `[[build]]` step (Windows): download the prebuilt herdr-reviewr.exe from the matching
# GitHub Release into the plugin's bin/ dir. Mirror of herdr/install.sh — same version
# resolution, retry, and checksum contract. Runs on `herdr plugin install`; `herdr plugin
# link` skips the build step — for a local checkout, build with `cargo install --path .`.
#
# The build runs with the plugin checkout as the working directory, so the plugin root is
# resolved from this script's location rather than $HERDR_PLUGIN_ROOT (build commands may
# not receive the runtime env). At runtime the pane command reads
# $HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe.
$ErrorActionPreference = 'Stop'

$Name = 'herdr-reviewr'
$Repo = 'dcieslak19973/herdr-reviewr'
$Root = Split-Path -Parent $PSScriptRoot
$BinDir = Join-Path $Root 'bin'

# The release tag matches the manifest version, so a checkout always pulls its own release.
$versionLine = Get-Content (Join-Path $Root 'herdr-plugin.toml') |
  Where-Object { $_ -match '^version' } | Select-Object -First 1
if ($versionLine -notmatch '"([^"]+)"') { throw "${Name}: cannot read version from herdr-plugin.toml" }
$Tag = "v$($Matches[1])"

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
  throw "${Name}: no prebuilt binary for windows-$($env:PROCESSOR_ARCHITECTURE) — build from source with 'cargo install --path .'"
}
$Target = 'x86_64-pc-windows-msvc'

$Archive = "$Name-$Target.zip"
# taiki-e's checksum sidecar drops the archive extension: <name>-<target>.sha256.
$Checksum = "$Name-$Target.sha256"
$Base = if ($env:HERDR_REVIEWR_BASE_URL) { $env:HERDR_REVIEWR_BASE_URL }
        else { "https://github.com/$Repo/releases/download/$Tag" }

# Release-asset downloads are eventually-consistent: GitHub's CDN can 404 for a few minutes
# after a release publishes. Retry (incl. on 404) so an install right after a release does
# not fail spuriously. A local path (the test hook) is copied instead of downloaded.
function Get-Asset([string]$Source, [string]$Dest) {
  if (Test-Path $Source) { Copy-Item $Source $Dest; return }
  for ($attempt = 1; $attempt -le 6; $attempt++) {
    try { Invoke-WebRequest -UseBasicParsing -Uri $Source -OutFile $Dest; return }
    catch { if ($attempt -eq 6) { throw }; Start-Sleep -Seconds 3 }
  }
}

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) "herdr-reviewr-install-$PID"
New-Item -ItemType Directory -Force $Tmp | Out-Null
try {
  Write-Output "${Name}: downloading $Archive ($Tag)"
  Get-Asset "$Base/$Archive" (Join-Path $Tmp $Archive)
  Get-Asset "$Base/$Checksum" (Join-Path $Tmp $Checksum)

  Write-Output "${Name}: verifying checksum"
  $expected = ((Get-Content (Join-Path $Tmp $Checksum) -TotalCount 1) -split '\s+')[0].ToLowerInvariant()
  $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp $Archive)).Hash.ToLowerInvariant()
  if ($expected -ne $actual) { throw "${Name}: checksum mismatch (expected $expected, got $actual)" }

  Expand-Archive -Path (Join-Path $Tmp $Archive) -DestinationPath $Tmp -Force
  New-Item -ItemType Directory -Force $BinDir | Out-Null
  Copy-Item (Join-Path $Tmp "$Name.exe") (Join-Path $BinDir "$Name.exe") -Force
  Write-Output "${Name}: installed $(Join-Path $BinDir "$Name.exe")"

  # Post-install next steps: printed on success only. PATH is not modified on Windows —
  # the pane and actions invoke the binary by absolute path, and users can too.
  Write-Output "${Name}: next steps"
  Write-Output "  1) install the agent skill:  & `"$(Join-Path $BinDir "$Name.exe")`" skill-install"
  Write-Output "     (or: herdr plugin action invoke skill-install --plugin dcieslak19973.reviewr)"
} finally {
  Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
```

- [ ] **Step 2: Test the happy path locally with the base-URL hook**

```powershell
cargo build --release
$stage = Join-Path $env:TEMP "reviewr-install-test"
New-Item -ItemType Directory -Force $stage | Out-Null
Compress-Archive -Path target\release\herdr-reviewr.exe -DestinationPath (Join-Path $stage "herdr-reviewr-x86_64-pc-windows-msvc.zip") -Force
$hash = (Get-FileHash -Algorithm SHA256 (Join-Path $stage "herdr-reviewr-x86_64-pc-windows-msvc.zip")).Hash.ToLowerInvariant()
Set-Content -Encoding ascii (Join-Path $stage "herdr-reviewr-x86_64-pc-windows-msvc.sha256") "$hash *herdr-reviewr-x86_64-pc-windows-msvc.zip"
$env:HERDR_REVIEWR_BASE_URL = $stage
powershell -NoProfile -ExecutionPolicy Bypass -File herdr\install.ps1
```

Expected: prints `downloading`, `verifying checksum`, `installed …\bin\herdr-reviewr.exe`, `next steps`; `bin\herdr-reviewr.exe` exists and `& .\bin\herdr-reviewr.exe skill-path` runs. (`bin/` is a build product — confirm it is gitignored; install.sh has always created it on unix.)

- [ ] **Step 3: Test the checksum-mismatch path**

```powershell
Set-Content -Encoding ascii (Join-Path $stage "herdr-reviewr-x86_64-pc-windows-msvc.sha256") "0000000000000000000000000000000000000000000000000000000000000000 *x"
powershell -NoProfile -ExecutionPolicy Bypass -File herdr\install.ps1
```

Expected: non-zero exit, error contains `checksum mismatch`. Then clean up: `Remove-Item Env:HERDR_REVIEWR_BASE_URL; Remove-Item -Recurse -Force $stage, bin`.

- [ ] **Step 4: Add the Windows target to `release.yml`**

In the matrix `include` list (line 34-38), add:

```yaml
          - { target: x86_64-pc-windows-msvc, os: windows-latest }
```

(taiki-e/upload-rust-binary-action defaults to `.zip` for Windows targets and keeps the `$bin-$target` archive name and sha256 sidecar — exactly what install.ps1 expects.)

- [ ] **Step 5: Commit**

```
git add herdr/install.ps1 .github/workflows/release.yml
git commit -m "feat: Windows install script and x86_64-pc-windows-msvc release target"
```

---

### Task 6: Windows CI job

**Files:**
- Modify: `.github/workflows/ci.yml` (append job)

**Interfaces:**
- Consumes: nothing; runs the same `cargo test` the unix job does.
- Produces: green `test (windows)` check on PRs.

- [ ] **Step 1: Append the job to `ci.yml`**

After the `static-linux` job:

```yaml
  windows:
    name: test (windows)
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5

      - name: Install toolchain (pinned via rust-toolchain.toml)
        uses: dtolnay/rust-toolchain@1.90.0

      - uses: Swatinem/rust-cache@v2

      - name: Test
        run: cargo test --all-features

      - name: Build (release)
        run: cargo build --release
```

(fmt/clippy stay unix-only — they are platform-independent; the Windows job exists to catch `cfg(windows)` code paths and path-handling regressions. The workflow-level `RUSTFLAGS: "-D warnings"` applies to this job automatically.)

- [ ] **Step 2: Validate the workflow syntax**

Run: `Get-Content .github\workflows\ci.yml | Select-Object -First 1` (sanity) and rely on the PR run for real validation — or, if `gh` is authenticated: `gh workflow list` after push.
Expected: the job appears and passes on the eventual PR.

- [ ] **Step 3: Commit**

```
git add .github/workflows/ci.yml
git commit -m "ci: run tests on windows-latest"
```

---

### Task 7: Documentation (README, api notes, CHANGELOG)

**Files:**
- Modify: `README.md` (platform section ~line 509-512; install/troubleshooting area; clipboard mentions)
- Modify: `docs/herdr-api-notes.md` (manifest section)
- Modify: `CHANGELOG.md` (new entry at top)

**Interfaces:** none — prose only. Anchor by quoted text, not line numbers (they shift).

- [ ] **Step 1: README platform section**

Replace the bullet `- **macOS and Linux only** — no Windows.` with:

```markdown
- **macOS, Linux, and Windows.** Windows needs `git` on `PATH` (ships with
  [Git for Windows](https://gitforwindows.org/)) and a herdr new enough to run Windows
  plugin commands (0.7.1+); an older herdr silently skips the build step, leaving a
  binary-less install — `herdr plugin list` shows the plugin but the pane won't open.
```

Replace, in the clipboard bullet ending `OSC 52 and Windows are on the roadmap.`, that sentence with: `Windows uses the built-in \`clip\`. OSC 52 is on the roadmap.`

- [ ] **Step 2: README troubleshooting entry**

Find the install documentation (the section containing `herdr plugin install dcieslak19973/herdr-reviewr`) and add beneath it:

```markdown
> **`Error { kind: NotFound, message: "program not found" }`** during
> `herdr plugin install` means herdr could not spawn `git` — it is not installed or not on
> `PATH` in this shell. Install [Git for Windows](https://gitforwindows.org/) (or
> `git` via your package manager), open a fresh shell, and re-run the install.
```

- [ ] **Step 3: `docs/herdr-api-notes.md` manifest section**

In the "Plugin manifest" section, after the top-level fields line, add:

```markdown
`platforms` accepts `"macos"`, `"linux"`, `"windows"`. Item-level `platforms` on any
`[[build]]`/`[[panes]]`/`[[actions]]`/`[[events]]` entry override the top-level list —
declare one entry per platform (same id) to vary the command. Commands are direct argv, no
shell; Windows resolves `PATHEXT` shims (`npm.cmd` etc.) for build/action/event commands.
Observed (0.7.1-preview): with no `"windows"` in `platforms`, `plugin install` on Windows
still installs but reports `build (skipped on windows)`; a missing `git` on `PATH` fails
the clone with the raw spawn error `Error { kind: NotFound, message: "program not found" }`.
```

- [ ] **Step 4: CHANGELOG entry**

Add at the top, following the file's existing format (read it first and match heading style):

```markdown
## Unreleased

- Windows support: prebuilt `x86_64-pc-windows-msvc` release binary, `herdr/install.ps1`
  build step, per-platform manifest commands, and clipboard via `clip`.
- The sidebar actions and `worktree.created` hook now run `herdr-reviewr sidebar <mode>`
  (a Rust port of `herdr/sidebar.sh`) — `jq` and `bash` are no longer runtime
  dependencies on any platform.
- `min_herdr_version` is now 0.7.1 (item-level manifest `platforms` support).
```

- [ ] **Step 5: Commit**

```
git add README.md docs/herdr-api-notes.md CHANGELOG.md
git commit -m "docs: Windows platform support, git-on-PATH troubleshooting"
```

---

### Task 8: End-to-end verification on this Windows machine

**Files:** none (verification only).

**Interfaces:**
- Consumes: everything above; the local herdr (`0.7.1-preview`) at `C:\Users\Dan Cieslak\AppData\Local\Programs\Herdr\bin\herdr.exe`.

- [ ] **Step 1: Stage a linked plugin with a real binary**

```powershell
cargo build --release
New-Item -ItemType Directory -Force bin | Out-Null
Copy-Item target\release\herdr-reviewr.exe bin\herdr-reviewr.exe -Force
herdr plugin link .
herdr plugin list
```

Expected: plugin listed, enabled, 4 actions / 1 pane / 1 event.

- [ ] **Step 2: Exercise the sidebar action headlessly**

```powershell
herdr plugin action invoke toggle --plugin dcieslak19973.reviewr
herdr plugin log list --plugin dcieslak19973.reviewr --limit 10
```

Expected: with no herdr workspace focused this may refuse with `no workspace context (invoke from inside herdr)` — that exact message IS the pass criterion for the headless case (proves powershell → exe → sidebar dispatch works). With the herdr app open on a repo workspace: `opened <pane> (split) in <ws>` and a visible reviewr pane; a second invoke prints `closed <pane> in <ws>`.

- [ ] **Step 3: In-pane smoke test (needs the herdr app open — coordinate with the user if this session is headless)**

Inside the reviewr pane on any repo with changes: navigate the diff, press `y` on a comment to copy — then `Get-Clipboard` in a shell shows the comment (Task 4's `clip` path).

- [ ] **Step 4: Clean up and report**

```powershell
herdr plugin unlink dcieslak19973.reviewr
Remove-Item -Recurse -Force bin
```

Report results, including anything that only a post-release `herdr plugin install` can prove (the real download path — verify after the next release per `docs/RELEASING.md`, then have the colleague re-run their install with git installed).
