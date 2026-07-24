//! herdr host integration: resolve the send target and write to an agent pane.
//!
//! See `specs/herdr-host.md`. Uses the herdr CLI via `$HERDR_BIN_PATH`. Only the
//! agent-send export depends on this module; browsing and clipboard do not.

use std::collections::HashMap;
use std::env;
use std::process::Command;

use crate::turn::Status;
use anyhow::{Context, Result};
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
    /// The agent session name (`herdr agent start <name>` / `agent rename`). Usually absent,
    /// unset for every `claude` session and most `codex` ones, so it is a lead label only when
    /// present (`AgentChoice::lead`). A missing field deserializes to `None`, like `agent`.
    name: Option<String>,
    /// The pane's live terminal title — what the agent is working on. The one field that
    /// reliably differs between same-kind agents in a worktree (`herdr-host.md` HH-MANY-PICK).
    terminal_title_stripped: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TabListResponse {
    result: TabList,
}

#[derive(Debug, Deserialize)]
struct TabList {
    tabs: Vec<TabInfo>,
}

#[derive(Debug, Deserialize)]
struct TabInfo {
    tab_id: String,
    /// The tab's user-assigned label, when it has one.
    label: Option<String>,
}

/// One agent the reviewer can send to, resolved for the picker. `pane_id` is the send
/// target and is never displayed; the rest compose the row (`specs/herdr-host.md`
/// HH-MANY-PICK, `specs/input.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentChoice {
    /// The pane to write to. Internal — never shown.
    pub pane_id: String,
    /// The agent kind, `claude`/`codex` (the `agent` field).
    pub kind: String,
    /// The session name, when set — usually `None`.
    pub name: Option<String>,
    /// The tab's label, when the join found one — shown only when it differs from the kind.
    pub tab_label: Option<String>,
    pub status: Status,
    /// The terminal title (`""` when the pane reports none).
    pub title: String,
}

impl AgentChoice {
    /// The row's lead label: the session name if set, else the agent kind.
    pub fn lead(&self) -> &str {
        self.name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.kind)
    }

    /// The tab label to show as context — only when it adds information beyond the kind
    /// (a tab literally named `codex` beside a `codex` agent says nothing).
    pub fn tab_note(&self) -> Option<&str> {
        self.tab_label.as_deref().filter(|l| !l.is_empty() && *l != self.kind)
    }
}

