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
}
