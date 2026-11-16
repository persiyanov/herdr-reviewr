//! The herdr connection: this pane's visibility and ids, and turn tracking from the member agents'
//! statuses, pushed over herdr's socket (a named pipe on Windows) instead of polled.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::herdr::socket::{Line, Subscription};
use crate::herdr::{AgentSample, PaneIds};
use crate::logln;
use crate::schedule::Backoff;
use crate::turn::{Status, TurnFeed, TurnHost, TurnNews, TurnReport, Wrote};

/// The first herdr whose socket has everything this connection reads: `session.snapshot`,
/// subscriptions that start live, `events_lost`, and the move events.
pub const MIN_VERSION: (u32, u32, u32) = (0, 9, 3);
/// How long one request, or a subscription's acknowledgment, may take to answer.
const ANSWER: Duration = Duration::from_secs(5);
/// A connection that stayed up this long was healthy: its loss starts the backoff over.
const HEALTHY: Duration = Duration::from_mins(1);
/// How many times in a row a subscribe follows the members moving under it before it settles
/// for the next session event.
const FOLLOWS: u32 = 3;
/// The events that can change what reviewr reads from the session. Every one triggers a
/// fresh snapshot.
const EVENTS: &[&str] = &[
    "tab.focused",
    "workspace.focused",
    "tab.moved",
    "pane.moved",
    "workspace.moved",
    "pane.created",
    "pane.closed",
    "pane.agent_detected",
];

/// What reviewr reads from the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// This pane's current ids, `None` when the session no longer lists its terminal.
    pub me: Option<PaneIds>,
    /// Whether this pane's tab is the active tab of the focused workspace.
    pub visible: bool,
}

/// What the connection reports.
#[derive(Debug)]
pub enum HerdrEvent {
    Session(Session),
    /// Turn tracking moved: a turn ended, the baseline changed, or the agents present did.
    Turn(TurnReport),
    /// The connection dropped or could not open; it retries on a backoff. Until it is back,
    /// reviewr assumes it is visible, and the outage is a gap in turn tracking.
    Lost,
    /// herdr is older than [`MIN_VERSION`]: reported once, never retried. Carries its version.
    TooOld(String),
}

/// The handle. Dropping it stops the thread.
#[derive(Debug)]
pub struct Connection {
    tx: mpsc::Sender<Msg>,
}

/// Everything the connection's thread hears, on one queue: a status edge and the write after it
/// are judged in the order they arrived.
enum Msg {
    /// A line of the subscription tagged so, and when it arrived.
    Line(u64, Line, Instant),
    News(TurnNews),
    Stop,
}

impl Connection {
    /// Connect to the herdr socket at `socket` as the pane `pane_id`, tracking the turns of the
    /// agents working in `repo`, and report to `tx`.
    pub fn start(
        socket: OsString,
        pane_id: String,
        repo: PathBuf,
        tx: crate::wake::Sender<HerdrEvent>,
    ) -> std::io::Result<Self> {
        let (me, rx) = mpsc::channel();
        let queue = me.clone();
        // Turn snapshots run git on `repo`, even after a switch let go of it.
        let session = crate::git::lease_session(&repo);
        std::thread::Builder::new().name("herdr".into()).spawn(move || {
            let _session = session;
            let mut thread = Thread {
                socket,
                pane_id,
                terminal: None,
                host: TurnHost::open(repo),
                agents: HashMap::new(),
                members: Vec::new(),
                subscribed: Vec::new(),
                membership_retry: None,
                tx,
                rx,
                me: queue,
                tag: 0,
            };
            thread.run();
        })?;
        Ok(Self { tx: me })
    }

