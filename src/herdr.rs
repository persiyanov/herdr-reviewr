//! herdr integration: the CLI via `$HERDR_BIN_PATH`, and the send over its socket API.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::logln;
use crate::turn::Status;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AgentList {
    agents: Vec<AgentPane>,
}

/// herdr's optional strings: null, absent and `""` all read as `None`.
fn non_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.filter(|text| !text.is_empty()))
}

/// One `herdr agent list` entry: identity fields required, picker fields optional.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct AgentPane {
    #[serde(default, deserialize_with = "non_empty")]
    agent: Option<String>,
    agent_status: String,
    pane_id: String,
    tab_id: String,
    workspace_id: String,
    /// Where the agent works: turn tracking maps it to a worktree.
    #[serde(default, deserialize_with = "non_empty")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "non_empty")]
    name: Option<String>,
    #[serde(default, deserialize_with = "non_empty")]
    display_agent: Option<String>,
    state_labels: Option<HashMap<String, String>>,
}

/// One picker row: the pane the send addresses, and the three parts the row shows
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentChoice {
    pub pane_id: String,
    pub name: String,
    pub state: String,
    pub tab: String,
}

/// What `Send` does with the agents herdr reports; a refusal is [`send_target`]'s `Err`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendTarget {
    /// Exactly one agent. The send goes straight to it, with no picker.
    One(AgentChoice),
    /// Several agents, in herdr's own order. The picker opens over them.
    Many(Vec<AgentChoice>),
}

/// The plugin's id, as herdr knows it: its config dir, its state dir, its pane entrypoint.
pub(crate) const PLUGIN_ID: &str = "persiyanov.reviewr";

/// A herdr context variable; herdr leaves one unset or empty alike.
pub(crate) fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

/// A herdr path variable, which need not be UTF-8; unset or empty alike.
pub(crate) fn var_os(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

/// The plugin id herdr runs this as, else the published one.
pub(crate) fn plugin_id() -> String {
    var("HERDR_PLUGIN_ID").unwrap_or_else(|| PLUGIN_ID.to_owned())
}

/// The label reviewr stamps on its pane and tab; display only, never identity.
pub(crate) const LABEL: &str = "reviewr";

/// The herdr binary herdr names, else `herdr` on `PATH`. An empty value names nothing.
fn herdr_bin() -> String {
    var("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into())
}

/// How a herdr call failed, classified so a caller can tell a benign race from a real failure.
#[derive(Debug, PartialEq, Eq)]
pub enum HerdrError {
    /// herdr gave no answer: not run, not reachable, or not within its bound.
    Unanswered,
    /// herdr exited non-zero, with its error envelope's `error.code` when it wrote one.
    Refused(Option<String>),
    /// herdr exited 0 without the shape the call documents, never read as empty.
    Unreadable,
    /// The addressed pane no longer exists: it exited between an earlier read and this call.
    PaneGone,
}

/// Why a review did not reach an agent's input.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// herdr failed a call the send made.
    Herdr(HerdrError),
    /// The named agent waits on a permission or confirm prompt, which would drop a paste.
    AtPrompt(String),
    /// The workspace holds no agent to send to.
    NoAgent,
    /// The review is over the send cap, so it cannot go as one paste.
    TooLarge,
}

impl From<HerdrError> for SendError {
    fn from(error: HerdrError) -> Self {
        Self::Herdr(error)
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Herdr(error) => error.fmt(f),
            Self::AtPrompt(name) => write!(f, "{name} is at a prompt"),
            Self::NoAgent => write!(f, "no agent in the workspace"),
            Self::TooLarge => write!(f, "the review is over the {MAX_REQUEST_BYTES}-byte send cap"),
        }
    }
}

impl std::error::Error for SendError {}

impl HerdrError {
    /// A refusal carrying herdr's `error.code`, a gone pane read as such.
    fn refused(code: Option<String>) -> Self {
        match code.as_deref() {
            Some("pane_not_found") => Self::PaneGone,
            _ => Self::Refused(code),
        }
    }
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unanswered => write!(f, "herdr didn't answer"),
            Self::Refused(Some(code)) => write!(f, "herdr refused: {code}"),
            Self::Refused(None) => write!(f, "herdr refused"),
            Self::Unreadable => write!(f, "herdr answered in an unknown shape"),
            Self::PaneGone => write!(f, "the pane is gone"),
        }
    }
}

impl std::error::Error for HerdrError {}

/// Run a herdr subcommand: its stdout, or the classified failure, logged in full.
fn call(args: &[&str]) -> Result<String, HerdrError> {
    use crate::proc::RunError;
    let mut cmd = crate::proc::command(herdr_bin());
    cmd.args(args);
    let deadline = Instant::now() + CALL_BOUND;
    match crate::proc::run_tree(cmd, || Instant::now() >= deadline) {
        Ok(stdout) => Ok(stdout),
        Err(RunError::Failed { stderr }) => {
            logln!("herdr {args:?} failed: {}", stderr.trim());
            Err(HerdrError::refused(error_code(&stderr)))
        }
        Err(RunError::Stopped) => {
            logln!("herdr {args:?} unanswered after {CALL_BOUND:?}");
            Err(HerdrError::Unanswered)
        }
        Err(error) => {
            logln!("herdr {args:?} could not run: {error:?}");
            Err(HerdrError::Unanswered)
        }
    }
}

/// How long one herdr CLI call may run, so a wedged herdr never holds an action's lock.
pub(crate) const CALL_BOUND: Duration = Duration::from_secs(8);

