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
    let me = env::var("HERDR_PANE_ID").ok();
    let agents = parse_agents(&herdr(&["agent", "list"])?)?;
    pick_agent_pane(&agents, tab.as_deref(), ws.as_deref(), me.as_deref())
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

/// The resolved agent's `agent_status` (`idle`/`working`/`blocked`/`done`/`unknown`), for
/// turn tracking (`specs/herdr-host.md`). `Ok(None)` when no agent resolves, so the caller
/// treats an absent or ambiguous agent the same as a missing herdr — turn tracking pauses.
pub fn resolved_agent_status() -> Result<Option<String>> {
    let tab = env::var("HERDR_TAB_ID").ok();
    let ws = env::var("HERDR_WORKSPACE_ID").ok();
    let me = env::var("HERDR_PANE_ID").ok();
    let agents = parse_agents(&herdr(&["agent", "list"])?)?;
    Ok(pick_agent(&agents, tab.as_deref(), ws.as_deref(), me.as_deref())
        .and_then(|a| a.get("agent_status").and_then(Value::as_str).map(String::from)))
}

/// The pane to send to: the unique agent in this tab, else the sole workspace agent.
fn pick_agent_pane(
    agents: &[Value],
    tab: Option<&str>,
    ws: Option<&str>,
    me: Option<&str>,
) -> Option<String> {
    pick_agent(agents, tab, ws, me).and_then(pane_id)
}

/// The unique agent in this tab, else the sole workspace agent. `me` is our own pane,
/// excluded throughout — herdr lists the reviewr sidebar as an agent, so without this the
/// real agent looks ambiguous in our own tab and workspace.
fn pick_agent<'a>(
    agents: &'a [Value],
    tab: Option<&str>,
    ws: Option<&str>,
    me: Option<&str>,
) -> Option<&'a Value> {
    sole_agent(agents, "tab_id", tab, me).or_else(|| sole_agent(agents, "workspace_id", ws, me))
}

/// The unique agent whose `key` equals `want`, ignoring our own pane `me`; `None` if zero
/// or many remain.
fn sole_agent<'a>(
    agents: &'a [Value],
    key: &str,
    want: Option<&str>,
    me: Option<&str>,
) -> Option<&'a Value> {
    let want = want?;
    let mut matches = agents
        .iter()
        .filter(|a| a.get(key).and_then(Value::as_str) == Some(want))
        .filter(|a| pane_id(a).as_deref() != me);
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first)
}

/// The `pane_id` of an agent entry.
fn pane_id(agent: &Value) -> Option<String> {
    agent.get("pane_id").and_then(Value::as_str).map(String::from)
}

/// The `pane_id` of the unique agent for `key`/`want`, ignoring `me`; a thin pane-returning
/// wrapper over [`sole_agent`].
#[cfg(test)]
fn sole_pane(agents: &[Value], key: &str, want: Option<&str>, me: Option<&str>) -> Option<String> {
    sole_agent(agents, key, want, me).and_then(pane_id)
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
        assert_eq!(sole_pane(&agents, "tab_id", Some("w8:t1"), None), Some("w8:p1".to_string()));
        assert_eq!(sole_pane(&agents, "tab_id", Some("nope"), None), None);
    }

    #[test]
    fn sole_pane_is_none_when_ambiguous() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        assert_eq!(sole_pane(&agents, "workspace_id", Some("w8"), None), None);
    }

    #[test]
    fn the_reviewr_pane_excludes_itself_so_the_real_agent_resolves() {
        // herdr lists our own sidebar pane (w8:p5) as an agent alongside the real one (w8:p1),
        // both in our tab. Excluding our pane leaves the real agent unambiguous.
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p5", "w8:t1", "w8")];
        assert_eq!(
            pick_agent_pane(&agents, Some("w8:t1"), Some("w8"), Some("w8:p5")),
            Some("w8:p1".to_string())
        );
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
        assert_eq!(
            pick_agent_pane(&agents, Some("w8:t2"), Some("w8"), None),
            Some("w8:p2".to_string())
        );
    }

    #[test]
    fn pick_falls_back_to_the_sole_workspace_agent() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8")];
        // No agent shares our tab, but exactly one is in the workspace.
        assert_eq!(
            pick_agent_pane(&agents, Some("w8:tX"), Some("w8"), None),
            Some("w8:p1".to_string())
        );
    }

    #[test]
    fn pick_is_none_when_the_workspace_is_ambiguous() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Neither shares our tab and the workspace has two — refuse to guess.
        assert_eq!(pick_agent_pane(&agents, Some("w8:tZ"), Some("w8"), None), None);
    }
}