    /// The handle the watcher feeds turn tracking through.
    pub fn feed(&self) -> TurnFeed {
        let tx = self.tx.clone();
        TurnFeed::new(move |news| {
            let _ = tx.send(Msg::News(news));
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// One agent pane as sampled.
#[derive(Clone, Debug)]
struct Agent {
    sample: AgentSample,
    /// Its status came from a live event, which no snapshot read after it can be newer than.
    heard: bool,
}

/// A subscription and the tag its reader stamps on every line.
struct Live {
    /// Held only: dropping it closes the connection.
    _subscription: Subscription,
    tag: u64,
}

/// The connection thread's state.
struct Thread {
    socket: OsString,
    pane_id: String,
    /// The terminal this pane runs, which survives a move; found in the first snapshot.
    terminal: Option<String>,
    host: TurnHost,
    /// Every agent pane but this one; a status heard on the live subscription beats a snapshot.
    agents: HashMap<String, Agent>,
    /// The member panes, sorted.
    members: Vec<String>,
    /// The member panes the live subscription watches.
    subscribed: Vec<String>,
    /// When to resolve the members again, after a member's directory would not resolve.
    membership_retry: Option<(Instant, Backoff)>,
    /// Where reports go: the frame loop.
    tx: crate::wake::Sender<HerdrEvent>,
    rx: mpsc::Receiver<Msg>,
    /// Where subscriptions' readers queue their lines.
    me: mpsc::Sender<Msg>,
    tag: u64,
}

enum Outcome {
    Lost(String),
    /// herdr refused a subscribe, naming why: a named pane may have just closed.
    Refused(String),
    TooOld(String),
    /// Stopped, or the loop's receiver is gone: reviewr is exiting.
    Gone,
}

impl Thread {
    /// Connect, and after each loss back off and connect again; a panic in a cycle is a loss too,
    /// so turn tracking outlives it.
    fn run(&mut self) {
        let mut backoff = Backoff::default();
        let mut wait = false;
        loop {
            let mut live = None;
            let cycle = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.cycle(&mut backoff, wait, &mut live)
            }));
            match cycle {
                Ok(true) => {}
                Ok(false) => return,
                Err(_) => {
                    self.lose("the connection thread crashed");
                    if self.tx.send(HerdrEvent::Lost).is_err() {
                        return;
                    }
                }
            }
            // A connection that stayed live a while starts the backoff over.
            if live.is_some_and(|since: Instant| since.elapsed() >= HEALTHY) {
                backoff = Backoff::default();
            }
            wait = true;
        }
    }

    /// One cycle: back off (after a loss), connect, and follow until the connection is lost.
    /// `false` when the thread ends.
    fn cycle(&mut self, backoff: &mut Backoff, wait: bool, live: &mut Option<Instant>) -> bool {
        if wait {
            let until = Instant::now() + backoff.next_wait();
            loop {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match self.rx.recv_timeout(left) {
                    Ok(msg) => {
                        if self.handle_outside_lines(msg) {
                            return false;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return false,
                }
            }
        }
        match self.connected(live) {
            Outcome::TooOld(version) => {
                let _ = self.tx.send(HerdrEvent::TooOld(version));
                return false;
            }
            Outcome::Gone => return false,
            Outcome::Lost(reason) | Outcome::Refused(reason) => {
                self.lose(&reason);
                if self.tx.send(HerdrEvent::Lost).is_err() {
                    return false;
                }
            }
        }
        true
    }

    /// Apply a message that is not a live subscription's line; whether the thread should stop.
    fn handle_outside_lines(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::News(news) => self.news(news),
            Msg::Stop => return true,
            Msg::Line(..) => {}
        }
        false
    }

    fn news(&mut self, news: TurnNews) {
        match news {
            TurnNews::Wrote(wrote) => {
                if self.host.on_worktree_change(&wrote) {
                    self.report_turn(self.host.status());
                }
            }
            TurnNews::WatcherDown(down) => self.host.watcher_down(down),
        }
    }

    /// The connection is lost: nobody saw the agents meanwhile, so the next status cannot start
    /// a turn.
    fn lose(&mut self, reason: &str) {
        logln!("herdr connection lost: {reason}");
        self.host.gap();
        self.agents.clear();
        self.subscribed.clear();
    }

    /// One connected stretch: check the version, subscribe, report the session, then follow
    /// events until something ends it. `live` is when it subscribed.
    fn connected(&mut self, live: &mut Option<Instant>) -> Outcome {
        let pong = match self.request("ping") {
            Ok(pong) => pong,
            Err(e) => return Outcome::Lost(e),
        };
        let version = pong["version"].as_str().unwrap_or_default().to_string();
        let (major, minor, patch) = MIN_VERSION;
        let min = semver::Version::new(major.into(), minor.into(), patch.into());
        if parse_version(&version).is_none_or(|v| v < min) {
            return Outcome::TooOld(version);
        }
        let mut current = match self.resubscribe(Vec::new()) {
            Ok(current) => current,
            Err(outcome) => return outcome,
        };
        *live = Some(Instant::now());
        logln!("herdr {version} connected");
        loop {
            let deadline = self.membership_retry.map(|(at, _)| at);
            let first = match deadline {
                Some(at) => {
                    match self.rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(msg) => Some(msg),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return Outcome::Gone,
                    }
                }
                None => match self.rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return Outcome::Gone,
                },
            };
            // A retry deadline that passed asks for the session again.
            let mut session_changed = first.is_none();
            // What arrived together folds in order, and its writes are checked once.
            let mut burst: Vec<Msg> = first.into_iter().collect();
            burst.extend(std::iter::from_fn(|| self.rx.try_recv().ok()));
            let mut writes: Option<Wrote> = None;
            for msg in burst {
                if let Msg::News(TurnNews::Wrote(wrote)) = msg {
                    match writes.as_mut() {
                        Some(writes) => writes.absorb(wrote),
                        None => writes = Some(wrote),
                    }
                    continue;
                }
                if let Some(writes) = writes.take() {
                    self.news(TurnNews::Wrote(writes));
                }
                match msg {
                    Msg::Line(from, text, at) if from == current.tag => {
                        match self.fold_line(text, at) {
                            Ok(changed) => session_changed |= changed,
                            Err(outcome) => return outcome,
                        }
                    }
                    other => {
                        if self.handle_outside_lines(other) {
                            return Outcome::Gone;
                        }
                    }
                }
            }
            if let Some(writes) = writes {
                self.news(TurnNews::Wrote(writes));
            }
            if session_changed {
                if let Err(outcome) = self.read_and_report() {
                    return outcome;
                }
                if self.members != self.subscribed {
                    match self.resubscribe(vec![current]) {
                        Ok(next) => current = next,
                        Err(outcome) => return outcome,
                    }
                }
            }
        }
    }