/// The `error.code` of the envelope a failed call writes to stderr, read line by line.
fn error_code(stderr: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Envelope {
        error: ErrorBody,
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        code: String,
    }
    stderr
        .lines()
        .find_map(|line| serde_json::from_str::<Envelope>(line.trim()).ok())
        .map(|envelope| envelope.error.code)
}

/// The `result` of a herdr JSON answer as `T`, else [`HerdrError::Unreadable`].
fn answer<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, HerdrError> {
    #[derive(Deserialize)]
    struct Answer<T> {
        result: T,
    }
    serde_json::from_str::<Answer<T>>(json).map(|answer| answer.result).map_err(|error| {
        logln!("herdr answer unreadable: {error}");
        HerdrError::Unreadable
    })
}

/// One `herdr pane list` snapshot of a workspace.
#[derive(Debug, Deserialize)]
pub(crate) struct PaneList {
    pub(crate) panes: Vec<PaneEntry>,
}

/// One pane in a [`PaneList`]; an entry without a `pane_id` fails the parse.
#[derive(Debug, Deserialize)]
pub(crate) struct PaneEntry {
    pub(crate) pane_id: String,
    /// The live foreground process's cwd, which can differ from the launch cwd.
    #[serde(default, deserialize_with = "non_empty")]
    pub(crate) foreground_cwd: Option<String>,
    #[serde(default, deserialize_with = "non_empty")]
    label: Option<String>,
}

impl PaneList {
    /// The panes in workspace `ws`.
    pub(crate) fn of(ws: &str) -> Result<Self, HerdrError> {
        answer(&call(&["pane", "list", "--workspace", ws])?)
    }

    /// This snapshot's non-empty pane labels, keyed by pane id.
    fn labels(self) -> HashMap<String, String> {
        self.panes
            .into_iter()
            .filter_map(|pane| pane.label.map(|label| (pane.pane_id, label)))
            .collect()
    }

    /// The entry for pane `pane`, if the snapshot lists it.
    pub(crate) fn pane(&self, pane: &str) -> Option<&PaneEntry> {
        self.panes.iter().find(|entry| entry.pane_id == pane)
    }
}

/// The processes herdr reports in a pane's foreground: the group on unix, one process on Windows.
#[derive(Debug, Deserialize)]
pub(crate) struct ProcessInfo {
    /// Required: an answer without it is a shape failure, never zero processes.
    #[serde(rename = "pane_id")]
    _pane_id: String,
    /// herdr omits an empty list, so an absent key is zero processes.
    #[serde(default)]
    pub(crate) foreground_processes: Vec<Process>,
}

/// One foreground process, identified by its executable, never its rewritable title.
#[derive(Debug, Deserialize)]
pub(crate) struct Process {
    #[serde(default, deserialize_with = "non_empty")]
    pub(crate) argv0: Option<String>,
    #[serde(default)]
    pub(crate) argv: Vec<String>,
}

impl ProcessInfo {
    /// The foreground processes of pane `pane`.
    pub(crate) fn of(pane: &str) -> Result<Self, HerdrError> {
        Self::parse(&call(&["pane", "process-info", "--pane", pane])?)
    }

    fn parse(json: &str) -> Result<Self, HerdrError> {
        #[derive(Deserialize)]
        struct Result {
            process_info: ProcessInfo,
        }
        answer::<Result>(json).map(|result| result.process_info)
    }
}

/// The pane a `herdr plugin pane open` created.
#[derive(Debug, Deserialize)]
pub(crate) struct OpenedPane {
    pub(crate) pane_id: String,
    #[serde(default, deserialize_with = "non_empty")]
    pub(crate) tab_id: Option<String>,
}

/// Where `plugin pane open` puts a pane, with what that placement needs.
#[derive(Debug)]
pub(crate) enum Spot<'a> {
    Split { target: &'a str, direction: crate::config::ToggleDirection },
    Zoomed { target: &'a str },
    Tab { workspace: &'a str },
    Overlay,
}

/// Where and how `plugin pane open` opens a plugin's pane.
#[derive(Debug)]
pub(crate) struct PaneOpen<'a> {
    pub(crate) plugin: &'a str,
    pub(crate) spot: Spot<'a>,
    pub(crate) cwd: &'a str,
    pub(crate) focus: bool,
}

/// Open one of a plugin's panes.
pub(crate) fn open_plugin_pane(open: &PaneOpen) -> Result<OpenedPane, HerdrError> {
    #[derive(Deserialize)]
    struct Result {
        plugin_pane: PluginPane,
    }
    #[derive(Deserialize)]
    struct PluginPane {
        pane: OpenedPane,
    }
    let mut args = vec!["plugin", "pane", "open", "--plugin", open.plugin, "--entrypoint", "pane"];
    match open.spot {
        Spot::Split { target, direction } => {
            args.extend([
                "--placement",
                "split",
                "--target-pane",
                target,
                "--direction",
                direction.as_str(),
            ]);
        }
        Spot::Zoomed { target } => args.extend(["--placement", "zoomed", "--target-pane", target]),
        Spot::Tab { workspace } => args.extend(["--placement", "tab", "--workspace", workspace]),
        Spot::Overlay => args.extend(["--placement", "overlay"]),
    }
    args.extend(["--cwd", open.cwd, if open.focus { "--focus" } else { "--no-focus" }]);
    let opened = answer::<Result>(&call(&args)?)?.plugin_pane.pane;
    if opened.pane_id.is_empty() {
        return Err(HerdrError::Unreadable);
    }
    Ok(opened)
}

/// Close pane `pane` by id with `pane close`, which reaches any pane.
pub(crate) fn close_pane(pane: &str) -> Result<(), HerdrError> {
    call(&["pane", "close", pane]).map(drop)
}

/// Set tab `tab`'s label.
pub(crate) fn rename_tab(tab: &str, label: &str) -> Result<(), HerdrError> {
    call(&["tab", "rename", tab, label]).map(drop)
}

/// How long a startup or exit path waits for herdr; the call itself runs on.
const ANSWER_BOUND: Duration = Duration::from_secs(2);

/// Run a herdr subcommand on its own thread; drop the receiver to fire and forget.
fn herdr_on_thread(args: Vec<String>) -> mpsc::Receiver<Result<String, HerdrError>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = tx.send(call(&refs));
    });
    rx
}

