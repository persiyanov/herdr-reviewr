//! herdr host integration: resolve the agent pane and send to it.
//!
//! See `specs/herdr-host.md`. Uses the herdr CLI via `$HERDR_BIN_PATH`. Only the
//! agent-send export depends on this module; browsing and clipboard do not.

use std::env;
use std::process::Command;

use crate::turn::Status;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AgentListResponse {
    result: AgentList,
}

#[derive(Debug, Deserialize)]
struct AgentList {
    agents: Vec<AgentPane>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct AgentPane {
    agent: Option<String>,
    agent_status: Status,
    pane_id: String,
    tab_id: String,
    workspace_id: String,
}

#[derive(Debug, Deserialize)]
struct TabListResponse {
    result: TabList,
}

#[derive(Debug, Deserialize)]
struct TabList {
    tabs: Vec<TabRef>,
}

/// One `herdr tab list` entry. The array order is the live left-to-right tab order and
/// tracks drag-reorders; `number` is a creation id, not a position, so only order counts.
#[derive(Debug, Deserialize)]
struct TabRef {
    tab_id: String,
    workspace_id: String,
}

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
fn agent_list() -> Result<Vec<AgentPane>> {
    parse_agents(&herdr(&["agent", "list"])?)
}

/// The workspace's tab ids in visible left-to-right order (`herdr tab list` array order).
/// Best-effort: any failure yields an empty order, which disables the nearest-left rule
/// without affecting the tab and sole-workspace resolutions.
fn tab_order(ws: Option<&str>) -> Vec<String> {
    let Some(ws) = ws else { return Vec::new() };
    match herdr(&["tab", "list", "--workspace", ws]) {
        Ok(json) => parse_tab_order(&json, ws),
        Err(_) => Vec::new(),
    }
}

/// The `result.tabs` ids from `herdr tab list`, in array order, scoped to `ws`. A malformed
/// envelope yields an empty order. `--workspace` already scopes the list; the `ws` filter is
/// a guard against a herdr that ignores the flag and returns every workspace's tabs.
fn parse_tab_order(json: &str, ws: &str) -> Vec<String> {
    let Ok(response) = serde_json::from_str::<TabListResponse>(json) else {
        return Vec::new();
    };
    response
        .result
        .tabs
        .into_iter()
        .filter(|tab| tab.workspace_id == ws)
        .map(|tab| tab.tab_id)
        .collect()
}

/// The agent pane to send to: the sole tab agent, else the sole workspace agent, else the
/// agent in the nearest tab to the left of the sidebar (`specs/herdr-host.md`, HH-NEAREST-LEFT).
///
/// A refusal says why and names the clipboard fallback (`specs/herdr-host.md`, HH-SOLE-OR-REFUSE/HH-REFUSE-SAYS-CLIPBOARD) —
/// the status line renders it as `agent failed: <this message>`.
pub fn resolve_agent_pane() -> Result<String> {
    let (tab, ws, me) = agent_env();
    let order = || tab_order(ws.as_deref());
    match pick_agent(&agent_list()?, order, tab.as_deref(), ws.as_deref(), me.as_deref()) {
        Ok(agent) => Ok(agent.pane_id.clone()),
        Err(Refusal::NoAgent) => bail!("no agent here — copy to the clipboard instead"),
        Err(Refusal::Several) => bail!("several agents here — copy to the clipboard instead"),
    }
}

/// The documented `result.agents` array from `herdr agent list`.
fn parse_agents(json: &str) -> Result<Vec<AgentPane>> {
    let response: AgentListResponse = serde_json::from_str(json).context("parsing agent list")?;
    Ok(response.result.agents)
}

/// The resolved agent's `agent_status` (`idle`/`working`/`blocked`/`done`/`unknown`), for
/// turn tracking (`specs/herdr-host.md`). `Ok(None)` when no agent resolves, so the caller
/// treats an absent or ambiguous agent the same as a missing herdr — turn tracking pauses.
pub fn resolved_agent_status() -> Result<Option<Status>> {
    let (tab, ws, me) = agent_env();
    let order = || tab_order(ws.as_deref());
    Ok(pick_agent(&agent_list()?, order, tab.as_deref(), ws.as_deref(), me.as_deref())
        .ok()
        .map(|agent| agent.agent_status))
}

/// Why no agent resolved: none to send to, or too many to pick from.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    NoAgent,
    Several,
}