/// Where a `Send` should go once the candidates are resolved (`specs/herdr-host.md`,
/// HH-ONE-SENDS / HH-MANY-PICK / HH-ZERO-REFUSE): straight to the sole pane, to the picker
/// over several, or nowhere.
#[derive(Debug)]
pub enum SendTarget {
    One(String),
    Many(Vec<AgentChoice>),
    None,
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
        anyhow::bail!("herdr {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
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
/// parsing live, shared by send-target and status resolution.
fn agent_list() -> Result<Vec<AgentPane>> {
    parse_agents(&herdr(&["agent", "list"])?)
}

/// `tab_id → label` for one workspace, best-effort: any tab without a label is skipped.
fn tab_labels(ws: &str) -> Result<HashMap<String, String>> {
    parse_tab_labels(&herdr(&["tab", "list", "--workspace", ws])?)
}

/// The `tab_id → label` map from a `herdr tab list` envelope, tabs without a label skipped.
fn parse_tab_labels(json: &str) -> Result<HashMap<String, String>> {
    let resp: TabListResponse = serde_json::from_str(json).context("parsing tab list")?;
    Ok(resp.result.tabs.into_iter().filter_map(|t| t.label.map(|l| (t.tab_id, l))).collect())
}

/// The send target for the written comments: the sole agent in this tab, else the sole
/// workspace agent (`HH-ONE-SENDS`); several candidates go to the picker (`HH-MANY-PICK`);
/// none refuses (`HH-ZERO-REFUSE`). Only the `Many` branch joins `tab list` for the row
/// labels; `One`/`None` skip it.
pub fn resolve_send_target() -> Result<SendTarget> {
    let (tab, ws, me) = agent_env();
    let agents = agent_list()?;
    Ok(match pick(&agents, tab.as_deref(), ws.as_deref(), me.as_deref()) {
        Pick::One(agent) => SendTarget::One(agent.pane_id.clone()),
        Pick::None => SendTarget::None,
        Pick::Many(panes) => {
            // Best-effort labels: without a workspace id (or on a failed call) rows fall back
            // to the kind. All candidates share the reviewer's workspace, so one call covers them.
            let labels = ws.as_deref().and_then(|w| tab_labels(w).ok()).unwrap_or_default();
            SendTarget::Many(panes.iter().map(|a| a.to_choice(&labels)).collect())
        }
    })
}

/// The documented `result.agents` array from `herdr agent list`.
fn parse_agents(json: &str) -> Result<Vec<AgentPane>> {
    let response: AgentListResponse = serde_json::from_str(json).context("parsing agent list")?;
    Ok(response.result.agents)
}

/// The resolved agent's `agent_status` (`idle`/`working`/`blocked`/`done`/`unknown`), for
/// turn tracking (`specs/herdr-host.md`). `Ok(None)` when no *single* agent resolves, so an
/// absent or ambiguous agent is treated like a missing herdr — turn tracking pauses. The
/// picker never changes this: an ambiguous send is still `None` here.
pub fn resolved_agent_status() -> Result<Option<Status>> {
    let (tab, ws, me) = agent_env();
    let agents = agent_list()?;
    Ok(match pick(&agents, tab.as_deref(), ws.as_deref(), me.as_deref()) {
        Pick::One(agent) => Some(agent.agent_status),
        Pick::Many(_) | Pick::None => None,
    })
}

impl AgentPane {
    /// Build the picker choice for this pane, looking its tab label up in the join map.
    /// `agent` is always `Some` here — non-agent panes never become candidates.
    fn to_choice(&self, labels: &HashMap<String, String>) -> AgentChoice {
        AgentChoice {
            pane_id: self.pane_id.clone(),
            kind: self.agent.clone().unwrap_or_default(),
            name: self.name.clone().filter(|s| !s.is_empty()),
            tab_label: labels.get(&self.tab_id).cloned(),
            status: self.agent_status,
            title: self.terminal_title_stripped.clone().unwrap_or_default(),
        }
    }
}

/// The resolved send target: one agent, several to pick from, or none.
enum Pick<'a> {
    One(&'a AgentPane),
    Many(Vec<&'a AgentPane>),
    None,
}

/// Classify the candidates (`specs/herdr-host.md`, HH-AGENT-PANES / HH-NOT-SELF / HH-TAB-WINS
/// / HH-ONE-SENDS / HH-MANY-PICK / HH-ZERO-REFUSE):
///
/// - a sole tab agent wins outright (`HH-TAB-WINS`);
/// - otherwise the **widest non-empty** candidate set decides — the workspace candidates when
///   present, else the tab candidates: one → `One`, several → `Many(those)`;
/// - `None` only when both sets are empty. So two agents sharing our tab with no workspace id
///   are `Many`, never `None` — the picker still opens.
fn pick<'a>(
    agents: &'a [AgentPane],
    tab: Option<&str>,
    ws: Option<&str>,
    me: Option<&str>,
) -> Pick<'a> {
    let in_tab = candidates(agents, tab, me, |agent| &agent.tab_id);
    if let &[agent] = in_tab.as_slice() {
        return Pick::One(agent);
    }
    let in_ws = candidates(agents, ws, me, |agent| &agent.workspace_id);
    match in_ws.as_slice() {
        &[agent] => Pick::One(agent),
        [] if in_tab.is_empty() => Pick::None,
        // Workspace scope is empty (no workspace id) but the tab holds several — pick among them.
        [] => Pick::Many(in_tab),
        // Workspace scope is the widest ambiguous set (it is a superset of the tab candidates).
        _ => Pick::Many(in_ws),
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
///
/// Uses `pane send-text`, not the agent-level send: herdr 0.7.5 replaced `agent send` with
/// the logical-key `agent send-keys`, while `pane send-text` has carried the literal-text,
/// no-Enter semantics unchanged since 0.7.0 (`docs/herdr-api-notes.md`).
pub fn send_text(pane: &str, text: &str) -> Result<()> {
    herdr(&["pane", "send-text", pane, text])?;
    Ok(())
}

/// Focus the agent pane so the reviewer can add context and submit.
pub fn focus(pane: &str) -> Result<()> {
    herdr(&["agent", "focus", pane])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AgentChoice, AgentPane, Pick, Status, parse_agents, parse_tab_labels, pick};

    /// One agent entry shaped like the real `herdr agent list` output (api notes).
    fn agent(pane: &str, tab: &str, ws: &str) -> AgentPane {
        AgentPane {
            agent: Some("claude".to_string()),
            agent_status: Status::Working,
            pane_id: pane.to_string(),
            tab_id: tab.to_string(),
            workspace_id: ws.to_string(),
            name: None,
            terminal_title_stripped: None,
        }
    }

    /// One non-agent pane as herdr lists it live: `agent_status: unknown`, no `agent`
    /// field — a plugin sidebar or a plain shell.
    fn non_agent_pane(pane: &str, tab: &str, ws: &str) -> AgentPane {
        AgentPane { agent: None, agent_status: Status::Unknown, ..agent(pane, tab, ws) }
    }

    /// A choice for the row-compose tests.
    fn choice(kind: &str, name: Option<&str>, tab_label: Option<&str>, title: &str) -> AgentChoice {
        AgentChoice {
            pane_id: "w:p1".to_string(),
            kind: kind.to_string(),
            name: name.map(str::to_string),
            tab_label: tab_label.map(str::to_string),
            status: Status::Idle,
            title: title.to_string(),
        }
    }

    /// [`pick`] reduced to pane ids, for terse assertions.
    #[derive(Debug, PartialEq, Eq)]
    enum Picked {
        One(String),
        Many(Vec<String>),
        None,
    }

    fn picked(
        agents: &[AgentPane],
        tab: Option<&str>,
        ws: Option<&str>,
        me: Option<&str>,
    ) -> Picked {
        match pick(agents, tab, ws, me) {
            Pick::One(a) => Picked::One(a.pane_id.clone()),
            Pick::Many(v) => Picked::Many(v.iter().map(|a| a.pane_id.clone()).collect()),
            Pick::None => Picked::None,
        }
    }

    #[test]
    fn pick_prefers_the_tab_agent_over_the_workspace() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Both share workspace w8; our tab is w8:t2, so its pane wins (HH-TAB-WINS).
        assert_eq!(picked(&agents, Some("w8:t2"), Some("w8"), None), Picked::One("w8:p2".into()));
    }

    #[test]
    fn pick_falls_back_to_the_sole_workspace_agent() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8")];
        // No agent shares our tab, but exactly one is in the workspace (HH-ONE-SENDS).
        assert_eq!(picked(&agents, Some("w8:tX"), Some("w8"), None), Picked::One("w8:p1".into()));
    }