    /// Fold one line of a live subscription, arrived `at`. `Ok(true)` when the session must be
    /// read again.
    fn fold_line(&mut self, line: Line, at: Instant) -> Result<bool, Outcome> {
        let line = match line {
            Line::Text(line) => line,
            Line::Closed(reason) => return Err(Outcome::Lost(reason)),
        };
        let Ok(value) = serde_json::from_slice::<Value>(&line) else { return Ok(false) };
        match value["error"]["code"].as_str() {
            // herdr dropped events: nobody saw what they held, and the session is read again.
            Some("events_lost") => {
                logln!("herdr lost events: a gap in turn tracking");
                self.host.gap();
                return Ok(true);
            }
            Some(code) => return Err(Outcome::Lost(format!("subscription error: {code}"))),
            None => {}
        }
        if value["event"] == "pane.agent_status_changed" {
            let data = &value["data"];
            if let (Some(pane), Some(status)) =
                (data["pane_id"].as_str(), data["agent_status"].as_str())
            {
                self.observe_status(pane, status, at);
            }
            return Ok(false);
        }
        Ok(value.get("event").is_some())
    }

    /// Subscribe to the session events and the members' statuses; `old` closes only once the new
    /// one is live, and a subscribe refused for a just-closed pane re-reads and tries again.
    fn resubscribe(&mut self, mut old: Vec<Live>) -> Result<Live, Outcome> {
        let mut follows = 0;
        if old.is_empty() {
            self.read_and_report()?;
        }
        loop {
            let watched = self.members.clone();
            let mut subscriptions: Vec<Value> =
                EVENTS.iter().map(|e| json!({ "type": e })).collect();
            subscriptions.extend(
                watched
                    .iter()
                    .map(|pane| json!({ "type": "pane.agent_status_changed", "pane_id": pane })),
            );
            match self.subscribe(&subscriptions, &old) {
                Ok(live) => {
                    old.clear();
                    self.subscribed = watched;
                    for agent in self.agents.values_mut() {
                        agent.heard = false;
                    }
                    self.read_and_report()?;
                    if self.members == self.subscribed || follows == FOLLOWS {
                        return Ok(live);
                    }
                    // The members moved while subscribing: subscribe again, this one still live.
                    follows += 1;
                    old.push(live);
                }
                // Each refusal follows members that moved, within the same cap.
                Err(Outcome::Refused(code)) if follows < FOLLOWS => {
                    follows += 1;
                    logln!("herdr subscribe refused ({code}); reading the session again");
                    self.read_and_report()?;
                }
                Err(Outcome::Refused(code)) => {
                    return Err(Outcome::Lost(format!("subscribe refused: {code}")));
                }
                Err(outcome) => return Err(outcome),
            }
        }
    }

