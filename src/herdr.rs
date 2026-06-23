//! herdr host integration: resolve the agent pane and send to it.
//!
//! See `specs/herdr-host.md`. Uses the herdr CLI via `$HERDR_BIN_PATH`. Only the
//! agent-send export depends on this module; browsing and clipboard do not.

use std::env;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

fn herdr_bin() -> String {
    env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

fn herdr(args: &[&str]) -> Result<String> {
    let out = Command::new(herdr_bin())
        .args(args)
        .output()
        .with_context(|| format!("running herdr {args:?}"))?;
    if !out.status.success() {
        bail!("herdr {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The agent pane to send to: the agent in this tab, else the sole workspace agent.
///
/// Returns an error when no agent resolves, or when the choice is ambiguous
/// (two agents and none shares the tab).
pub fn resolve_agent_pane() -> Result<String> {
    let tab = env::var("HERDR_TAB_ID").ok();
    let ws = env::var("HERDR_WORKSPACE_ID").ok();
    let agents = parse_agents(&herdr(&["agent", "list"])?)?;
    pick_agent_pane(&agents, tab.as_deref(), ws.as_deref())
        .context("no unambiguous agent in this tab or workspace")
}

/// The agents array from `herdr agent list`. The CLI's exact envelope is not pinned
/// by the spike notes, so accept a bare array, `result.agents`, or `agents`.
fn parse_agents(json: &str) -> Result<Vec<Value>> {
    let value: Value = serde_json::from_str(json).context("parsing agent list")?;
    if let Some(array) = value.as_array() {
        return Ok(array.clone());
    }
    value
        .get("result")
        .and_then(|r| r.get("agents"))
        .or_else(|| value.get("agents"))
        .and_then(Value::as_array)
        .cloned()
        .context("agent list has no agents array")
}

/// The pane to send to: the unique agent in this tab, else the sole workspace agent.
fn pick_agent_pane(agents: &[Value], tab: Option<&str>, ws: Option<&str>) -> Option<String> {
    sole_pane(agents, "tab_id", tab).or_else(|| sole_pane(agents, "workspace_id", ws))
}

/// The `pane_id` of the unique agent whose `key` equals `want`; `None` if zero or many.
fn sole_pane(agents: &[Value], key: &str, want: Option<&str>) -> Option<String> {
    let want = want?;
    let mut matches = agents.iter().filter(|a| a.get(key).and_then(Value::as_str) == Some(want));
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    first.get("pane_id").and_then(Value::as_str).map(String::from)
}

/// Write literal text into the agent pane's input, without submitting.
pub fn send_text(pane: &str, text: &str) -> Result<()> {
    herdr(&["agent", "send", pane, text])?;
    Ok(())
}

/// Focus the agent pane so the reviewer can add context and submit.
pub fn focus(pane: &str) -> Result<()> {
    herdr(&["agent", "focus", pane])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_agents, pick_agent_pane, sole_pane};
    use serde_json::{Value, json};

    /// One agent entry shaped like the real `herdr agent list` output (api notes).
    fn agent(pane: &str, tab: &str, ws: &str) -> Value {
        json!({
            "agent": "claude",
            "agent_status": "working",
            "cwd": "/repo",
            "pane_id": pane,
            "tab_id": tab,
            "workspace_id": ws,
            "focused": true
        })
    }

    #[test]
    fn sole_pane_picks_unique_match() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w9:p1", "w9:t1", "w9")];
        assert_eq!(sole_pane(&agents, "tab_id", Some("w8:t1")), Some("w8:p1".to_string()));
        assert_eq!(sole_pane(&agents, "tab_id", Some("nope")), None);
    }

    #[test]
    fn sole_pane_is_none_when_ambiguous() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        assert_eq!(sole_pane(&agents, "workspace_id", Some("w8")), None);
    }

    #[test]
    fn parse_agents_accepts_bare_array_and_result_envelope() {
        let a = agent("w8:p1", "w8:t1", "w8");
        let bare = json!([a]).to_string();
        assert_eq!(parse_agents(&bare).unwrap().len(), 1);
        let wrapped =
            json!({ "result": { "agents": [agent("w8:p1", "w8:t1", "w8")] } }).to_string();
        assert_eq!(parse_agents(&wrapped).unwrap().len(), 1);
    }

    #[test]
    fn pick_prefers_the_tab_agent_over_the_workspace() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Both share workspace w8; our tab is w8:t2, so its pane wins.
        assert_eq!(pick_agent_pane(&agents, Some("w8:t2"), Some("w8")), Some("w8:p2".to_string()));
    }

    #[test]
    fn pick_falls_back_to_the_sole_workspace_agent() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8")];
        // No agent shares our tab, but exactly one is in the workspace.
        assert_eq!(pick_agent_pane(&agents, Some("w8:tX"), Some("w8")), Some("w8:p1".to_string()));
    }

    #[test]
    fn pick_is_none_when_the_workspace_is_ambiguous() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Neither shares our tab and the workspace has two — refuse to guess.
        assert_eq!(pick_agent_pane(&agents, Some("w8:tZ"), Some("w8")), None);
    }
}