/// The sole tab agent, else the sole workspace agent, else the agent in the nearest tab to
/// the left of the sidebar (`specs/herdr-host.md`, HH-AGENT-PANES through HH-NEAREST-LEFT).
///
/// The workspace candidates are a superset of the tab candidates whenever both env ids are
/// present. The nearest-left rule only fires when several workspace agents remain, so a lone
/// agent is always taken first. With nothing resolvable, no candidates anywhere is `NoAgent`,
/// anything else is `Several`.
///
/// `tab_order` is lazy: only several workspace agents can reach the nearest-left rule, so the
/// common single-agent resolutions never pay for the `herdr tab list` subprocess it wraps.
fn pick_agent<'a>(
    agents: &'a [AgentPane],
    tab_order: impl FnOnce() -> Vec<String>,
    tab: Option<&str>,
    ws: Option<&str>,
    me: Option<&str>,
) -> Result<&'a AgentPane, Refusal> {
    let in_tab = candidates(agents, tab, me, |agent| &agent.tab_id);
    if let &[agent] = in_tab.as_slice() {
        return Ok(agent);
    }
    let in_ws = candidates(agents, ws, me, |agent| &agent.workspace_id);
    if let &[agent] = in_ws.as_slice() {
        return Ok(agent);
    }
    // `&&` short-circuits, so `tab_order()` (a `herdr tab list` subprocess) runs only when
    // several workspace agents actually reach the nearest-left rule.
    if in_ws.len() >= 2
        && let Some(agent) = nearest_left_agent(&in_ws, &tab_order(), tab)
    {
        return Ok(agent);
    }
    if in_tab.is_empty() && in_ws.is_empty() {
        Err(Refusal::NoAgent)
    } else {
        Err(Refusal::Several)
    }
}

/// The workspace agent in the nearest tab to the left of the sidebar's own tab, by visible
/// tab order (`specs/herdr-host.md`, HH-NEAREST-LEFT). `None` when the sidebar's tab is
/// absent from the order, no agent sits left of it, or the nearest left tab holds several
/// agents — a tab reviewr will not guess between.
fn nearest_left_agent<'a>(
    ws_agents: &[&'a AgentPane],
    tab_order: &[String],
    tab: Option<&str>,
) -> Option<&'a AgentPane> {
    let own = tab_order.iter().position(|id| Some(id.as_str()) == tab)?;
    let position_of = |tab_id: &str| tab_order.iter().position(|id| id.as_str() == tab_id);
    let left: Vec<(usize, &'a AgentPane)> = ws_agents
        .iter()
        .filter_map(|agent| position_of(agent.tab_id.as_str()).map(|pos| (pos, *agent)))
        .filter(|(pos, _)| *pos < own)
        .collect();
    let nearest = left.iter().map(|(pos, _)| *pos).max()?;
    let in_nearest: Vec<&'a AgentPane> =
        left.into_iter().filter(|(pos, _)| *pos == nearest).map(|(_, agent)| agent).collect();
    match in_nearest.as_slice() {
        &[agent] => Some(agent),
        _ => None,
    }
}