    /// Open one subscription and wait for its acknowledgment, folding in order what `old`
    /// delivers and what else arrives meanwhile. Its later lines stay queued for the caller.
    fn subscribe(&mut self, subscriptions: &[Value], old: &[Live]) -> Result<Live, Outcome> {
        let body = json!({
            "id": "subscribe",
            "method": "events.subscribe",
            "params": { "subscriptions": subscriptions },
        });
        self.tag += 1;
        let tag = self.tag;
        let deadline = Instant::now() + ANSWER;
        let queue = self.me.clone();
        let sink = move |line| queue.send(Msg::Line(tag, line, Instant::now())).is_ok();
        let subscription = Subscription::open(&self.socket, &body.to_string(), deadline, sink)
            .map_err(|e| Outcome::Lost(e.to_string()))?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(msg) = self.rx.recv_timeout(left) else {
                return Err(Outcome::Lost("subscribe: herdr did not answer".into()));
            };
            match msg {
                Msg::Line(from, line, _) if from == tag => {
                    let Line::Text(ack) = line else {
                        return Err(Outcome::Lost("subscribe: herdr closed the connection".into()));
                    };
                    let ack: Value = serde_json::from_slice(&ack)
                        .map_err(|e| Outcome::Lost(format!("subscribe: {e}")))?;
                    if let Some(code) = ack["error"]["code"].as_str() {
                        return Err(Outcome::Refused(code.to_string()));
                    }
                    return Ok(Live { _subscription: subscription, tag });
                }
                // A session event an old one carries is read in the snapshot that follows.
                Msg::Line(from, line, at) if old.iter().any(|l| l.tag == from) => {
                    self.fold_line(line, at)?;
                }
                other => {
                    if self.handle_outside_lines(other) {
                        return Err(Outcome::Gone);
                    }
                }
            }
        }
    }

    /// Read the session, report it, and fold the agents' statuses. A member keeps the status an
    /// event on the live subscription gave it, which no snapshot read after it can be newer than.
    fn read_and_report(&mut self) -> Result<(), Outcome> {
        let result = self.request("session.snapshot").map_err(Outcome::Lost)?;
        let (session, agents) =
            read_session(&result["snapshot"], &self.pane_id, &mut self.terminal);
        let workspace = session.me.as_ref().and_then(|me| me.workspace.clone());
        self.tx.send(HerdrEvent::Session(session)).map_err(|_| Outcome::Gone)?;
        // No list, or an entry herdr shaped unexpectedly that may be a member: hold, and read again.
        let Some(sampled) = agents else {
            self.hold_membership();
            return Ok(());
        };
        self.agents = sampled
            .agents
            .into_iter()
            .map(|(pane, mut sample)| {
                let heard = match self.agents.get(&pane) {
                    Some(known) if known.heard => {
                        sample.status = known.sample.status;
                        true
                    }
                    _ => false,
                };
                (pane, Agent { sample, heard })
            })
            .collect();
        let panes: Vec<(&str, &AgentSample)> =
            self.agents.iter().map(|(pane, a)| (pane.as_str(), &a.sample)).collect();
        let Some(members) = self.host.members(&panes, &sampled.odd, workspace.as_deref()) else {
            self.hold_membership();
            return Ok(());
        };
        let mut members: Vec<String> = members.into_iter().map(str::to_string).collect();
        members.sort();
        // A member first seen mid-turn may have written before anyone watched: a gap, like any.
        if members.iter().any(|pane| !self.members.contains(pane)) {
            self.host.gap();
        }
        self.members = members;
        self.membership_retry = None;
        self.fold(Instant::now());
        Ok(())
    }

    /// The members are unknown for now: a gap, so a turn that starts meanwhile is never adopted,
    /// retried on a backoff.
    fn hold_membership(&mut self) {
        self.host.gap();
        let mut backoff = self.membership_retry.map(|(_, b)| b).unwrap_or_default();
        self.membership_retry = Some((Instant::now() + backoff.next_wait(), backoff));
        let report = self.host.observe_statuses(None, Instant::now());
        self.report_turn(report);
    }

    /// One agent's status changed, heard `at`: fold the worktree's state again.
    fn observe_status(&mut self, pane: &str, status: &str, at: Instant) {
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.sample.status = Status::from_wire(status);
            agent.heard = true;
            self.fold(at);
        }
    }

    /// Fold the members' statuses, as they stood `at`, into turn tracking.
    fn fold(&mut self, at: Instant) {
        let statuses: Vec<Status> = self
            .members
            .iter()
            .filter_map(|pane| self.agents.get(pane))
            .map(|a| a.sample.status)
            .collect();
        let report = self.host.observe_statuses(Some(&statuses), at);
        self.report_turn(report);
    }

    /// Send the turn state; the app acts only on what changed.
    fn report_turn(&self, report: TurnReport) {
        let _ = self.tx.send(HerdrEvent::Turn(report));
    }

    /// One request on its own connection: herdr reads exactly one line per connection.
    fn request(&self, method: &str) -> Result<Value, String> {
        crate::herdr::socket_request(self.socket.clone(), method, ANSWER)
            .map_err(|e| format!("{method}: {e}"))
    }
}

