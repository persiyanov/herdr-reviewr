//! herdr host integration: resolve the agent pane to send to, sample the agents turn
//! tracking watches, ask herdr for the plugin config directory, and stamp/clear the
//! pane's cosmetic `reviewr` label.
//!
//! See `specs/herdr-host.md`. Uses the herdr CLI via `$HERDR_BIN_PATH`. The two agent readers
//! ask different questions and neither narrows the other: [`send_target`] resolves candidates
//! from the reviewr pane's herdr workspace, while [`agent_samples`] reports every agent and lets
//! the caller decide membership by worktree. Browsing and the clipboard export never come
//! through here.

use std::collections::HashMap;
use std::env;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::logln;
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

/// One entry of `herdr agent list`. The picker-facing fields are optional: herdr 0.7.5 omits
/// `name`, `display_agent`, and `state_labels` entirely until something sets them, and
/// `herdr agent rename --clear` leaves `name` present and null. Both parse to `None`. The
/// identity fields stay required, so a payload missing `pane_id` fails the parse loudly
/// instead of minting an unaddressable send target.
///
/// `agent_status` is kept as herdr spelled it, not as the [`Status`] it parses to: the picker
/// row shows the spelling and looks its label up by it, so a state herdr adds must survive a
/// round trip reviewr does not understand (`specs/herdr-host.md`).
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct AgentPane {
    agent: Option<String>,
    agent_status: String,
    pane_id: String,
    tab_id: String,
    workspace_id: String,
    /// Where the agent works. Turn tracking resolves it to a git top level to decide which
    /// worktree the agent belongs to (`specs/herdr-host.md`).
    cwd: Option<String>,
    name: Option<String>,
    display_agent: Option<String>,
    state_labels: Option<HashMap<String, String>>,
}

/// One picker row: the pane the send addresses, and the three parts the row shows
/// (`specs/herdr-host.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentChoice {
    pub pane_id: String,
    pub name: String,
    pub state: String,
    pub tab: String,
}

/// What `Send` does with the agents herdr reports (`specs/herdr-host.md`). A refusal is the
/// `Err` of [`send_target`], so zero agents and a failed enumeration land in one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendTarget {
    /// Exactly one agent. The send goes straight to it, with no picker.
    One(AgentChoice),
    /// Several agents, in herdr's own order. The picker opens over them.
    Many(Vec<AgentChoice>),
}