/// The socket and pane id the herdr connection needs; `None` outside herdr.
pub fn connection_target() -> Option<(OsString, String)> {
    Some((var_os("HERDR_SOCKET_PATH")?, var("HERDR_PANE_ID")?))
}

/// This pane's (workspace, pane) ids: the environment's at launch, then what the herdr
/// connection sees, since a move to another tab or workspace changes them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneIds {
    pub workspace: Option<String>,
    pub pane: Option<String>,
}

impl PaneIds {
    /// The ids herdr launched this pane with; none outside herdr.
    pub fn from_env() -> Self {
        Self { workspace: var("HERDR_WORKSPACE_ID"), pane: var("HERDR_PANE_ID") }
    }
}

/// Stamp our pane's `reviewr` label unless the user named it; best effort, never waited on.
pub fn label_pane(ids: &PaneIds) {
    let (Some(ws), Some(pane)) = (ids.workspace.clone(), ids.pane.clone()) else { return };
    thread::spawn(move || {
        // An unreadable listing stamps anyway: the rename fails too, and both log.
        if current_label(&ws, &pane).is_none() {
            let _ = call(&["pane", "rename", &pane, LABEL]);
        }
    });
}

/// Clear our `reviewr` label on exit, waiting at most a bound.
pub fn clear_pane_label(ids: &PaneIds) {
    let (Some(ws), Some(pane)) = (ids.workspace.clone(), ids.pane.clone()) else { return };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if current_label(&ws, &pane).as_deref() == Some(LABEL) {
            let _ = call(&["pane", "rename", &pane, "--clear"]);
        }
        let _ = tx.send(());
    });
    if rx.recv_timeout(ANSWER_BOUND).is_err() {
        logln!("pane label clear unanswered after {ANSWER_BOUND:?}; leaving the label");
    }
}

/// Our pane's label, `None` when unset or unreadable; blocking, so never on the frame loop.
fn current_label(ws: &str, pane: &str) -> Option<String> {
    PaneList::of(ws).ok()?.pane(pane)?.label.clone()
}

/// This plugin's config directory from herdr, `None` when herdr cannot say.
pub fn plugin_config_dir() -> Option<String> {
    let rx = herdr_on_thread(vec!["plugin".into(), "config-dir".into(), plugin_id()]);
    let Ok(answer) = rx.recv_timeout(ANSWER_BOUND) else {
        logln!("plugin config-dir unanswered after {ANSWER_BOUND:?}; no config directory");
        return None;
    };
    let out = answer.ok()?;
    let dir = out.trim();
    (!dir.is_empty()).then(|| dir.to_owned())
}

/// The agents herdr lists: the one `agent list` call.
fn agent_list() -> Result<Vec<AgentPane>, HerdrError> {
    parse_agents(&call(&["agent", "list"])?)
}

/// What `Send` does: one agent sends, several open the picker, none refuses.
pub fn send_target(ids: &PaneIds) -> Result<SendTarget, SendError> {
    let (ws, me) = (ids.workspace.clone(), ids.pane.clone());
    let agents = agent_list()?;
    // Candidates: agents in our workspace other than our pane, in herdr's own order.
    let picked = candidates(&agents, ws.as_deref(), me.as_deref());
    match picked.len() {
        0 => Err(SendError::NoAgent),
        // The sole-agent send shows no row, so only the picker pays for the label calls.
        1 => Ok(SendTarget::One(picked[0].choice(&HashMap::new(), &HashMap::new()))),
        _ => {
            let tabs = tab_labels(ws.as_deref());
            let panes = pane_labels(ws.as_deref());
            Ok(SendTarget::Many(
                picked.into_iter().map(|agent| agent.choice(&tabs, &panes)).collect(),
            ))
        }
    }
}

impl AgentPane {
    /// This pane as a picker row.
    fn choice(
        &self,
        tabs: &HashMap<String, String>,
        panes: &HashMap<String, String>,
    ) -> AgentChoice {
        AgentChoice {
            pane_id: self.pane_id.clone(),
            name: self.row_name(panes.get(&self.pane_id).map(String::as_str)),
            state: self.row_state(),
            tab: tabs.get(&self.tab_id).cloned().unwrap_or_default(),
        }
    }

    /// The agent's name, else its pane label, display agent, kind, or pane id; empty labels fall through.
    fn row_name(&self, pane_label: Option<&str>) -> String {
        [self.name.as_deref(), pane_label, self.display_agent.as_deref(), self.agent.as_deref()]
            .into_iter()
            .flatten()
            .find(|name| !name.is_empty())
            .unwrap_or(&self.pane_id)
            .to_owned()
    }

    /// The state's label from `state_labels`, else herdr's own spelling, never `unknown`.
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

