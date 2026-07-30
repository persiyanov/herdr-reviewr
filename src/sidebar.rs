//! `herdr-reviewr sidebar <toggle|open|close|auto-open>` — the sidebar actions and event hook.
//!
//! Rust port of the former `herdr/sidebar.sh` (`specs/herdr-host.md`, contracts A3/A5/A7,
//! P5/P6): one implementation for every platform, no `bash` or `jq` at runtime. The
//! workspace's sidebar is any pane labeled "reviewr" in the live pane list; there is no
//! state file. Actions refuse loudly (exit 1, one stderr line) and report successes on
//! stdout; the `auto-open` event refuses silently (exit 0), except a config error, which
//! reports through stderr for herdr's plugin log.

use std::process::{Command, ExitCode};

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
    non_empty_str(value.get("focused_pane_cwd"))
        .or_else(|| non_empty_str(value.get("workspace_cwd")))
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

/// The manifest pane entrypoint for this build's platform: herdr rejects duplicate pane ids
/// even across disjoint platform filters, so Windows has its own `sidebar-windows` twin.
const fn entrypoint() -> &'static str {
    if cfg!(windows) { "sidebar-windows" } else { "sidebar" }
}

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
        entrypoint().into(),
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
        let primary =
            r#"{"data":{"workspace":{"workspace_id":"w1","worktree":{"checkout_path":"/wt"}}}}"#;
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
        assert_eq!(args, ["--placement", "split", "--target-pane", "p9", "--direction", "right"]);
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

    #[test]
    fn entrypoint_matches_the_platform_pane_id() {
        let expected = if cfg!(windows) { "sidebar-windows" } else { "sidebar" };
        assert_eq!(super::entrypoint(), expected);
    }
}