    #[test]
    fn the_reviewr_pane_excludes_itself_so_the_real_agent_resolves() {
        // Even if herdr listed our own sidebar pane (w8:p5) as an agent alongside the real
        // one (w8:p1), excluding our pane leaves the real agent unambiguous (HH-NOT-SELF).
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p5", "w8:t1", "w8")];
        assert_eq!(
            picked(&agents, Some("w8:t1"), Some("w8"), Some("w8:p5")),
            Picked::One("w8:p1".into())
        );
    }

    #[test]
    fn non_agent_panes_do_not_make_the_tab_ambiguous() {
        // A tab holding one real agent plus a non-agent pane (another plugin's sidebar, a
        // plain shell) resolves to the agent, not the picker (HH-AGENT-PANES).
        let agents = vec![agent("w3:p1", "w3:t1", "w3"), non_agent_pane("w3:p4", "w3:t1", "w3")];
        assert_eq!(
            picked(&agents, Some("w3:t1"), Some("w3"), Some("w3:p5")),
            Picked::One("w3:p1".into())
        );
    }

    #[test]
    fn only_non_agent_panes_resolve_to_none() {
        // A tab and workspace holding nothing but non-agent panes has no one to send to
        // (HH-AGENT-PANES, HH-ZERO-REFUSE).
        let agents =
            vec![non_agent_pane("w3:p2", "w3:t1", "w3"), non_agent_pane("w3:p4", "w3:t1", "w3")];
        assert_eq!(picked(&agents, Some("w3:t1"), Some("w3"), None), Picked::None);
    }