/// The real agents whose projected ID equals `want`, ignoring our own pane `me`. Only
/// entries carrying an `agent` field count — `herdr agent list` returns every pane, and a
/// non-agent pane (a plugin sidebar, a plain shell) has `agent_status: unknown` and no
/// `agent` field.
fn candidates<'a>(
    agents: &'a [AgentPane],
    want: Option<&str>,
    me: Option<&str>,
    id: impl Fn(&'a AgentPane) -> &'a str,
) -> Vec<&'a AgentPane> {
    let Some(want) = want else { return Vec::new() };
    agents
        .iter()
        .filter(|agent| agent.agent.is_some())
        .filter(|agent| id(agent) == want)
        .filter(|agent| Some(agent.pane_id.as_str()) != me)
        .collect()
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
    use super::{AgentPane, Refusal, Status, parse_agents, parse_tab_order, pick_agent};

    /// One agent entry shaped like the real `herdr agent list` output (api notes).
    fn agent(pane: &str, tab: &str, ws: &str) -> AgentPane {
        AgentPane {
            agent: Some("claude".to_string()),
            agent_status: Status::Working,
            pane_id: pane.to_string(),
            tab_id: tab.to_string(),
            workspace_id: ws.to_string(),
        }
    }

    /// One non-agent pane as herdr 0.7.1 lists it live: `agent_status: unknown`, no `agent`
    /// field — a plugin sidebar or a plain shell.
    fn non_agent_pane(pane: &str, tab: &str, ws: &str) -> AgentPane {
        AgentPane {
            agent: None,
            agent_status: Status::Unknown,
            pane_id: pane.to_string(),
            tab_id: tab.to_string(),
            workspace_id: ws.to_string(),
        }
    }

    /// [`pick_agent`] with no tab order, reduced to the picked `pane_id`. Exercises the tab
    /// and sole-workspace rules; the nearest-left rule stays dormant with an empty order.
    fn pick(
        agents: &[AgentPane],
        tab: Option<&str>,
        ws: Option<&str>,
        me: Option<&str>,
    ) -> Result<String, Refusal> {
        pick_agent(agents, Vec::new, tab, ws, me).map(|agent| agent.pane_id.clone())
    }

    /// [`pick_agent`] with a visible tab order, reduced to the picked `pane_id`. Drives the
    /// nearest-left rule; `order` lists tab ids left to right.
    fn pick_ordered(
        agents: &[AgentPane],
        order: &[&str],
        tab: Option<&str>,
        ws: Option<&str>,
        me: Option<&str>,
    ) -> Result<String, Refusal> {
        let order: Vec<String> = order.iter().map(ToString::to_string).collect();
        pick_agent(agents, move || order, tab, ws, me).map(|agent| agent.pane_id.clone())
    }

    #[test]
    fn pick_prefers_the_tab_agent_over_the_workspace() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Both share workspace w8; our tab is w8:t2, so its pane wins (HH-TAB-WINS).
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
        // one (w8:p1), excluding our pane leaves the real agent unambiguous (HH-NOT-SELF).
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p5", "w8:t1", "w8")];
        assert_eq!(
            pick(&agents, Some("w8:t1"), Some("w8"), Some("w8:p5")),
            Ok("w8:p1".to_string())
        );
    }

    #[test]
    fn non_agent_panes_do_not_make_the_tab_ambiguous() {
        // A tab holding one real agent plus a non-agent pane (another plugin's sidebar, a
        // plain shell) resolves to the agent, not an ambiguity refusal (HH-AGENT-PANES, #6).
        let agents = vec![agent("w3:p1", "w3:t1", "w3"), non_agent_pane("w3:p4", "w3:t1", "w3")];
        assert_eq!(
            pick(&agents, Some("w3:t1"), Some("w3"), Some("w3:p5")),
            Ok("w3:p1".to_string())
        );
    }

    #[test]
    fn only_non_agent_panes_refuse_as_no_agent() {
        // A tab and workspace holding nothing but non-agent panes has no one to send to (HH-AGENT-PANES, HH-SOLE-OR-REFUSE).
        let agents =
            vec![non_agent_pane("w3:p2", "w3:t1", "w3"), non_agent_pane("w3:p4", "w3:t1", "w3")];
        assert_eq!(pick(&agents, Some("w3:t1"), Some("w3"), None), Err(Refusal::NoAgent));
    }

    #[test]
    fn no_matching_agent_refuses_as_no_agent() {
        let agents = vec![agent("w9:p1", "w9:t1", "w9")];
        // An agent exists, but in another workspace entirely (HH-SOLE-OR-REFUSE, HH-REFUSE-SAYS-CLIPBOARD).
        assert_eq!(pick(&agents, Some("w8:t1"), Some("w8"), None), Err(Refusal::NoAgent));
    }

    #[test]
    fn two_workspace_agents_refuse_as_several() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Neither shares our tab and the workspace has two — refuse to guess (HH-SOLE-OR-REFUSE, HH-REFUSE-SAYS-CLIPBOARD).
        assert_eq!(pick(&agents, Some("w8:tZ"), Some("w8"), None), Err(Refusal::Several));
    }

    #[test]
    fn two_tab_agents_refuse_as_several_even_without_a_workspace_id() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t1", "w8")];
        // Two agents share our tab and no workspace id is available to widen the scope —
        // still a several-agents refusal, not a missing-agent one (HH-SOLE-OR-REFUSE, HH-REFUSE-SAYS-CLIPBOARD).
        assert_eq!(pick(&agents, Some("w8:t1"), None, None), Err(Refusal::Several));
    }

    #[test]
    fn parse_agents_accepts_only_the_documented_envelope() {
        let wrapped = r#"{"result":{"agents":[{"agent":"claude","agent_status":"working","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8"}]}}"#;
        assert_eq!(parse_agents(wrapped).unwrap(), [agent("w8:p1", "w8:t1", "w8")]);
        assert!(parse_agents("[]").is_err());
        assert_eq!(serde_json::from_str::<Status>(r#""starting""#).unwrap(), Status::Unknown);
    }

    #[test]
    fn nearest_left_binds_the_first_agent_left_of_the_sidebar() {
        let agents = vec![agent("w:p1", "w:t1", "w"), agent("w:p7", "w:t7", "w")];
        // Sidebar tab w:t5 has no agent; w:t2 to its left has none either, so the nearest
        // agent tab left of it is w:t1 (HH-NEAREST-LEFT). w:t7 is to the right, ignored.
        let order = ["w:t1", "w:t2", "w:t5", "w:t7"];
        assert_eq!(
            pick_ordered(&agents, &order, Some("w:t5"), Some("w"), None),
            Ok("w:p1".to_string())
        );
    }

    #[test]
    fn nearest_left_prefers_the_closest_of_several_left_agents() {
        let agents = vec![
            agent("w:p1", "w:t1", "w"),
            agent("w:p3", "w:t3", "w"),
            agent("w:p7", "w:t7", "w"),
        ];
        // Two agents sit left of the sidebar (w:t1, w:t3); the closer one, w:t3, wins.
        let order = ["w:t1", "w:t3", "w:t5", "w:t7"];
        assert_eq!(
            pick_ordered(&agents, &order, Some("w:t5"), Some("w"), None),
            Ok("w:p3".to_string())
        );
    }

    #[test]
    fn a_sole_tab_agent_wins_over_an_agent_to_the_left() {
        let agents = vec![agent("w:p5", "w:t5", "w"), agent("w:p1", "w:t1", "w")];
        // The sidebar shares w:t5 with one agent; the tab rule takes it before nearest-left.
        let order = ["w:t1", "w:t5"];
        assert_eq!(
            pick_ordered(&agents, &order, Some("w:t5"), Some("w"), Some("w:p9")),
            Ok("w:p5".to_string())
        );
    }

    #[test]
    fn a_sole_workspace_agent_wins_even_when_it_is_to_the_right() {
        let agents = vec![agent("w:p7", "w:t7", "w")];
        // One agent, to the right of the sidebar: the sole-workspace rule still binds it, so
        // nearest-left never runs (it only disambiguates several agents).
        let order = ["w:t5", "w:t7"];
        assert_eq!(
            pick_ordered(&agents, &order, Some("w:t5"), Some("w"), None),
            Ok("w:p7".to_string())
        );
    }

    #[test]
    fn several_agents_with_none_to_the_left_refuse_as_several() {
        let agents = vec![agent("w:p7", "w:t7", "w"), agent("w:p9", "w:t9", "w")];
        // Both agents are to the right of the sidebar and neither shares its tab — nothing to
        // the left, so it refuses rather than guess (→ empty last-turn).
        let order = ["w:t5", "w:t7", "w:t9"];
        assert_eq!(
            pick_ordered(&agents, &order, Some("w:t5"), Some("w"), None),
            Err(Refusal::Several)
        );
    }

    #[test]
    fn several_agents_in_the_nearest_left_tab_refuse() {
        let agents = vec![agent("w:p1", "w:t1", "w"), agent("w:p2", "w:t1", "w")];
        // The nearest tab to the left holds two agents; reviewr will not guess between them.
        let order = ["w:t1", "w:t5"];
        assert_eq!(
            pick_ordered(&agents, &order, Some("w:t5"), Some("w"), None),
            Err(Refusal::Several)
        );
    }

    #[test]
    fn a_sidebar_tab_absent_from_the_order_disables_nearest_left() {
        let agents = vec![agent("w:p1", "w:t1", "w"), agent("w:p3", "w:t3", "w")];
        // The order predates the sidebar's tab (a stale `tab list`), so "left of it" is
        // undefined — nearest-left stays dormant and two agents refuse (HH-NEAREST-LEFT).
        let order = ["w:t1", "w:t3"];
        assert_eq!(
            pick_ordered(&agents, &order, Some("w:t9"), Some("w"), None),
            Err(Refusal::Several)
        );
    }

    #[test]
    fn parse_tab_order_keeps_array_order_and_scopes_to_the_workspace() {
        let json = r#"{"result":{"tabs":[
            {"tab_id":"w:t7","workspace_id":"w","number":7,"focused":true},
            {"tab_id":"w:t1","workspace_id":"w","number":1,"focused":false},
            {"tab_id":"x:t2","workspace_id":"x","number":2,"focused":false}
        ]}}"#;
        // Array order is preserved (not sorted by `number`) and other workspaces drop out.
        assert_eq!(parse_tab_order(json, "w"), ["w:t7".to_string(), "w:t1".to_string()]);
        // A malformed envelope yields an empty order, which disables the nearest-left rule.
        assert!(parse_tab_order("not json", "w").is_empty());
    }
}
