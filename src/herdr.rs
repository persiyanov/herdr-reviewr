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

/// The (tab, workspace, pane) id trio identifying this sidebar in the herdr environment.
fn agent_env() -> (Option<String>, Option<String>, Option<String>) {
    (
        env::var("HERDR_TAB_ID").ok(),
        env::var("HERDR_WORKSPACE_ID").ok(),
        env::var("HERDR_PANE_ID").ok(),
    )
}

/// The agents herdr currently lists. The one place the `agent list` call and its envelope
/// parsing live, shared by pane and status resolution.
fn agent_list() -> Result<Vec<Value>> {
    parse_agents(&herdr(&["agent", "list"])?)
}

/// The agent pane to send to: the sole agent in this tab, else the sole workspace agent.
///
/// A refusal says why and names the clipboard fallback (`specs/herdr-host.md`, S4/S5) —
/// the status line renders it as `agent failed: <this message>`.
pub fn resolve_agent_pane() -> Result<String> {
    let (tab, ws, me) = agent_env();
    match pick_agent(&agent_list()?, tab.as_deref(), ws.as_deref(), me.as_deref()) {
        Ok(agent) => pane_id(agent).context("agent entry has no pane_id"),
        Err(Refusal::NoAgent) => bail!("no agent here — copy to the clipboard instead"),
        Err(Refusal::Several) => bail!("several agents here — copy to the clipboard instead"),
    }
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
    let (tab, ws, me) = agent_env();
    Ok(pick_agent(&agent_list()?, tab.as_deref(), ws.as_deref(), me.as_deref())
        .ok()
        .and_then(|a| a.get("agent_status").and_then(Value::as_str).map(String::from)))
}

/// Why no agent resolved: none to send to, or too many to pick from.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    NoAgent,
    Several,
}

/// The sole agent in this tab, else the sole workspace agent (`specs/herdr-host.md`, S1–S4).
///
/// The workspace candidates are a superset of the tab candidates whenever both env ids are
/// present, so the refusal reason reads off the widest scope: no candidates anywhere is
/// `NoAgent`, anything else is `Several`.
fn pick_agent<'a>(
    agents: &'a [Value],
    tab: Option<&str>,
    ws: Option<&str>,
    me: Option<&str>,
) -> Result<&'a Value, Refusal> {
    let in_tab = candidates(agents, "tab_id", tab, me);
    if let &[agent] = in_tab.as_slice() {
        return Ok(agent);
    }
    match candidates(agents, "workspace_id", ws, me).as_slice() {
        &[agent] => Ok(agent),
        [] if in_tab.is_empty() => Err(Refusal::NoAgent),
        _ => Err(Refusal::Several),
    }
}

/// The real agents whose `key` equals `want`, ignoring our own pane `me`. Only entries
/// carrying an `agent` field count — `herdr agent list` returns every pane, and a non-agent
/// pane (a plugin sidebar, a plain shell) has `agent_status: unknown` and no `agent` field.
fn candidates<'a>(
    agents: &'a [Value],
    key: &str,
    want: Option<&str>,
    me: Option<&str>,
) -> Vec<&'a Value> {
    let Some(want) = want else { return Vec::new() };
    agents
        .iter()
        .filter(|a| a.get("agent").and_then(Value::as_str).is_some())
        .filter(|a| a.get(key).and_then(Value::as_str) == Some(want))
        .filter(|a| pane_id(a).as_deref() != me)
        .collect()
}

/// The `pane_id` of an agent entry.
fn pane_id(agent: &Value) -> Option<String> {
    agent.get("pane_id").and_then(Value::as_str).map(String::from)
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
    use super::{Refusal, pane_id, parse_agents, pick_agent};
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

    /// One non-agent pane as herdr 0.7.1 lists it live: `agent_status: unknown`, no `agent`
    /// field — a plugin sidebar or a plain shell.
    fn non_agent_pane(pane: &str, tab: &str, ws: &str) -> Value {
        json!({
            "agent_status": "unknown",
            "cwd": "/repo",
            "pane_id": pane,
            "tab_id": tab,
            "workspace_id": ws,
            "focused": false
        })
    }

    /// [`pick_agent`] reduced to the picked `pane_id`, for terse assertions.
    fn pick(
        agents: &[Value],
        tab: Option<&str>,
        ws: Option<&str>,
        me: Option<&str>,
    ) -> Result<String, Refusal> {
        pick_agent(agents, tab, ws, me).map(|a| pane_id(a).expect("fixture has pane_id"))
    }

    #[test]
    fn pick_prefers_the_tab_agent_over_the_workspace() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Both share workspace w8; our tab is w8:t2, so its pane wins (S3).
        assert_eq!(pick(&agents, Some("w8:t2"), Some("w8"), None), Ok("w8:p2".to_string()));
    }

    #[test]
    fn pick_falls_back_to_the_sole_workspace_agent() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8")];
        // No agent shares our tab, but exactly one is in the workspace.
        assert_eq!(pick(&agents, Some("w8:tX"), Some("w8"), None), Ok("w8:p1".to_string()));
    }

    #[test]
    fn the_reviewr_pane_excludes_itself_so_the_real_agent_resolves() {
        // Even if herdr listed our own sidebar pane (w8:p5) as an agent alongside the real
        // one (w8:p1), excluding our pane leaves the real agent unambiguous (S2).
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p5", "w8:t1", "w8")];
        assert_eq!(
            pick(&agents, Some("w8:t1"), Some("w8"), Some("w8:p5")),
            Ok("w8:p1".to_string())
        );
    }

    #[test]
    fn non_agent_panes_do_not_make_the_tab_ambiguous() {
        // A tab holding one real agent plus a non-agent pane (another plugin's sidebar, a
        // plain shell) resolves to the agent, not an ambiguity refusal (S1, #6).
        let agents = vec![agent("w3:p1", "w3:t1", "w3"), non_agent_pane("w3:p4", "w3:t1", "w3")];
        assert_eq!(
            pick(&agents, Some("w3:t1"), Some("w3"), Some("w3:p5")),
            Ok("w3:p1".to_string())
        );
    }

    #[test]
    fn only_non_agent_panes_refuse_as_no_agent() {
        // A tab and workspace holding nothing but non-agent panes has no one to send to (S1, S4).
        let agents =
            vec![non_agent_pane("w3:p2", "w3:t1", "w3"), non_agent_pane("w3:p4", "w3:t1", "w3")];
        assert_eq!(pick(&agents, Some("w3:t1"), Some("w3"), None), Err(Refusal::NoAgent));
    }

    #[test]
    fn no_matching_agent_refuses_as_no_agent() {
        let agents = vec![agent("w9:p1", "w9:t1", "w9")];
        // An agent exists, but in another workspace entirely (S4, S5).
        assert_eq!(pick(&agents, Some("w8:t1"), Some("w8"), None), Err(Refusal::NoAgent));
    }

    #[test]
    fn two_workspace_agents_refuse_as_several() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Neither shares our tab and the workspace has two — refuse to guess (S4, S5).
        assert_eq!(pick(&agents, Some("w8:tZ"), Some("w8"), None), Err(Refusal::Several));
    }

    #[test]
    fn two_tab_agents_refuse_as_several_even_without_a_workspace_id() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t1", "w8")];
        // Two agents share our tab and no workspace id is available to widen the scope —
        // still a several-agents refusal, not a missing-agent one (S4, S5).
        assert_eq!(pick(&agents, Some("w8:t1"), None, None), Err(Refusal::Several));
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
}