fn herdr_bin() -> String {
    env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

/// Run a herdr subcommand and return its stdout.
///
/// Nothing shows this error to a reviewer: every caller either replaces it with a sentence of its
/// own or drops it. So the whole of it — the argv, which carries a review's text in `pane
/// send-text`, and herdr's JSON error envelope — goes to the log and only there.
fn herdr(args: &[&str]) -> Result<String> {
    let out = match crate::proc::command(herdr_bin()).args(args).output() {
        Ok(out) => out,
        Err(e) => {
            logln!("herdr {args:?} could not run: {e}");
            bail!("herdr could not run");
        }
    };
    if !out.status.success() {
        logln!("herdr {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        bail!("herdr refused");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// How long a startup or exit path waits for a herdr answer before moving on. The call keeps
/// running on its own thread — only the wait is bounded — so a wedged herdr costs at most
/// this once and never wedges reviewr with it: not the first paint, not the event loop's
/// entry, and not the shell prompt after exit.
const ANSWER_BOUND: Duration = Duration::from_secs(2);

/// How long a herdr answer may take before the caller signals the wait. Under this, the
/// answer is effectively instant and nothing flashes; over it, the caller says what it is
/// waiting on, so a slow answer never swaps the screen silently
/// (`policies/ux-responsiveness.md`).
const SIGNAL_DELAY: Duration = Duration::from_millis(150);

/// Run a herdr subcommand on its own thread and hand back the channel its answer lands on.
/// Dropping the receiver makes the call fire-and-forget; the thread still reaps the child
/// either way, and a failure logs inside [`herdr`] as usual.
fn herdr_on_thread(args: Vec<String>) -> mpsc::Receiver<Result<String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = tx.send(herdr(&refs));
    });
    rx
}

/// Stamp our own pane's cosmetic `reviewr` label — but only when the pane carries no label,
/// so a name the user gave their pane survives running reviewr in it (`specs/herdr-host.md`
/// Pane identity: reviewr supplies the default name, never overrides one). Display only:
/// the actions and the event identify a reviewr pane by its foreground process, never this
/// label, so a failed read or write just logs — and nothing waits on it, so a hung herdr
/// cannot sit between the first paint and the event loop. Without a pane id — outside
/// herdr — a no-op.
pub fn label_pane() {
    let (Ok(ws), Ok(pane)) = (env::var("HERDR_WORKSPACE_ID"), env::var("HERDR_PANE_ID")) else {
        return;
    };
    thread::spawn(move || {
        // An unreadable listing stamps anyway: with herdr wedged the rename fails too,
        // and both failures land in the log.
        if current_label(&ws, &pane).is_none() {
            let _ = herdr(&["pane", "rename", &pane, "reviewr"]);
        }
    });
}

/// Clear the cosmetic label on a normal exit — but only a `reviewr` label, so a name the
/// user set is never deleted (`specs/herdr-host.md` Pane identity). The wait is bounded:
/// this runs after the terminal is restored, and a hung herdr must not hold the shell
/// prompt hostage for a label a stale copy of which changes nothing.
pub fn clear_pane_label() {
    let (Ok(ws), Ok(pane)) = (env::var("HERDR_WORKSPACE_ID"), env::var("HERDR_PANE_ID")) else {
        return;
    };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if current_label(&ws, &pane).as_deref() == Some("reviewr") {
            let _ = herdr(&["pane", "rename", &pane, "--clear"]);
        }
        let _ = tx.send(());
    });
    if rx.recv_timeout(ANSWER_BOUND).is_err() {
        logln!("pane label clear unanswered after {ANSWER_BOUND:?}; leaving the label");
    }
}

/// Our pane's current label from `pane list`, or `None` when it has none or the listing
/// fails. Blocking — the label threads call it, never the frame loop.
fn current_label(ws: &str, pane: &str) -> Option<String> {
    parse_pane_label(&herdr(&["pane", "list", "--workspace", ws]).ok()?, pane)
}

/// The `label` of pane `pane` in a `pane list` envelope. Absent key, empty label, unknown
/// pane, and an unparseable envelope all read as no label.
fn parse_pane_label(json: &str, pane: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Response {
        result: PaneList,
    }
    #[derive(Deserialize)]
    struct PaneList {
        panes: Vec<PaneEntry>,
    }
    #[derive(Deserialize)]
    struct PaneEntry {
        pane_id: String,
        #[serde(default)]
        label: Option<String>,
    }
    let response: Response = serde_json::from_str(json).ok()?;
    response
        .result
        .panes
        .into_iter()
        .find(|entry| entry.pane_id == pane)?
        .label
        .filter(|label| !label.is_empty())
}

/// The config directory herdr resolves for this plugin, from `herdr plugin config-dir`.
/// `None` — herdr absent, refusing, or not answering — means no config directory, never an
/// error (`specs/config.md`).
pub fn plugin_config_dir() -> Option<String> {
    plugin_config_dir_with(|| ())
}

/// [`plugin_config_dir`] with a slow-answer signal: `on_slow` runs once if the answer takes
/// longer than [`SIGNAL_DELAY`], so the pane can say what it is waiting on instead of
/// silently swapping a painted frame later. The pane calls this only after its first paint,
/// and the total wait stays bounded by [`ANSWER_BOUND`], so a wedged herdr degrades a
/// visible pane to the defaults instead of holding the blank grid issue #4 fixed.
pub fn plugin_config_dir_with(on_slow: impl FnOnce()) -> Option<String> {
    let rx =
        herdr_on_thread(vec!["plugin".into(), "config-dir".into(), "persiyanov.reviewr".into()]);
    let answer = if let Ok(answer) = rx.recv_timeout(SIGNAL_DELAY) {
        answer
    } else {
        on_slow();
        let Ok(answer) = rx.recv_timeout(ANSWER_BOUND.saturating_sub(SIGNAL_DELAY)) else {
            logln!("plugin config-dir unanswered after {ANSWER_BOUND:?}; no config directory");
            return None;
        };
        answer
    };
    let out = answer.ok()?;
    let dir = out.trim();
    (!dir.is_empty()).then(|| dir.to_owned())
}