    /// A real agent pane other than ours: the one gate the send and turn tracking share.
    fn is_agent_other_than(&self, me: Option<&str>) -> bool {
        self.agent.is_some() && Some(self.pane_id.as_str()) != me
    }
}

/// Pane id to label for one workspace; best effort, never failing the send.
fn pane_labels(ws: Option<&str>) -> HashMap<String, String> {
    let Some(ws) = ws else { return HashMap::new() };
    PaneList::of(ws).map(PaneList::labels).unwrap_or_default()
}

/// Tab id to label for one workspace; best effort, never failing the send.
fn tab_labels(ws: Option<&str>) -> HashMap<String, String> {
    let Some(ws) = ws else { return HashMap::new() };
    let Ok(json) = call(&["tab", "list", "--workspace", ws]) else {
        return HashMap::new();
    };
    parse_tab_labels(&json).unwrap_or_default()
}

/// `herdr tab list`'s labelled tabs, as tab id → label.
fn parse_tab_labels(json: &str) -> Result<HashMap<String, String>, HerdrError> {
    Ok(answer::<TabList>(json)?
        .tabs
        .into_iter()
        .filter_map(|tab| tab.label.map(|label| (tab.tab_id, label)))
        .collect())
}

#[derive(Debug, Deserialize)]
struct TabList {
    tabs: Vec<TabInfo>,
}

#[derive(Debug, Deserialize)]
struct TabInfo {
    tab_id: String,
    #[serde(default, deserialize_with = "non_empty")]
    label: Option<String>,
}

/// The documented `result.agents` array from `herdr agent list`.
fn parse_agents(json: &str) -> Result<Vec<AgentPane>, HerdrError> {
    answer::<AgentList>(json).map(|list| list.agents)
}

/// One agent as turn tracking sees it; membership is the caller's to decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSample {
    pub cwd: Option<String>,
    pub status: Status,
}

/// A session snapshot's agents, read entry by entry; `None` when there is no list at all.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Sampled {
    /// The real agents other than our own pane, each by its pane.
    pub agents: Vec<(String, AgentSample)>,
    /// The cwd of each entry herdr shaped unexpectedly, which may be an agent.
    pub odd: Vec<Option<String>>,
}

/// Read a session snapshot's agent list; our own pane `me` is left out.
pub(crate) fn samples_of(agents: &serde_json::Value, me: Option<&str>) -> Option<Sampled> {
    let mut readable = Vec::new();
    let mut odd = Vec::new();
    for entry in agents.as_array()? {
        match AgentPane::deserialize(entry) {
            Ok(agent) => readable.push(agent),
            // Our own pane, or a pane with no agent named: no agent, as for a readable entry.
            Err(_) if me.is_some() && entry["pane_id"].as_str() == me => {}
            Err(_)
                if matches!(&entry["agent"], serde_json::Value::Null)
                    || entry["agent"].as_str() == Some("") => {}
            Err(_) => odd.push(entry["cwd"].as_str().filter(|c| !c.is_empty()).map(str::to_string)),
        }
    }
    Some(Sampled { agents: samples_among(readable, me), odd })
}

/// The sampling rule: real agents other than our own pane.
fn samples_among(agents: Vec<AgentPane>, me: Option<&str>) -> Vec<(String, AgentSample)> {
    agents
        .into_iter()
        .filter(|agent| agent.is_agent_other_than(me))
        .map(|agent| {
            (agent.pane_id.clone(), AgentSample { status: agent.status(), cwd: agent.cwd })
        })
        .collect()
}

/// The real agents in workspace `ws`, our own pane `me` excluded.
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

/// Refuse a send to an agent at a prompt, read fresh; herdr offers no atomic send-if-ready.
fn ensure_ready(pane: &str) -> Result<(), SendError> {
    readiness_in(&agent_list()?, pane)
}

/// Only an agent at a prompt refuses, since a prompt drops a paste; one no longer listed is gone.
fn readiness_in(agents: &[AgentPane], pane: &str) -> Result<(), SendError> {
    match agents.iter().find(|agent| agent.pane_id == pane && agent.agent.is_some()) {
        None => {
            logln!("agent pane {pane} is gone");
            Err(HerdrError::PaneGone.into())
        }
        Some(agent) if agent.status() == Status::Blocked => {
            Err(SendError::AtPrompt(agent.row_name(None)))
        }
        Some(_) => Ok(()),
    }
}

/// The largest request a send writes, escaping included: what herdr reads in time on every OS.
const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// How long a send waits for herdr's reply: its 5 s read window plus the answer.
const SEND_BOUND: Duration = Duration::from_secs(5).saturating_add(ANSWER_BOUND);

/// Paste literal text into the agent pane's input, unsubmitted, in one socket request.
pub fn send_text(pane: &str, text: &str) -> Result<(), SendError> {
    let Some(socket) = var_os("HERDR_SOCKET_PATH") else {
        logln!("no HERDR_SOCKET_PATH to send through");
        return Err(HerdrError::Unanswered.into());
    };
    let request = serde_json::json!({
        "id": "reviewr:send",
        "method": "pane.send_text",
        "params": {"pane_id": pane, "text": pasted(text)},
    })
    .to_string();
    if request.len() > MAX_REQUEST_BYTES {
        return Err(SendError::TooLarge);
    }
    ensure_ready(pane)?;
    Ok(socket_call(socket, request)?)
}

/// One request line answered by one reply line, on its own thread, bounded by [`SEND_BOUND`].
fn socket_call(socket: OsString, request: String) -> Result<(), HerdrError> {
    reply_outcome(&socket_exchange(socket, request, SEND_BOUND)?)
}