    #[test]
    fn no_matching_agent_resolves_to_none() {
        let agents = vec![agent("w9:p1", "w9:t1", "w9")];
        // An agent exists, but in another workspace entirely (HH-ZERO-REFUSE).
        assert_eq!(picked(&agents, Some("w8:t1"), Some("w8"), None), Picked::None);
    }

    #[test]
    fn two_workspace_agents_go_to_the_picker_with_the_exact_set() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t2", "w8")];
        // Neither shares our tab and the workspace has two — the picker lists exactly them
        // (HH-MANY-PICK), in list order.
        assert_eq!(
            picked(&agents, Some("w8:tZ"), Some("w8"), None),
            Picked::Many(vec!["w8:p1".into(), "w8:p2".into()])
        );
    }

    #[test]
    fn two_tab_agents_go_to_the_picker_even_without_a_workspace_id() {
        let agents = vec![agent("w8:p1", "w8:t1", "w8"), agent("w8:p2", "w8:t1", "w8")];
        // Two agents share our tab and no workspace id is available to widen the scope — still
        // the picker over both, not a no-agent refusal (HH-MANY-PICK; preserves the pre-picker
        // ambiguity boundary).
        assert_eq!(
            picked(&agents, Some("w8:t1"), None, None),
            Picked::Many(vec!["w8:p1".into(), "w8:p2".into()])
        );
    }

    #[test]
    fn parse_agents_accepts_the_envelope_and_defaults_absent_fields() {
        let wrapped = r#"{"result":{"agents":[{"agent":"claude","agent_status":"working","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8"}]}}"#;
        // name/terminal_title_stripped are absent → None (agent() sets both None).
        assert_eq!(parse_agents(wrapped).unwrap(), [agent("w8:p1", "w8:t1", "w8")]);
        assert!(parse_agents("[]").is_err());
        assert_eq!(serde_json::from_str::<Status>(r#""starting""#).unwrap(), Status::Unknown);
    }

    #[test]
    fn parse_agents_reads_the_name_and_title_fields_when_present() {
        // Pins the exact `herdr agent list` key spelling the picker's labels depend on: a
        // rename on herdr's side would otherwise deserialize silently to None and blank the
        // row, with every other test still green.
        let json = r#"{"result":{"agents":[{"agent":"codex","agent_status":"idle","pane_id":"w:p1","tab_id":"w:t1","workspace_id":"w","name":"fbig-import","terminal_title_stripped":"port to kotlin"}]}}"#;
        let parsed = parse_agents(json).unwrap();
        assert_eq!(parsed[0].name.as_deref(), Some("fbig-import"));
        assert_eq!(parsed[0].terminal_title_stripped.as_deref(), Some("port to kotlin"));
    }

    #[test]
    fn parse_tab_labels_reads_labels_and_skips_unlabeled_tabs() {
        // Pins the `herdr tab list` key spelling (`tabs[].label`) the tab-note column depends on.
        let json =
            r#"{"result":{"tabs":[{"tab_id":"w:t1","label":"upgrade test"},{"tab_id":"w:t2"}]}}"#;
        let labels = parse_tab_labels(json).unwrap();
        assert_eq!(labels.get("w:t1").map(String::as_str), Some("upgrade test"));
        assert!(!labels.contains_key("w:t2"), "a tab with no label is skipped");
    }

    #[test]
    fn row_lead_prefers_name_then_kind() {
        assert_eq!(choice("codex", Some("fbig-import"), None, "t").lead(), "fbig-import");
        assert_eq!(choice("claude", None, None, "t").lead(), "claude");
        // An empty-string name is treated as unset.
        assert_eq!(choice("codex", Some(""), None, "t").lead(), "codex");
    }

    #[test]
    fn row_tab_note_is_shown_only_when_it_adds_over_the_kind() {
        // A meaningful tab label shows.
        assert_eq!(
            choice("codex", None, Some("upgrade test"), "t").tab_note(),
            Some("upgrade test")
        );
        // A tab label that just repeats the kind is omitted as redundant.
        assert_eq!(choice("codex", None, Some("codex"), "t").tab_note(), None);
        // No tab label (join missing / no workspace id) omits it.
        assert_eq!(choice("claude", None, None, "t").tab_note(), None);
        // An empty-string label is treated as absent.
        assert_eq!(choice("claude", None, Some(""), "t").tab_note(), None);
    }
}