/// The (workspace, pane) id pair identifying this reviewr pane in the herdr environment. There is
/// no tab here on purpose: the send scopes to the workspace and turn tracking scopes to the
/// worktree, so nothing reads `HERDR_TAB_ID` and the reviewr pane's placement changes neither.
fn agent_env() -> (Option<String>, Option<String>) {
    (env::var("HERDR_WORKSPACE_ID").ok(), env::var("HERDR_PANE_ID").ok())
}

/// The agents herdr currently lists. The one place the `agent list` call and its envelope
/// parsing live, shared by the send's pane resolution and turn tracking's sampling.
fn agent_list() -> Result<Vec<AgentPane>> {
    parse_agents(&herdr(&["agent", "list"])?)
}

/// What `Send` does: one workspace agent sends directly, several open the picker, and no
/// agent refuses (`specs/herdr-host.md`). A failed enumeration refuses too, but says so rather
/// than reporting a count herdr never gave. Either refusal is the whole status line, so both
/// stay one short sentence naming the clipboard the reviewer can fall back to.
pub fn send_target() -> Result<SendTarget> {
    let (ws, me) = agent_env();
    let agents = match agent_list() {
        Ok(agents) => agents,
        Err(e) => {
            // A refusal is the whole status line, so it says the clipboard rather than herdr's
            // own wording. The cause is already in the log, with the argv `herdr` kept out of it.
            logln!("agent list failed: {e:#}");
            bail!("herdr did not answer — copy to the clipboard instead")
        }
    };
    // Candidacy is decided once, here: an `agent` field, our workspace, not our own pane.
    // Rows keep `agent list` order, which is herdr's own (`specs/herdr-host.md`). Turn
    // tracking does not come through here: it asks where each agent works instead.
    let picked = candidates(&agents, ws.as_deref(), me.as_deref());
    match picked.len() {
        0 => bail!("no agent here — copy to the clipboard instead"),
        // The sole-agent send shows no row, so only the picker pays for the tab-label call.
        1 => Ok(SendTarget::One(picked[0].choice(&HashMap::new()))),
        _ => {
            let tabs = tab_labels(ws.as_deref());
            Ok(SendTarget::Many(picked.into_iter().map(|agent| agent.choice(&tabs)).collect()))
        }
    }
}

impl AgentPane {
    /// This pane as a picker row (`specs/herdr-host.md`).
    fn choice(&self, tabs: &HashMap<String, String>) -> AgentChoice {
        AgentChoice {
            pane_id: self.pane_id.clone(),
            name: self.row_name(),
            state: self.row_state(),
            tab: tabs.get(&self.tab_id).cloned().unwrap_or_default(),
        }
    }

    /// The agent's `name`, else its `display_agent`, else its kind (`specs/herdr-host.md`).
    /// A cleared name arrives as null and falls through like an absent one. The pane id is a
    /// last resort no live agent reaches, so the row and the success line always name something.
    fn row_name(&self) -> String {
        [&self.name, &self.display_agent, &self.agent]
            .into_iter()
            .flatten()
            .find(|part| !part.is_empty())
            .cloned()
            .unwrap_or_else(|| self.pane_id.clone())
    }

    /// The agent's `state_labels` entry for its state, else the state itself. Both the lookup
    /// key and the fallback are herdr's own spelling, so a state reviewr does not know still
    /// names itself on the row instead of reading `unknown` (`specs/herdr-host.md`).
    fn row_state(&self) -> String {
        self.state_labels
            .as_ref()
            .and_then(|labels| labels.get(&self.agent_status))
            .filter(|label| !label.is_empty())
            .cloned()
            .unwrap_or_else(|| self.agent_status.clone())
    }

    /// The lifecycle status turn tracking reads from this pane.
    fn status(&self) -> Status {
        Status::from_wire(&self.agent_status)
    }