/// One request answered by one reply, read as `T`; herdr's error code becomes `Refused`.
pub(crate) fn socket_request<T: serde::de::DeserializeOwned>(
    socket: OsString,
    method: &str,
    bound: Duration,
) -> Result<T, HerdrError> {
    let request = serde_json::json!({ "id": method, "method": method, "params": {} });
    let reply = socket_exchange(socket, request.to_string(), bound)?;
    if let Some(code) = error_code(&reply) {
        return Err(HerdrError::refused(Some(code)));
    }
    answer(&reply)
}

/// Write `request` and read herdr's one reply line, on its own thread, within `bound`: a pipe's
/// read timeout may be unsupported, so the thread is what bounds it.
pub(crate) fn socket_exchange(
    socket: OsString,
    request: String,
    bound: Duration,
) -> Result<String, HerdrError> {
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + bound;
    thread::spawn(move || {
        let _ = tx.send(socket::exchange(&socket, &request, deadline));
    });
    match rx.recv_timeout(bound) {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(error)) => {
            logln!("herdr socket call failed: {error}");
            Err(HerdrError::Unanswered)
        }
        Err(_) => {
            logln!("herdr socket call unanswered after {bound:?}");
            Err(HerdrError::Unanswered)
        }
    }
}

/// A `result` reply is success; an error envelope is a refusal carrying its code.
fn reply_outcome(reply: &str) -> Result<(), HerdrError> {
    if let Some(code) = error_code(reply) {
        logln!("herdr refused over the socket: {}", reply.trim());
        return Err(HerdrError::refused(Some(code)));
    }
    answer::<serde::de::IgnoredAny>(reply).map(drop)
}

/// The socket transport: a Unix socket, or a named pipe on Windows; one request per connection.
pub(crate) mod socket {
    use std::ffi::OsStr;
    use std::io::{self, BufRead, BufReader, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// Write `request` as one line and read the one line herdr answers.
    pub(super) fn exchange(socket: &OsStr, request: &str, deadline: Instant) -> io::Result<String> {
        let mut stream = connect(socket, deadline)?;
        stream.write_all(request.as_bytes())?;
        stream.write_all(b"\n")?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        if !reply.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "herdr closed the connection without answering",
            ));
        }
        Ok(reply)
    }

    /// The time left before `deadline`, or a timeout once none is.
    fn left(deadline: Instant) -> io::Result<Duration> {
        Some(deadline.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "herdr did not answer in time"))
    }

    /// One `events.subscribe` connection: a reader thread forwards its lines, the acknowledgment
    /// first; dropping it unblocks that reader and closes the connection.
    pub(crate) struct Subscription {
        stream: Arc<Stream>,
        /// Set before unblocking, so a reader between reads stops instead of reading again.
        closed: Arc<AtomicBool>,
        reader: std::thread::JoinHandle<()>,
    }

    /// What a subscription's reader forwards.
    #[derive(Debug)]
    pub(crate) enum Line {
        /// One line, without its newline.
        Text(Vec<u8>),
        /// The connection closed or failed; nothing follows.
        Closed(String),
    }

    #[cfg(unix)]
    type Stream = std::os::unix::net::UnixStream;
    #[cfg(windows)]
    type Stream = interprocess::local_socket::Stream;

    impl Subscription {
        /// Connect, write `request`, and hand every line read to `sink`, which says whether anyone
        /// still listens.
        pub(crate) fn open(
            socket: &OsStr,
            request: &str,
            deadline: Instant,
            sink: impl Fn(Line) -> bool + Send + 'static,
        ) -> io::Result<Self> {
            let mut stream = connect(socket, deadline)?;
            stream.write_all(request.as_bytes())?;
            stream.write_all(b"\n")?;
            #[cfg(unix)]
            stream.set_read_timeout(None)?;
            let stream = Arc::new(stream);
            let closed = Arc::new(AtomicBool::new(false));
            let (theirs, their_closed) = (Arc::clone(&stream), Arc::clone(&closed));
            let reader = std::thread::Builder::new()
                .name("herdr-subscription".into())
                .spawn(move || forward(&theirs, &their_closed, &sink))?;
            Ok(Self { stream, closed, reader })
        }
    }

    impl Drop for Subscription {
        /// Unblock the reader until it returns: a cancel lands only on a read already pending,
        /// so it repeats, bounded, for a reader that was between reads.
        fn drop(&mut self) {
            self.closed.store(true, Ordering::SeqCst);
            for _ in 0..1000 {
                #[cfg(unix)]
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
                #[cfg(windows)]
                cancel(&self.stream);
                if self.reader.is_finished() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            crate::logln!("herdr subscription reader did not stop");
        }
    }

    /// Read `stream` until it closes or `closed` is set, forwarding each complete line.
    fn forward(stream: &Stream, closed: &AtomicBool, sink: &impl Fn(Line) -> bool) {
        use std::io::Read;
        let mut pending = Vec::new();
        let mut chunk = [0_u8; 8192];
        let ended = loop {
            if closed.load(Ordering::SeqCst) {
                return;
            }
            match (&*stream).read(&mut chunk) {
                Ok(0) => break "herdr closed the subscription".to_owned(),
                Ok(n) => pending.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break e.to_string(),
            }
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = pending.drain(..=end).collect();
                line.pop();
                if !sink(Line::Text(line)) {
                    return;
                }
            }
        };
        if !closed.load(Ordering::SeqCst) {
            sink(Line::Closed(ended));
        }
    }

    /// The handle the stream's reads go through: a duplicate would not cancel them.
    #[cfg(windows)]
    fn raw_handle(stream: &Stream) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        let Stream::NamedPipe(pipe) = stream;
        pipe.as_handle().as_raw_handle()
    }

    /// Cancel the reads pending on `stream`'s own handle, so the reader thread returns.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn cancel(stream: &Stream) {
        use windows_sys::Win32::System::IO::CancelIoEx;
        // SAFETY: the handle stays open while the subscription holds its stream.
        unsafe { CancelIoEx(raw_handle(stream).cast(), std::ptr::null()) };
    }

    /// Timeouts set once at connect: macOS fails `setsockopt` once herdr has closed.
    #[cfg(unix)]
    fn connect(socket: &OsStr, deadline: Instant) -> io::Result<std::os::unix::net::UnixStream> {
        let stream = std::os::unix::net::UnixStream::connect(socket)?;
        let left = left(deadline)?;
        stream.set_read_timeout(Some(left))?;
        stream.set_write_timeout(Some(left))?;
        Ok(stream)
    }

    /// Wait for a free pipe instance only until the deadline, as herdr's own client does.
    #[cfg(windows)]
    fn connect(
        socket: &OsStr,
        deadline: Instant,
    ) -> io::Result<interprocess::local_socket::Stream> {
        use interprocess::ConnectWaitMode;
        use interprocess::local_socket::{ConnectOptions, GenericNamespaced, prelude::*};
        let name = socket.to_string_lossy().into_owned().to_ns_name::<GenericNamespaced>()?;
        ConnectOptions::new()
            .name(name)
            .wait_mode(ConnectWaitMode::Timeout(left(deadline)?))
            .connect_sync()
    }
}