/// The session as reviewr reads it, and its agents but this pane, by pane. This pane is found by
/// `terminal` once known, else by `pane_id`, which then names its terminal.
pub(crate) fn read_session(
    snapshot: &Value,
    pane_id: &str,
    terminal: &mut Option<String>,
) -> (Session, Option<crate::herdr::Sampled>) {
    let panes = snapshot["panes"].as_array().cloned().unwrap_or_default();
    let mine = panes.iter().find(|p| match terminal.as_deref() {
        Some(t) => p["terminal_id"].as_str() == Some(t),
        None => p["pane_id"].as_str() == Some(pane_id),
    });
    if terminal.is_none() {
        *terminal = mine.and_then(|p| p["terminal_id"].as_str()).map(str::to_string);
    }
    let field = |p: &Value, key: &str| p[key].as_str().map(str::to_string);
    let me =
        mine.map(|p| PaneIds { workspace: field(p, "workspace_id"), pane: field(p, "pane_id") });
    // Unknown visibility (no focused tab, or this pane unlisted) keeps reviewr live.
    let visible = match (mine, snapshot["focused_tab_id"].as_str()) {
        (Some(p), Some(tab)) => p["tab_id"].as_str() == Some(tab),
        _ => true,
    };
    let agents =
        crate::herdr::samples_of(&snapshot["agents"], me.as_ref().and_then(|m| m.pane.as_deref()));
    (Session { me, visible }, agents)
}