    /// A real agent pane other than our own — the shared gate both readers apply, so turn
    /// sampling and send targeting never drift on what counts as an agent
    /// (`../docs/herdr-api-notes.md`).
    fn is_agent_other_than(&self, me: Option<&str>) -> bool {
        self.agent.is_some() && Some(self.pane_id.as_str()) != me
    }
}

/// Tab id to tab label for one workspace. Labelling is best effort: a failed call or a
/// missing tab leaves the row's tab part empty rather than failing the send.
fn tab_labels(ws: Option<&str>) -> HashMap<String, String> {
    let Some(ws) = ws else { return HashMap::new() };
    let Ok(json) = herdr(&["tab", "list", "--workspace", ws]) else {
        return HashMap::new();
    };
    parse_tab_labels(&json).unwrap_or_default()
}

/// The documented `result.tabs` array from `herdr tab list`, as tab id → label. A tab
/// without a label is dropped, so its rows show no tab part.
fn parse_tab_labels(json: &str) -> Result<HashMap<String, String>> {
    let response: TabListResponse = serde_json::from_str(json).context("parsing tab list")?;
    Ok(response
        .result
        .tabs
        .into_iter()
        .filter_map(|tab| tab.label.map(|label| (tab.tab_id, label)))
        .collect())
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
    #[serde(default)]
    label: Option<String>,
}

/// The documented `result.agents` array from `herdr agent list`.
fn parse_agents(json: &str) -> Result<Vec<AgentPane>> {
    let response: AgentListResponse = serde_json::from_str(json).context("parsing agent list")?;
    Ok(response.result.agents)
}

/// One agent as turn tracking sees it: where it works, and what it is doing. Membership is
/// the caller's to decide, since only the worker knows the reviewed worktree
/// (`specs/herdr-host.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSample {
    pub cwd: Option<String>,
    pub status: Status,
}

/// Every agent herdr reports, minus our own pane. Neither the tab nor the workspace narrows
/// this (see the module header). `Err` means the enumeration failed, which the caller treats
/// as "nothing changed" rather than "no agents".
pub fn agent_samples() -> Result<Vec<AgentSample>> {
    let (_, me) = agent_env();
    Ok(samples_of(agent_list()?, me.as_deref()))
}

/// The sampling rule, split out so it is testable without the CLI. Only entries carrying an
/// `agent` field count, and our own pane never does.
fn samples_of(agents: Vec<AgentPane>, me: Option<&str>) -> Vec<AgentSample> {
    agents
        .into_iter()
        .filter(|agent| agent.is_agent_other_than(me))
        .map(|agent| AgentSample { status: agent.status(), cwd: agent.cwd })
        .collect()
}

/// The real agents in workspace `ws`, ignoring our own pane `me`. Only entries carrying an
/// `agent` field count. herdr 0.7.5 already keeps non-agent panes
/// out of `agent list`, so both filters are defensive: a reviewr pane or a plain shell shows
/// up in `pane list` without an `agent` key and never here (`../docs/herdr-api-notes.md`).
fn candidates<'a>(
    agents: &'a [AgentPane],
    ws: Option<&str>,
    me: Option<&str>,
) -> Vec<&'a AgentPane> {
    let Some(ws) = ws else { return Vec::new() };
    agents
        .iter()
        .filter(|agent| agent.is_agent_other_than(me))
        .filter(|agent| agent.workspace_id == ws)
        .collect()
}

/// Write literal text into the agent pane's input, without submitting.
///
/// Uses `pane send-text`, not the agent-level send: herdr 0.7.5 replaced `agent send` with
/// the logical-key `agent send-keys`, while `pane send-text` has carried the literal-text,
/// no-Enter semantics unchanged since 0.7.0 (`docs/herdr-api-notes.md`).
pub fn send_text(pane: &str, text: &str) -> Result<()> {
    herdr(&["pane", "send-text", pane, &pasted(text)])?;
    Ok(())
}

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// The batch as one bracketed paste event, never raw bytes: a paste inserts verbatim in any
/// input mode, where raw bytes execute as commands in a vim-style input resting in normal
/// mode (`specs/herdr-host.md`). A terminator inside the batch would end the frame early and
/// hand the tail to the command interpreter. The body is rebuilt with a suffix check per
/// character, so a terminator never survives, not even one spliced together by an earlier
/// removal — and the send stays linear, where a delete-and-rescan loop is quadratic on
/// splice-heavy input and stalls the frame loop mid-send.
fn pasted(text: &str) -> String {
    let mut body = String::with_capacity(text.len());
    for ch in text.chars() {
        body.push(ch);
        if body.ends_with(PASTE_END) {
            body.truncate(body.len() - PASTE_END.len());
        }
    }
    format!("{PASTE_START}{body}{PASTE_END}")
}