/// The bracketed-paste markers.
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// The batch as one bracketed paste, line breaks as herdr's own paste encodes them on this OS,
/// every inner terminator stripped in one pass.
fn pasted(text: &str) -> String {
    let text: std::borrow::Cow<'_, str> =
        if cfg!(windows) { crate::text::crlf_line_breaks(text).into() } else { text.into() };
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
pub fn focus(pane: &str) -> Result<(), HerdrError> {
    call(&["agent", "focus", pane]).map(drop)
}

#[cfg(test)]
mod tests {
    use super::{
        AgentChoice, AgentPane, HashMap, HerdrError, Status, parse_agents, parse_tab_labels,
    };

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

    /// A non-agent pane as herdr lists it: `agent_status: unknown`, no `agent`.
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
        panes: &HashMap<String, String>,
    ) -> Vec<AgentChoice> {
        super::candidates(agents, ws, me)
            .into_iter()
            .map(|agent| agent.choice(tabs, panes))
            .collect()
    }

    #[test]
    fn a_send_refuses_only_an_agent_at_a_prompt() {
        let at = |status: &str| AgentPane {
            agent_status: status.into(),
            state_labels: Some(HashMap::from([("compacting".into(), "Compacting".into())])),
            ..agent("w8:p1", "w8:t1", "w8")
        };
        // A working agent takes typing mid-turn: the paste waits in its input.
        for status in ["idle", "done", "working", "unknown", "compacting"] {
            assert_eq!(super::readiness_in(&[at(status)], "w8:p1"), Ok(()), "{status}");
        }
        // A prompt drops a paste.
        let blocked = super::readiness_in(&[at("blocked")], "w8:p1");
        assert_eq!(blocked, Err(super::SendError::AtPrompt("claude".into())));
        let gone = Err(super::SendError::Herdr(HerdrError::PaneGone));
        assert_eq!(super::readiness_in(&[at("idle")], "w8:p9"), gone);
        // A pane whose agent exited is listed without one: the send goes nowhere near it.
        let shell = AgentPane { agent: None, ..at("idle") };
        assert_eq!(super::readiness_in(&[shell], "w8:p1"), gone);
    }

    #[test]
    fn an_agent_entry_herdr_shaped_unexpectedly_is_set_aside_by_its_cwd() {
        let list = serde_json::json!([
            {"pane_id": "w1:p2", "tab_id": "w1:t1", "workspace_id": "w1",
             "agent_status": "working", "agent": "claude", "cwd": "/w"},
            {"pane_id": "w2:p1", "agent": "codex", "tab_id": null, "cwd": "/elsewhere"},
            {"pane_id": "w2:p2", "agent": "codex"},
            {"pane_id": "w2:p3", "tab_id": null, "cwd": "/w"},
            {"pane_id": "w2:p4", "agent": {"name": "codex"}, "cwd": "/reshaped"},
            {"pane_id": "w1:p9", "agent": "claude", "tab_id": null},
        ]);
        let sampled = super::samples_of(&list, Some("w1:p9")).expect("a list");
        assert_eq!(sampled.agents.len(), 1, "the readable agent still counts");
        let odd = [Some("/elsewhere".to_string()), None, Some("/reshaped".to_string())];
        assert_eq!(
            sampled.odd, odd,
            "a shell pane and our own pane are no agents, a reshaped one is"
        );
        assert_eq!(super::samples_of(&serde_json::Value::Null, None), None, "no list is no answer");
    }

    #[test]
    fn sampling_keeps_every_tab_and_workspace() {
        // Membership rides the agent's cwd, never its pane's tab or workspace.
        let agents = vec![
            AgentPane { cwd: Some("/w/one".into()), ..agent("w8:p1", "w8:t1", "w8") },
            AgentPane { cwd: Some("/w/two".into()), ..agent("w8:p2", "w8:t2", "w8") },
            AgentPane { cwd: Some("/w/three".into()), ..agent("w9:p1", "w9:t1", "w9") },
        ];
        let cwds: Vec<_> =
            super::samples_among(agents, None).into_iter().filter_map(|(_, s)| s.cwd).collect();
        assert_eq!(cwds, ["/w/one", "/w/two", "/w/three"]);
    }

    #[test]
    fn sampling_drops_our_own_pane_and_every_non_agent_pane() {
        let agents = vec![
            AgentPane { cwd: Some("/w/real".into()), ..agent("w3:p1", "w3:t1", "w3") },
            AgentPane { cwd: Some("/w/shell".into()), ..non_agent_pane("w3:p4", "w3:t1", "w3") },
            AgentPane { cwd: Some("/w/self".into()), ..agent("w3:p5", "w3:t1", "w3") },
        ];
        let samples = super::samples_among(agents, Some("w3:p5"));
        let panes: Vec<_> =
            samples.iter().map(|(pane, s)| (pane.as_str(), s.cwd.as_deref())).collect();
        assert_eq!(panes, [("w3:p1", Some("/w/real"))]);
    }

    #[test]
    fn a_sample_carries_the_status_tracking_folds() {
        let agents = vec![AgentPane {
            agent_status: "blocked".into(),
            cwd: Some("/w/one".into()),
            ..agent("w8:p1", "w8:t1", "w8")
        }];
        let samples = super::samples_among(agents, None);
        assert_eq!(samples[0].1.status, Status::Blocked);
    }

    /// One agent carrying the picker-facing fields herdr omits until something sets them.
    fn named(pane: &str, tab: &str, ws: &str, name: Option<&str>) -> AgentPane {
        AgentPane { name: name.map(str::to_string), ..agent(pane, tab, ws) }
    }

    #[test]
    fn a_row_name_prefers_the_rename_then_the_pane_label_then_the_display_agent_then_the_kind() {
        let renamed = named("w8:p1", "w8:t1", "w8", Some("release-bot"));
        // `herdr agent rename` sets `name`, which wins.
        assert_eq!(renamed.row_name(Some("wiki-revision")), "release-bot");
        let displayed =
            AgentPane { display_agent: Some("Claude".into()), ..agent("w8:p1", "w8:t1", "w8") };
        // A pane label outranks the display agent, since the border shows it.
        assert_eq!(displayed.row_name(Some("wiki-revision")), "wiki-revision");
        assert_eq!(displayed.row_name(None), "Claude");
        // An empty pane label falls through like an absent one.
        assert_eq!(displayed.row_name(Some("")), "Claude");
        let cleared = named("w8:p1", "w8:t1", "w8", None);
        // `--clear` leaves the key present and null, which falls through like an absent one.
        assert_eq!(cleared.row_name(None), "claude");
        // An empty pane label still lets the agent kind name the row.
        assert_eq!(cleared.row_name(Some("")), "claude");
        let anonymous = AgentPane { agent: None, ..agent("w8:p1", "w8:t1", "w8") };
        // With no kind either, the pane id keeps the row and the success line from going blank.
        assert_eq!(anonymous.row_name(None), "w8:p1");
        // An empty pane label must not hide the pane id fallback.
        assert_eq!(anonymous.row_name(Some("")), "w8:p1");
        // An empty name parses as no name, so it falls through too.
        let emptied = r#"{"result":{"agents":[{"agent":"codex","agent_status":"idle","pane_id":"w8:p2","tab_id":"w8:t1","workspace_id":"w8","name":"","display_agent":""}]}}"#;
        let parsed = super::parse_agents(emptied).unwrap();
        assert_eq!(parsed[0].row_name(None), "codex");
        // A pane label identifies the pane even when herdr sends empty agent names.
        assert_eq!(parsed[0].row_name(Some("wiki-revision")), "wiki-revision");
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
    fn picker_rows_are_every_workspace_agent_in_herdr_order_with_its_pane_and_tab_labels() {
        let agents = vec![
            agent("w8:p1", "w8:t1", "w8"),
            non_agent_pane("w8:p4", "w8:t1", "w8"),
            named("w8:p2", "w8:t2", "w8", Some("release-bot")),
            agent("w9:p1", "w9:t1", "w9"),
        ];
        let mut tabs = HashMap::new();
        tabs.insert("w8:t1".to_string(), "Grip Outreach".to_string());
        // w8:t2 has no label, so that row shows its state alone.
        let panes = HashMap::from([("w8:p1".into(), "wiki-revision".into())]);
        let rows = rows(&agents, Some("w8"), Some("w8:p9"), &tabs, &panes);
        assert_eq!(
            rows,
            vec![
                AgentChoice {
                    pane_id: "w8:p1".into(),
                    name: "wiki-revision".into(),
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
        let rows_of = |ws| rows(&agents, ws, Some("w3:p5"), &HashMap::new(), &HashMap::new());
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
        assert_eq!(parsed[0].row_name(None), "claude");
        assert_eq!(parsed[0].row_state(), "idle");
        // And with `name` explicitly null, as `herdr agent rename --clear` leaves it.
        let cleared = r#"{"result":{"agents":[{"agent":"codex","agent_status":"idle","pane_id":"w8:p2","tab_id":"w8:t1","workspace_id":"w8","name":null}]}}"#;
        assert_eq!(parse_agents(cleared).unwrap()[0].row_name(None), "codex");
    }

    /// A herdr that accepts and never answers holds the exchange only until its deadline.
    #[cfg(unix)]
    #[test]
    fn an_unanswered_exchange_ends_at_its_deadline() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (release, released) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let held = listener.accept();
            let _ = released.recv();
            drop(held);
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let deadline = Instant::now() + Duration::from_millis(200);
        std::thread::spawn(move || {
            let _ = tx.send(super::socket::exchange(path.as_os_str(), "{}", deadline));
        });
        let outcome = rx.recv_timeout(Duration::from_secs(5)).expect("the exchange ends");
        let kind = outcome.expect_err("no reply came").kind();
        assert!(matches!(kind, std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
        drop(release);
    }

    #[test]
    fn only_a_result_reply_is_a_delivered_send() {
        let ok = "{\"id\":\"reviewr:send\",\"result\":{\"type\":\"ok\"}}\n";
        assert_eq!(super::reply_outcome(ok), Ok(()));
        // herdr's error reply echoes the id and carries the same envelope as a failed CLI call.
        let gone = r#"{"id":"reviewr:send","error":{"code":"pane_not_found","message":"pane w8:p1 not found"}}"#;
        assert_eq!(super::reply_outcome(gone), Err(HerdrError::PaneGone));
        assert_eq!(super::reply_outcome(r#"{"id":"reviewr:send"}"#), Err(HerdrError::Unreadable));
    }

    #[test]
    fn a_send_is_one_bracketed_paste_no_embedded_terminator_can_end() {
        // Issue #41's repro string: sent raw, vim ate the leading `b` and `i`.
        let note = "bit/DESIGN.md:95 note";
        assert_eq!(super::pasted(note), "\x1b[200~bit/DESIGN.md:95 note\x1b[201~");
        // A snippet can carry the terminator, even spliced across a removal.
        assert_eq!(super::pasted("a\x1b[201~b"), "\x1b[200~ab\x1b[201~");
        assert_eq!(super::pasted("a\x1b[201\x1b[201~~b"), "\x1b[200~ab\x1b[201~");
    }

    #[test]
    fn a_pane_label_reads_only_our_pane_and_absent_or_empty_is_none() {
        // `label` appears only on labelled panes.
        let json = r#"{"result":{"panes":[{"pane_id":"w1:p1","label":"build"},{"pane_id":"w1:p2"},{"pane_id":"w1:p3","label":""}]}}"#;
        let list: super::PaneList = super::answer(json).unwrap();
        let label = |pane: &str| list.pane(pane).and_then(|p| p.label.as_deref());
        assert_eq!(label("w1:p1"), Some("build"));
        assert_eq!(label("w1:p2"), None);
        assert_eq!(label("w1:p3"), None, "empty label reads as none");
        assert_eq!(label("w9:p9"), None, "unknown pane reads as none");
    }

    #[test]
    fn a_herdr_answer_missing_its_shape_is_unreadable_never_empty() {
        // An error envelope with exit 0 is a shape failure, never an empty listing.
        let envelope = r#"{"error":{"code":"internal","message":"boom"},"id":"cli:request"}"#;
        assert_eq!(super::answer::<super::PaneList>(envelope).err(), Some(HerdrError::Unreadable));
        assert_eq!(super::ProcessInfo::parse(envelope).err(), Some(HerdrError::Unreadable));
        // herdr skips an empty `foreground_processes`: zero processes, a real answer.
        let bare = r#"{"result":{"process_info":{"pane_id":"w1:p1","shell_pid":7}}}"#;
        assert_eq!(super::ProcessInfo::parse(bare).unwrap().foreground_processes.len(), 0);
    }

    #[test]
    fn a_failed_call_carries_herdrs_error_code() {
        // The code tells a pane that exited mid-sweep from a herdr that failed.
        let gone = r#"{"error":{"code":"pane_not_found","message":"pane w1:p3 not found"},"id":"cli:request"}"#;
        assert_eq!(super::error_code(gone).as_deref(), Some("pane_not_found"));
        assert_eq!(HerdrError::refused(super::error_code(gone)), HerdrError::PaneGone);
        // An advisory line before the envelope does not hide it.
        let noisy = format!("warning: something\n{gone}\n");
        assert_eq!(super::error_code(&noisy).as_deref(), Some("pane_not_found"));
        let internal = r#"{"error":{"code":"internal","message":"boom"}}"#;
        assert_eq!(
            HerdrError::refused(super::error_code(internal)),
            HerdrError::Refused(Some("internal".into()))
        );
        assert_eq!(super::error_code("plain words"), None);
    }

    #[test]
    fn a_pane_list_parses_to_labels_and_unlabelled_or_empty_panes_are_dropped() {
        let json = r#"{"result":{"panes":[{"pane_id":"w7:p14","label":"s19-wiki-revision"},{"pane_id":"w7:p15"},{"pane_id":"w7:p16","label":""},{"pane_id":"w7:p17","label":null}]}}"#;
        let panes = super::answer::<super::PaneList>(json).unwrap();
        let labels = panes.labels();
        assert_eq!(labels, HashMap::from([("w7:p14".into(), "s19-wiki-revision".into())]));
        assert!(super::answer::<super::PaneList>("[]").is_err());
        assert!(super::answer::<super::PaneList>("not json").is_err());
        assert!(
            super::answer::<super::PaneList>(r#"{"result":{"panes":[{"label":"missing id"}]}}"#)
                .is_err()
        );
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
        // `cwd` from the wire: worktree membership rides on it.
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
        // The spelling is the `state_labels` key.
        let labelled = r#"{"result":{"agents":[{"agent":"claude","agent_status":"compacting","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","state_labels":{"compacting":"Compacting"}}]}}"#;
        assert_eq!(parse_agents(labelled).unwrap()[0].row_state(), "Compacting");
    }
}