fn parse_version(version: &str) -> Option<semver::Version> {
    semver::Version::parse(version.trim().trim_start_matches('v')).ok()
}

#[cfg(test)]
mod tests {
    use super::{parse_version, read_session};
    use serde_json::json;

    fn snapshot(focused_tab: &str, my_tab: &str, my_pane: &str) -> serde_json::Value {
        json!({
            "focused_tab_id": focused_tab,
            "panes": [
                { "pane_id": my_pane, "terminal_id": "term_me", "tab_id": my_tab,
                  "workspace_id": my_pane.split(':').next().unwrap() },
                { "pane_id": "w1:p2", "terminal_id": "term_agent", "tab_id": my_tab, "workspace_id": "w1" },
            ],
            "agents": [
                { "pane_id": "w1:p2", "cwd": "/repo", "agent_status": "working", "agent": "claude",
                  "tab_id": my_tab, "workspace_id": "w1" },
            ],
        })
    }

    #[test]
    fn this_pane_is_visible_exactly_when_its_tab_is_the_focused_one() {
        let mut terminal = None;
        let (s, agents) =
            read_session(&snapshot("w1:t1", "w1:t1", "w1:p1"), "w1:p1", &mut terminal);
        assert!(s.visible);
        assert_eq!(terminal.as_deref(), Some("term_me"), "the first snapshot names the terminal");
        assert_eq!(agents.map(|a| a.agents.len()), Some(1), "the agent beside it, never this pane");
        let (s, _) = read_session(&snapshot("w1:t2", "w1:t1", "w1:p1"), "w1:p1", &mut terminal);
        assert!(!s.visible, "another tab is on screen");
    }

    #[test]
    fn a_snapshot_with_no_focused_tab_keeps_the_pane_live() {
        let mut snap = snapshot("w1:t1", "w1:t1", "w1:p1");
        snap["focused_tab_id"] = serde_json::Value::Null;
        let (s, _) = read_session(&snap, "w1:p1", &mut None);
        assert!(s.visible, "unknown visibility never hides the pane");
    }

    #[test]
    fn a_moved_pane_is_found_by_its_terminal() {
        let mut terminal = Some("term_me".to_string());
        let moved = snapshot("w2:t5", "w2:t5", "w2:p9");
        let (s, _) = read_session(&moved, "w1:p1", &mut terminal);
        let me = s.me.expect("found after the move");
        assert_eq!((me.pane.as_deref(), me.workspace.as_deref()), (Some("w2:p9"), Some("w2")));
        assert!(s.visible, "visibility follows the new tab");
    }

    #[test]
    fn the_manifest_asks_herdr_for_the_version_the_connection_needs() {
        let manifest = include_str!("../herdr-plugin.toml");
        let (major, minor, patch) = super::MIN_VERSION;
        let floor = format!("min_herdr_version = \"{major}.{minor}.{patch}\"");
        assert!(manifest.contains(&floor), "herdr-plugin.toml must say {floor}");
    }

    #[test]
    fn versions_compare_by_number() {
        let min = semver::Version::new(0, 9, 3);
        assert_eq!(parse_version("0.9.3"), Some(min.clone()));
        assert!(parse_version("v0.10.0-rc1").unwrap() > min);
        assert!(
            parse_version("0.9.3-rc1").unwrap() < min,
            "a release candidate is not the release"
        );
        assert!(parse_version("0.9.2").unwrap() < min);
        assert_eq!(parse_version("garbage"), None);
    }
}