/// Focus the agent pane so the reviewer can add context and submit.
pub fn focus(pane: &str) -> Result<()> {
    herdr(&["agent", "focus", pane])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AgentChoice, AgentPane, HashMap, Status, parse_agents, parse_tab_labels};

    /// One agent entry shaped like the real `herdr agent list` output (api notes).
    fn agent(pane: &str, tab: &str, ws: &str) -> AgentPane {
        AgentPane {
            agent: Some("claude".to_string()),
            agent_status: "working".to_string(),
            pane_id: pane.to_string(),
            tab_id: tab.to_string(),
            workspace_id: ws.to_string(),
            ..AgentPane::default()
        }
    }

    /// One non-agent pane as herdr 0.7.1 lists it live: `agent_status: unknown`, no `agent`
    /// field — a reviewr pane or a plain shell.
    fn non_agent_pane(pane: &str, tab: &str, ws: &str) -> AgentPane {
        AgentPane {
            agent: None,
            agent_status: "unknown".to_string(),
            pane_id: pane.to_string(),
            tab_id: tab.to_string(),
            workspace_id: ws.to_string(),
            ..AgentPane::default()
        }
    }

    /// The picker-row mapping `send_target`'s Many arm applies to the workspace candidates.
    fn rows(
        agents: &[AgentPane],
        ws: Option<&str>,
        me: Option<&str>,
        tabs: &HashMap<String, String>,
    ) -> Vec<AgentChoice> {
        super::candidates(agents, ws, me).into_iter().map(|agent| agent.choice(tabs)).collect()
    }

    #[test]
    fn sampling_keeps_every_tab_and_workspace() {
        // Turn tracking asks where an agent works, never where its pane sits, so neither the
        // reviewr pane's tab nor its workspace narrows the sample (HH-TURN-PER-WORKTREE).
        // This is what makes the `tab` placement track exactly like `split`.
        let agents = vec![
            AgentPane { cwd: Some("/w/one".into()), ..agent("w8:p1", "w8:t1", "w8") },
            AgentPane { cwd: Some("/w/two".into()), ..agent("w8:p2", "w8:t2", "w8") },
            AgentPane { cwd: Some("/w/three".into()), ..agent("w9:p1", "w9:t1", "w9") },
        ];
        let cwds: Vec<_> =
            super::samples_of(agents, None).into_iter().filter_map(|s| s.cwd).collect();
        assert_eq!(cwds, ["/w/one", "/w/two", "/w/three"]);
    }

    #[test]
    fn sampling_drops_our_own_pane_and_every_non_agent_pane() {
        let agents = vec![
            AgentPane { cwd: Some("/w/real".into()), ..agent("w3:p1", "w3:t1", "w3") },
            AgentPane { cwd: Some("/w/shell".into()), ..non_agent_pane("w3:p4", "w3:t1", "w3") },
            AgentPane { cwd: Some("/w/self".into()), ..agent("w3:p5", "w3:t1", "w3") },
        ];
        let cwds: Vec<_> =
            super::samples_of(agents, Some("w3:p5")).into_iter().filter_map(|s| s.cwd).collect();
        assert_eq!(cwds, ["/w/real"]);
    }

    #[test]
    fn a_sample_carries_the_status_tracking_folds() {
        let agents = vec![AgentPane {
            agent_status: "blocked".into(),
            cwd: Some("/w/one".into()),
            ..agent("w8:p1", "w8:t1", "w8")
        }];
        assert_eq!(super::samples_of(agents, None)[0].status, Status::Blocked);
    }

    /// One agent carrying the picker-facing fields herdr omits until something sets them.
    fn named(pane: &str, tab: &str, ws: &str, name: Option<&str>) -> AgentPane {
        AgentPane { name: name.map(str::to_string), ..agent(pane, tab, ws) }
    }

    #[test]
    fn a_row_name_prefers_the_rename_then_the_display_agent_then_the_kind() {
        // `herdr agent rename` sets `name`, which wins (specs/herdr-host.md).
        assert_eq!(named("w8:p1", "w8:t1", "w8", Some("release-bot")).row_name(), "release-bot");
        // `--clear` leaves the key present and null, which falls through like an absent one.
        let cleared = named("w8:p1", "w8:t1", "w8", None);
        assert_eq!(cleared.row_name(), "claude");
        // With no kind either, the pane id keeps the row and the success line from going blank.
        let anonymous = AgentPane { agent: None, ..agent("w8:p1", "w8:t1", "w8") };
        assert_eq!(anonymous.row_name(), "w8:p1");
        let displayed = AgentPane {
            agent: None,
            display_agent: Some("Claude".into()),
            ..agent("w8:p1", "w8:t1", "w8")
        };
        assert_eq!(displayed.row_name(), "Claude");
    }

    #[test]
    fn a_row_state_prefers_the_state_label_over_the_wire_spelling() {
        let mut labels = HashMap::new();
        labels.insert("working".to_string(), "thinking".to_string());
        let labelled = AgentPane { state_labels: Some(labels), ..agent("w8:p1", "w8:t1", "w8") };
        assert_eq!(labelled.row_state(), "thinking");
        // herdr 0.7.5 sends no `state_labels`, so every live row falls back to the state itself.
        assert_eq!(agent("w8:p1", "w8:t1", "w8").row_state(), "working");
    }

    #[test]
    fn picker_rows_are_every_workspace_agent_in_herdr_order_with_its_tab_label() {
        let agents = vec![
            agent("w8:p1", "w8:t1", "w8"),
            non_agent_pane("w8:p4", "w8:t1", "w8"),
            named("w8:p2", "w8:t2", "w8", Some("release-bot")),
            agent("w9:p1", "w9:t1", "w9"),
        ];
        let mut tabs = HashMap::new();
        tabs.insert("w8:t1".to_string(), "Grip Outreach".to_string());
        // w8:t2 has no label, so that row shows its state alone.
        let rows = rows(&agents, Some("w8"), Some("w8:p9"), &tabs);
        assert_eq!(
            rows,
            vec![
                AgentChoice {
                    pane_id: "w8:p1".into(),
                    name: "claude".into(),
                    state: "working".into(),
                    tab: "Grip Outreach".into(),
                },
                AgentChoice {
                    pane_id: "w8:p2".into(),
                    name: "release-bot".into(),
                    state: "working".into(),
                    tab: String::new(),
                },
            ]
        );
    }

    #[test]
    fn picker_rows_exclude_our_own_pane_and_every_non_agent_pane() {
        // A shell and our own pane are not candidates, so neither becomes a row.
        let agents = vec![
            agent("w3:p1", "w3:t1", "w3"),
            non_agent_pane("w3:p4", "w3:t1", "w3"),
            agent("w3:p5", "w3:t1", "w3"),
        ];
        let rows_of = |ws| rows(&agents, ws, Some("w3:p5"), &HashMap::new());
        let picked: Vec<_> = rows_of(Some("w3")).iter().map(|r| r.pane_id.clone()).collect();
        assert_eq!(picked, ["w3:p1"]);
        // No workspace id means no candidates — never every agent on the machine.
        assert!(rows_of(None).is_empty());
    }

    #[test]
    fn an_agent_list_entry_parses_without_any_of_the_picker_fields() {
        // Exactly what herdr 0.7.5 emits: no `name`, no `display_agent`, no `state_labels`.
        let json = r#"{"result":{"agents":[{"agent":"claude","agent_status":"idle","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8"}]}}"#;
        let parsed = parse_agents(json).unwrap();
        assert_eq!(parsed[0].row_name(), "claude");
        assert_eq!(parsed[0].row_state(), "idle");
        // And with `name` explicitly null, as `herdr agent rename --clear` leaves it.
        let cleared = r#"{"result":{"agents":[{"agent":"codex","agent_status":"idle","pane_id":"w8:p2","tab_id":"w8:t1","workspace_id":"w8","name":null}]}}"#;
        assert_eq!(parse_agents(cleared).unwrap()[0].row_name(), "codex");
    }

    #[test]
    fn a_send_wraps_the_batch_in_one_bracketed_paste_frame() {
        // Issue #41's repro string: sent raw, vim ate the leading `b` and `i`.
        assert_eq!(
            super::pasted("bit/DESIGN.md:95 note"),
            "\x1b[200~bit/DESIGN.md:95 note\x1b[201~"
        );
    }

    #[test]
    fn an_embedded_paste_terminator_cannot_end_the_frame_early() {
        // A diff snippet is raw file content and can carry the terminator. The second
        // input splices one together across a removal.
        assert_eq!(super::pasted("a\x1b[201~b"), "\x1b[200~ab\x1b[201~");
        assert_eq!(super::pasted("a\x1b[201\x1b[201~~b"), "\x1b[200~ab\x1b[201~");
    }

    #[test]
    fn a_pane_label_reads_only_our_pane_and_absent_or_empty_is_none() {
        // The live `pane list` entry shape (docs/herdr-api-notes.md): `label` appears only
        // on labeled panes. The label logic stamps the unlabeled and clears only its own.
        let json = r#"{"result":{"panes":[{"pane_id":"w1:p1","label":"build"},{"pane_id":"w1:p2"},{"pane_id":"w1:p3","label":""}]}}"#;
        assert_eq!(super::parse_pane_label(json, "w1:p1").as_deref(), Some("build"));
        assert_eq!(super::parse_pane_label(json, "w1:p2"), None);
        assert_eq!(super::parse_pane_label(json, "w1:p3"), None, "empty label reads as none");
        assert_eq!(super::parse_pane_label(json, "w9:p9"), None, "unknown pane reads as none");
        assert_eq!(super::parse_pane_label("[]", "w1:p1"), None, "junk envelope reads as none");
    }

    #[test]
    fn a_tab_list_parses_to_labels_and_an_unlabelled_tab_is_dropped() {
        // The documented envelope (docs/herdr-api-notes.md): `label` can be absent.
        let json = r#"{"result":{"tabs":[{"tab_id":"w8:t1","label":"Grip Outreach","number":1,"pane_count":2},{"tab_id":"w8:t2","number":2,"pane_count":1}]}}"#;
        let labels = parse_tab_labels(json).unwrap();
        assert_eq!(labels.get("w8:t1").map(String::as_str), Some("Grip Outreach"));
        assert!(!labels.contains_key("w8:t2"));
        assert!(parse_tab_labels("[]").is_err());
    }

    #[test]
    fn parse_agents_accepts_only_the_documented_envelope() {
        // `cwd` is asserted from the wire on purpose: it is the one field worktree
        // membership rides on, so a renamed key must fail here, not silently in production.
        let wrapped = r#"{"result":{"agents":[{"agent":"claude","agent_status":"working","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","cwd":"/w/one"}]}}"#;
        assert_eq!(
            parse_agents(wrapped).unwrap(),
            [AgentPane { cwd: Some("/w/one".into()), ..agent("w8:p1", "w8:t1", "w8") }]
        );
        assert!(parse_agents("[]").is_err());
    }

    #[test]
    fn a_state_herdr_adds_names_itself_on_the_row_and_is_unknown_to_tracking() {
        let bare = r#"{"result":{"agents":[{"agent":"claude","agent_status":"compacting","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8"}]}}"#;
        let parsed = parse_agents(bare).unwrap();
        assert_eq!(parsed[0].row_state(), "compacting", "the row shows herdr's own spelling");
        assert_eq!(parsed[0].status(), Status::Unknown, "tracking folds it to unknown");
        // And the spelling is the `state_labels` key, so herdr can label a state reviewr has
        // never heard of (`specs/herdr-host.md`).
        let labelled = r#"{"result":{"agents":[{"agent":"claude","agent_status":"compacting","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","state_labels":{"compacting":"Compacting"}}]}}"#;
        assert_eq!(parse_agents(labelled).unwrap()[0].row_state(), "Compacting");
    }
}
