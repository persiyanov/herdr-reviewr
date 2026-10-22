//! The herdr connection against a fake herdr speaking 0.9.3's wire as observed live, over a Unix
//! socket or, on Windows, the named pipe herdr binds for the same path.

mod common;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::Repo;
use herdr_reviewr::herdr_socket::{Connection, HerdrEvent, Session};
use herdr_reviewr::turn::{LastTurn, TurnNews, TurnReport, Wrote};
use herdr_reviewr::wake::{Waker, channel};
use serde_json::{Value, json};

type Subscriber = Box<dyn Write + Send>;
/// A connection split into the half that reads it and the half that writes it.
type Halves<S> = fn(S) -> (Box<dyn Read + Send>, Subscriber);

#[derive(Default)]
struct State {
    snapshot: Value,
    subscribers: Vec<Subscriber>,
    connections: usize,
    /// Every subscription request's list, in order.
    subscribed: Vec<Value>,
    /// Refuse this many subscribes that name a pane, as herdr does for a pane that just closed.
    refuse_pane_subscribes: usize,
    /// Lines the next subscribe that names a pane writes in the same write as its ack: herdr
    /// polls and writes as soon as the subscription is live.
    with_next_ack: Vec<Value>,
    /// The session the next subscribe that names a pane leaves behind: what changed while it
    /// went live.
    after_next_subscribe: Option<Value>,
    /// Read every subscription until reviewr closes it, counting the closes in `closed`: nothing
    /// is written to it, so only reviewr's own close can end that read.
    watch_closes: bool,
    closed: usize,
    /// How many session snapshots were asked for.
    snapshots: usize,
    /// Hold the next pane subscribe's ack this long, as a slow herdr would.
    delay_next_ack: Option<Duration>,
}

struct FakeHerdr {
    path: PathBuf,
    state: Arc<Mutex<State>>,
    _dir: tempfile::TempDir,
}

impl FakeHerdr {
    fn start(version: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let state = Arc::new(Mutex::new(State { snapshot: session("w1:t1"), ..State::default() }));
        listen(&path, Arc::clone(&state), version.to_string());
        Self { path, state, _dir: dir }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn set_focus(&self, tab: &str) {
        self.state().snapshot = session(tab);
    }

    fn set_snapshot(&self, snapshot: Value) {
        self.state().snapshot = snapshot;
    }

    /// Session-wide events, written together: herdr's one tick.
    fn emit(&self, events: &[&str]) {
        let lines: Vec<Value> = events
            .iter()
            .map(|e| json!({"event": e, "data": {"type": e.replace('.', "_")}}))
            .collect();
        self.push(&lines);
    }

    /// Write lines to every subscription in one write, herdr's one tick, and count the ones
    /// still open: a closed one fails the write and is dropped.
    fn push(&self, lines: &[Value]) -> usize {
        let text = lines.iter().fold(String::new(), |mut text, l| {
            text.push_str(&l.to_string());
            text.push('\n');
            text
        });
        let mut state = self.state();
        state
            .subscribers
            .retain_mut(|s| s.write_all(text.as_bytes()).and_then(|()| s.flush()).is_ok());
        state.subscribers.len()
    }

    fn drop_subscribers(&self) {
        self.state().subscribers.clear();
    }
}

/// Bind a Unix domain socket at `path`, as herdr does on unix.
#[cfg(unix)]
fn listen(path: &Path, state: Arc<Mutex<State>>, version: String) {
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    let halves: Halves<std::os::unix::net::UnixStream> =
        |s| (Box::new(s.try_clone().unwrap()), Box::new(s));
    std::thread::spawn(move || serve(listener.incoming(), &state, &version, halves));
}

/// Bind the named pipe herdr binds for a socket path on Windows.
#[cfg(windows)]
fn listen(path: &Path, state: Arc<Mutex<State>>, version: String) {
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, prelude::*};
    let name = path.to_string_lossy().into_owned().to_ns_name::<GenericNamespaced>().unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();
    let halves: Halves<interprocess::local_socket::Stream> = |s| {
        let (read, write) = s.split();
        (Box::new(read), Box::new(write))
    };
    std::thread::spawn(move || serve(listener.incoming(), &state, &version, halves));
}

fn serve<S: Read + Write + Send + 'static>(
    incoming: impl Iterator<Item = io::Result<S>>,
    shared: &Arc<Mutex<State>>,
    version: &str,
    halves: Halves<S>,
) {
    for stream in incoming {
        let mut reader = BufReader::new(stream.unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let mut out = reader.into_inner();
        let request: Value = serde_json::from_str(&line).unwrap();
        let id = request["id"].clone();
        let mut state = shared.lock().unwrap();
        state.connections += 1;
        match request["method"].as_str().unwrap() {
            "ping" => {
                let pong = json!({"id": id, "result": {"type": "pong", "version": version, "protocol": 22}});
                let _ = writeln!(out, "{pong}");
            }
            "session.snapshot" => {
                state.snapshots += 1;
                let snapshot = state.snapshot.clone();
                let reply =
                    json!({"id": id, "result": {"type": "session_snapshot", "snapshot": snapshot}});
                let _ = writeln!(out, "{reply}");
            }
            "events.subscribe" => {
                let list = request["params"]["subscriptions"].clone();
                let names_pane = list.as_array().unwrap().iter().any(|s| s["pane_id"].is_string());
                state.subscribed.push(list);
                if names_pane && state.refuse_pane_subscribes > 0 {
                    state.refuse_pane_subscribes -= 1;
                    let err =
                        json!({"id": id, "error": {"code": "pane_not_found", "message": "gone"}});
                    let _ = writeln!(out, "{err}");
                    continue;
                }
                let ack = json!({"id": id, "result": {"type": "subscription_started"}});
                let mut text = format!("{ack}\n");
                if names_pane {
                    if let Some(next) = state.after_next_subscribe.take() {
                        state.snapshot = next;
                    }
                    for line in state.with_next_ack.drain(..) {
                        text.push_str(&line.to_string());
                        text.push('\n');
                    }
                }
                if let Some(delay) = state.delay_next_ack.take().filter(|_| names_pane) {
                    let shared = Arc::clone(shared);
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        let _ = out.write_all(text.as_bytes());
                        shared.lock().unwrap().subscribers.push(Box::new(out));
                    });
                    continue;
                }
                let _ = out.write_all(text.as_bytes());
                if !state.watch_closes {
                    state.subscribers.push(Box::new(out));
                    continue;
                }
                let (mut read, write) = halves(out);
                state.subscribers.push(write);
                let shared = Arc::clone(shared);
                std::thread::spawn(move || {
                    while matches!(read.read(&mut [0_u8; 64]), Ok(n) if n > 0) {}
                    shared.lock().unwrap().closed += 1;
                });
            }
            other => panic!("unexpected method {other}"),
        }
    }
}

/// A session where this pane (`w1:p1`, terminal `term_me`) sits in tab `w1:t1` beside an agent.
fn session(focused_tab: &str) -> Value {
    json!({
        "version": "0.9.3",
        "focused_workspace_id": "w1",
        "focused_tab_id": focused_tab,
        "focused_pane_id": "w1:p2",
        "workspaces": [], "tabs": [], "layouts": [],
        "panes": [
            {"pane_id": "w1:p1", "terminal_id": "term_me", "tab_id": "w1:t1", "workspace_id": "w1"},
            {"pane_id": "w1:p2", "terminal_id": "term_agent", "tab_id": "w1:t1", "workspace_id": "w1"},
        ],
        "agents": [agent("w1:p2", "/repo", "idle")],
    })
}

/// An agent pane as the session lists it.
fn agent(pane: &str, cwd: &str, status: &str) -> Value {
    json!({"pane_id": pane, "cwd": cwd, "agent_status": status, "agent": "claude", "tab_id": "w1:t1", "workspace_id": "w1"})
}

fn connect(fake: &FakeHerdr, repo: &Path) -> (Connection, Receiver<HerdrEvent>) {
    let (tx, rx) = channel(&Waker::detached());
    let socket = fake.path.clone().into_os_string();
    let connection = Connection::start(socket, "w1:p1".into(), repo.to_path_buf(), tx).unwrap();
    (connection, rx)
}

/// The next session report, past any turn reports and losses.
fn next_session(rx: &Receiver<HerdrEvent>) -> Session {
    session_until(rx, |_| true)
}

/// Session reports until one shows what `pred` asks: every read is reported, the same or not.
fn session_until(rx: &Receiver<HerdrEvent>, pred: impl Fn(&Session) -> bool) -> Session {
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).expect("an event") {
            HerdrEvent::Session(s) if pred(&s) => return s,
            HerdrEvent::Session(_) | HerdrEvent::Turn(_) | HerdrEvent::Lost => {}
            other @ HerdrEvent::TooOld(_) => panic!("expected a session, got {other:?}"),
        }
    }
}

/// Every event that arrives within `window`.
fn events_within(rx: &Receiver<HerdrEvent>, window: Duration) -> Vec<HerdrEvent> {
    let until = Instant::now() + window;
    std::iter::from_fn(|| rx.recv_timeout(until.saturating_duration_since(Instant::now())).ok())
        .collect()
}

fn wait_subscribed(fake: &FakeHerdr, n: usize) -> Vec<Value> {
    for _ in 0..500 {
        let subscribed = fake.state().subscribed.clone();
        if subscribed.len() >= n {
            return subscribed;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("never subscribed {n} times");
}

#[test]
fn visibility_follows_the_focused_tab() {
    let fake = FakeHerdr::start("0.9.3");
    let repo = tempfile::tempdir().unwrap();
    let (_c, rx) = connect(&fake, repo.path());
    assert!(next_session(&rx).visible, "this pane's tab is focused");

    wait_subscribed(&fake, 1);
    // The read that follows subscribing lands first, so the count starts after it.
    let _ = events_within(&rx, Duration::from_millis(300));
    let before = fake.state().snapshots;
    fake.set_focus("w1:t9");
    fake.emit(&["tab.focused", "workspace.focused", "pane.focused"]);
    session_until(&rx, |s| !s.visible);
    let sessions = events_within(&rx, Duration::from_millis(300))
        .into_iter()
        .filter(|e| matches!(e, HerdrEvent::Session(_)))
        .count();
    assert_eq!(sessions, 0, "a burst of focus events reports once");
    assert_eq!(fake.state().snapshots - before, 1, "and reads the session once");

    fake.set_focus("w1:t1");
    fake.emit(&["tab.focused"]);
    session_until(&rx, |s| s.visible);
}

#[test]
fn every_focus_report_reaches_the_pane_even_unchanged() {
    let fake = FakeHerdr::start("0.9.3");
    let repo = tempfile::tempdir().unwrap();
    fake.set_focus("w1:t9");
    let (_c, rx) = connect(&fake, repo.path());
    assert!(!next_session(&rx).visible, "hidden behind another tab");
    wait_subscribed(&fake, 1);
    let _ = events_within(&rx, Duration::from_millis(300));
    // Input may have shown the pane meanwhile: herdr's word must come through, the same or not.
    fake.emit(&["tab.focused"]);
    assert!(!next_session(&rx).visible, "herdr says hidden again, and the pane hears it");
}

fn tab_focused(tab: &str, workspace: &str) -> Value {
    json!({"event": "tab.focused", "data": {"type": "tab_focused", "tab_id": tab, "workspace_id": workspace}})
}

fn workspace_focused(workspace: &str) -> Value {
    json!({"event": "workspace.focused", "data": {"type": "workspace_focused", "workspace_id": workspace}})
}

/// Connect, and return once the read after subscribing landed and its reports drained.
fn connect_settled(fake: &FakeHerdr) -> (Connection, Receiver<HerdrEvent>, tempfile::TempDir) {
    let repo = tempfile::tempdir().unwrap();
    let (c, rx) = connect(fake, repo.path());
    next_session(&rx);
    wait_subscribed(fake, 1);
    let _ = events_within(&rx, Duration::from_millis(300));
    (c, rx, repo)
}

fn sessions_within(rx: &Receiver<HerdrEvent>, window: Duration) -> Vec<Session> {
    events_within(rx, window)
        .into_iter()
        .filter_map(|e| match e {
            HerdrEvent::Session(s) => Some(s),
            _ => None,
        })
        .collect()
}

#[test]
fn a_hidden_pane_reads_nothing_when_focus_moves_between_other_tabs() {
    let fake = FakeHerdr::start("0.9.3");
    fake.set_focus("w1:t9");
    let (_c, rx, _repo) = connect_settled(&fake);
    let before = fake.state().snapshots;
    fake.set_focus("w2:t3");
    fake.push(&[tab_focused("w2:t3", "w2"), workspace_focused("w2")]);
    fake.set_focus("w1:t9");
    fake.push(&[tab_focused("w1:t9", "w1")]);
    assert!(sessions_within(&rx, Duration::from_millis(400)).is_empty(), "nothing to report");
    assert_eq!(fake.state().snapshots - before, 0, "and no session read");
}

#[test]
fn a_hidden_pane_shows_when_focus_arrives_at_its_tab() {
    let fake = FakeHerdr::start("0.9.3");
    fake.set_focus("w2:t3");
    let (_c, rx, _repo) = connect_settled(&fake);
    fake.set_focus("w1:t1");
    fake.push(&[tab_focused("w1:t1", "w1")]);
    assert!(next_session(&rx).visible, "its own tab gained focus");
}

#[test]
fn a_hidden_pane_shows_when_focus_arrives_at_its_workspace() {
    let fake = FakeHerdr::start("0.9.3");
    fake.set_focus("w2:t3");
    let (_c, rx, _repo) = connect_settled(&fake);
    fake.set_focus("w1:t1");
    fake.push(&[workspace_focused("w1")]);
    assert!(next_session(&rx).visible, "its workspace, on its tab, gained focus");
}

#[test]
fn a_visible_pane_hides_when_focus_leaves_its_tab() {
    let fake = FakeHerdr::start("0.9.3");
    let (_c, rx, _repo) = connect_settled(&fake);
    fake.set_focus("w2:t3");
    fake.push(&[tab_focused("w2:t3", "w2")]);
    assert!(!next_session(&rx).visible, "focus moved to another workspace's tab");
    let before = fake.state().snapshots;
    fake.set_focus("w1:t1");
    fake.push(&[workspace_focused("w1")]);
    assert!(next_session(&rx).visible, "and back through its workspace");
    assert_eq!(fake.state().snapshots - before, 1, "one read for the return");
}

#[test]
fn a_moved_hidden_pane_shows_at_once_when_focus_reaches_its_new_tab() {
    let fake = FakeHerdr::start("0.9.3");
    fake.set_focus("w2:t3");
    let (_c, rx, _repo) = connect_settled(&fake);
    let mut moved = session("w3:t5");
    moved["panes"][0] = json!({"pane_id": "w3:p7", "terminal_id": "term_me", "tab_id": "w3:t5", "workspace_id": "w3"});
    fake.set_snapshot(moved);
    let sent = Instant::now();
    fake.emit(&["pane.moved"]);
    fake.push(&[tab_focused("w3:t5", "w3")]);
    assert!(next_session(&rx).visible, "its new tab gained focus");
    // Every pane settles at least 150ms: a report sooner came from the focus event itself.
    assert!(sent.elapsed() < Duration::from_millis(140), "shown at once: {:?}", sent.elapsed());
}

#[test]
fn a_focus_event_without_its_ids_still_reads() {
    let fake = FakeHerdr::start("0.9.3");
    fake.set_focus("w2:t3");
    let (_c, rx, _repo) = connect_settled(&fake);
    let before = fake.state().snapshots;
    fake.push(&[json!({"event": "tab.focused", "data": {"type": "tab_focused"}})]);
    assert!(!next_session(&rx).visible, "read, and still hidden");
    fake.push(&[json!({"event": "workspace.focused", "data": {"type": "workspace_focused"}})]);
    assert!(!next_session(&rx).visible, "read again");
    assert_eq!(fake.state().snapshots - before, 2, "one read for each");
}

#[test]
fn a_burst_of_layout_events_reads_the_session_once() {
    let fake = FakeHerdr::start("0.9.3");
    let (_c, rx, _repo) = connect_settled(&fake);
    let before = fake.state().snapshots;
    fake.emit(&["pane.created", "tab.moved"]);
    std::thread::sleep(Duration::from_millis(50));
    fake.emit(&["pane.closed", "pane.moved", "workspace.moved"]);
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(fake.state().snapshots - before, 0, "the burst settles before any read");
    let sessions = sessions_within(&rx, Duration::from_millis(1500));
    assert_eq!(sessions.len(), 1, "one report for the burst");
    assert_eq!(fake.state().snapshots - before, 1, "from one read");
}

#[test]
fn a_detected_agent_reads_at_once_and_serves_the_burst_before_it() {
    let fake = FakeHerdr::start("0.9.3");
    let (_c, rx, _repo) = connect_settled(&fake);
    let before = fake.state().snapshots;
    fake.emit(&["pane.created", "tab.moved"]);
    std::thread::sleep(Duration::from_millis(30));
    let detected = Instant::now();
    fake.emit(&["pane.agent_detected"]);
    next_session(&rx);
    // Every pane settles at least 150ms: a report sooner came from the detection itself.
    assert!(
        detected.elapsed() < Duration::from_millis(140),
        "read at once: {:?}",
        detected.elapsed()
    );
    assert!(sessions_within(&rx, Duration::from_millis(1500)).is_empty(), "no settle read after");
    assert_eq!(fake.state().snapshots - before, 1, "one read for the whole burst");
}

#[test]
fn a_moved_pane_keeps_its_identity_through_its_terminal() {
    let fake = FakeHerdr::start("0.9.3");
    let repo = tempfile::tempdir().unwrap();
    let (_c, rx) = connect(&fake, repo.path());
    next_session(&rx);
    wait_subscribed(&fake, 1);
    let mut moved = session("w2:t4");
    moved["panes"][0] = json!({"pane_id": "w2:p7", "terminal_id": "term_me", "tab_id": "w2:t4", "workspace_id": "w2"});
    fake.set_snapshot(moved);
    fake.emit(&["pane.moved"]);
    let s =
        session_until(&rx, |s| s.me.as_ref().is_some_and(|m| m.pane.as_deref() == Some("w2:p7")));
    let me = s.me.expect("still found");
    assert_eq!((me.pane.as_deref(), me.workspace.as_deref()), (Some("w2:p7"), Some("w2")));
    assert!(s.visible, "its new tab is the focused one");
}

#[test]
fn a_dropped_connection_reports_and_reconnects_on_a_backoff() {
    let fake = FakeHerdr::start("0.9.3");
    let repo = tempfile::tempdir().unwrap();
    let (_c, rx) = connect(&fake, repo.path());
    next_session(&rx);
    // The first outage waits a second; a connection that keeps dropping at once backs off more.
    let mut waits = Vec::new();
    for drop in 0..2 {
        wait_subscribed(&fake, drop + 1);
        fake.drop_subscribers();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).expect("an event") {
                HerdrEvent::Lost => break,
                HerdrEvent::Turn(_) | HerdrEvent::Session(_) => {}
                other @ HerdrEvent::TooOld(_) => panic!("expected the loss, got {other:?}"),
            }
        }
        let lost = Instant::now();
        assert!(next_session(&rx).visible, "back after the backoff");
        waits.push(lost.elapsed());
    }
    assert!(waits[0] >= Duration::from_millis(900), "the first outage waits a second: {waits:?}");
    assert!(waits[0] < Duration::from_millis(1900), "the first outage waits a second: {waits:?}");
    assert!(waits[1] >= Duration::from_millis(1800), "a flapping one waits longer: {waits:?}");
}

#[test]
fn an_old_herdr_is_reported_once_and_never_retried() {
    let fake = FakeHerdr::start("0.9.2");
    let repo = tempfile::tempdir().unwrap();
    let (_c, rx) = connect(&fake, repo.path());
    match rx.recv_timeout(Duration::from_secs(5)).expect("an event") {
        HerdrEvent::TooOld(version) => assert_eq!(version, "0.9.2"),
        other => panic!("expected TooOld, got {other:?}"),
    }
    assert!(events_within(&rx, Duration::from_millis(1500)).is_empty(), "nothing more");
    assert_eq!(fake.state().connections, 1, "one ping, no reconnect loop");
}

#[test]
#[ignore = "live herdr: run inside a herdr pane with --ignored"]
fn the_live_server_answers_with_this_pane() {
    let socket = std::env::var_os("HERDR_SOCKET_PATH").expect("inside herdr");
    let pane = std::env::var("HERDR_PANE_ID").expect("inside herdr");
    let (tx, rx) = channel(&Waker::detached());
    let repo = std::env::current_dir().unwrap();
    let _c = Connection::start(socket, pane.clone(), repo, tx).unwrap();
    let s = next_session(&rx);
    let me = s.me.expect("the live session lists this pane");
    println!("live: me={me:?} visible={}", s.visible);
    assert_eq!(me.pane.as_deref(), Some(pane.as_str()));
}

// --- turns --------------------------------------------------------------------------------

/// A repository an agent works in, by its resolved top level.
fn agent_repo() -> (Repo, PathBuf) {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let root = herdr_reviewr::git::toplevel(r.path()).unwrap();
    (r, root)
}

fn session_with_agent(root: &Path, status: &str) -> Value {
    let mut s = session("w1:t1");
    s["agents"] = json!([agent("w1:p2", root.to_str().unwrap(), status)]);
    s
}

fn status(pane: &str, status: &str) -> Value {
    json!({"event": "pane.agent_status_changed", "data": {"pane_id": pane, "workspace_id": "w1", "agent_status": status}})
}

/// Feed the connection a write of `paths` at `at`, as the watcher would.
fn feed_write(c: &Connection, at: Instant, paths: &[&str]) {
    let paths = Some(paths.iter().map(|p| (*p).to_string()).collect());
    c.feed().send(TurnNews::Wrote(Wrote { first: at, last: at, paths }));
}

/// Connect for a repo; returns once its first subscription is live and the read after it landed.
fn connect_for(fake: &FakeHerdr, root: &Path) -> (Connection, Receiver<HerdrEvent>) {
    let connected = connect(fake, root);
    next_session(&connected.1);
    wait_subscribed(fake, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.state().snapshots < 2 {
        assert!(Instant::now() < deadline, "the read after subscribing never came");
        std::thread::sleep(Duration::from_millis(10));
    }
    connected
}

/// Turn reports until `pred` holds, failing after a while.
fn turn_until(rx: &Receiver<HerdrEvent>, pred: impl Fn(&TurnReport) -> bool) -> TurnReport {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("a turn report") {
            HerdrEvent::Turn(r) if pred(&r) => return r,
            _ => {}
        }
    }
}

#[test]
fn a_turn_promotes_on_its_first_fed_change() {
    let (r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (c, rx) = connect_for(&fake, &root);
    let subscribed = wait_subscribed(&fake, 1);
    assert!(
        subscribed.last().unwrap().as_array().unwrap().iter().any(|s| s["pane_id"] == "w1:p2"),
        "the member agent's status is subscribed: {subscribed:?}"
    );
    let present = turn_until(&rx, |t| t.agents_present.is_some());
    assert_eq!(present.agents_present, Some(true), "an agent works here");

    fake.push(&[status("w1:p2", "working")]);
    // The start snapshot lands whenever the thread takes it: writes reported well after any
    // snapshot keep coming, as an agent's do, until one is a change against it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let promoted = (1..)
        .find_map(|n| {
            r.write("a.rs", &format!("edit {n}\n"));
            let later = Instant::now() + Duration::from_secs(5);
            feed_write(&c, later, &["a.rs"]);
            assert!(Instant::now() < deadline, "never promoted");
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(HerdrEvent::Turn(t)) if t.last.tree().is_some() => Some(t),
                _ => None,
            }
        })
        .expect("promoted");
    let persisted = herdr_reviewr::git::read_baseline_ref(&root);
    assert_eq!(persisted.as_deref(), promoted.last.tree(), "and persisted");
}

#[test]
fn the_watcher_feeds_turn_tracking_without_the_frame_loop() {
    let (r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (c, rx) = connect_for(&fake, &root);
    // No frame loop runs here: the watcher's writes go to the connection's thread alone.
    let (tx, _batches) = channel(&Waker::detached());
    let _watch = herdr_reviewr::watch::Watch::start(&root, None, tx, Some(c.feed()));
    std::thread::sleep(Duration::from_millis(500));
    fake.push(&[status("w1:p2", "working")]);
    // Well clear of the start snapshot's race window, so the first write can only promote.
    std::thread::sleep(Duration::from_millis(1500));
    let deadline = Instant::now() + Duration::from_secs(15);
    for n in 1.. {
        r.write("a.rs", &format!("one\n{n}\n"));
        if let Ok(HerdrEvent::Turn(t)) = rx.recv_timeout(Duration::from_millis(400))
            && t.last != LastTurn::Waiting
        {
            assert!(t.last.tree().is_some(), "the watcher's write promoted the turn: {t:?}");
            break;
        }
        assert!(Instant::now() < deadline, "the watcher's write never reached a turn");
    }
}

#[test]
fn a_turn_inside_one_delivery_still_has_both_edges() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (_c, rx) = connect_for(&fake, &root);
    fake.push(&[status("w1:p2", "working"), status("w1:p2", "idle")]);
    assert!(turn_until(&rx, |t| t.ended).ended, "working then idle in one tick ends a turn");
}

#[test]
fn a_status_with_the_subscribe_ack_is_not_lost() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    fake.state().with_next_ack.push(status("w1:p2", "working"));
    let (_c, rx) = connect_for(&fake, &root);
    fake.push(&[status("w1:p2", "idle")]);
    assert!(turn_until(&rx, |t| t.ended).ended, "the working that came with the ack opened a turn");
}

#[test]
fn a_status_that_changed_while_subscribing_is_read_from_the_newer_snapshot() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    fake.state().after_next_subscribe = Some(session_with_agent(&root, "working"));
    let (_c, rx) = connect_for(&fake, &root);
    fake.push(&[status("w1:p2", "idle")]);
    assert!(turn_until(&rx, |t| t.ended).ended, "the working read after subscribing opened a turn");
}

#[test]
fn a_snapshot_never_rewinds_a_status_the_stream_already_gave() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (_c, rx) = connect_for(&fake, &root);
    // The stream says working; a focus event then reads a snapshot taken before that.
    fake.push(&[status("w1:p2", "working"), json!({"event": "tab.focused", "data": {}})]);
    let ended = events_within(&rx, Duration::from_millis(300))
        .into_iter()
        .any(|e| matches!(e, HerdrEvent::Turn(t) if t.ended));
    assert!(!ended, "the older snapshot did not end the turn");
    fake.push(&[status("w1:p2", "idle")]);
    assert!(turn_until(&rx, |t| t.ended).ended, "the stream's own idle does");
}

#[test]
fn a_turn_that_starts_during_an_outage_is_never_adopted() {
    let (r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (c, rx) = connect_for(&fake, &root);
    fake.drop_subscribers();
    fake.set_snapshot(session_with_agent(&root, "working"));
    // Reconnected: the agent is already working, and its start fell in the gap.
    wait_subscribed(&fake, 2);
    r.write("a.rs", "two\n");
    let later = Instant::now() + Duration::from_secs(5);
    feed_write(&c, later, &["a.rs"]);
    fake.push(&[status("w1:p2", "idle")]);
    let promoted = events_within(&rx, Duration::from_millis(500))
        .into_iter()
        .any(|e| matches!(e, HerdrEvent::Turn(t) if t.last.tree().is_some()));
    assert!(!promoted, "a turn whose start nobody saw is never adopted");
}

#[test]
fn a_new_member_swaps_the_subscription_and_closes_the_old_one() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    fake.state().watch_closes = true;
    let (_c, rx) = connect_for(&fake, &root);
    let mut two = session_with_agent(&root, "idle");
    two["agents"].as_array_mut().unwrap().push(agent("w1:p3", root.to_str().unwrap(), "idle"));
    fake.set_snapshot(two);
    fake.emit(&["pane.agent_detected"]);
    let subscribed = wait_subscribed(&fake, 2);
    let last = subscribed.last().unwrap().as_array().unwrap();
    assert!(last.iter().any(|s| s["pane_id"] == "w1:p3"), "the new member is subscribed");
    // The swap unblocks the old reader itself: nothing is written to the old subscription.
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.state().closed == 0 {
        assert!(Instant::now() < deadline, "the old subscription never closed");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fake.state().closed, 1, "only the old one closed");
    let outage = events_within(&rx, Duration::from_millis(200))
        .into_iter()
        .any(|e| matches!(e, HerdrEvent::Lost));
    assert!(!outage, "the swap is no outage");
    // A status on the new subscription still counts.
    fake.push(&[status("w1:p3", "working"), status("w1:p3", "idle")]);
    assert!(turn_until(&rx, |t| t.ended).ended, "the new member's turn is tracked");
}

#[test]
fn a_detected_agent_is_subscribed_without_waiting_for_the_settle() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (_c, _rx) = connect_for(&fake, &root);
    let mut two = session_with_agent(&root, "idle");
    two["agents"].as_array_mut().unwrap().push(agent("w1:p3", root.to_str().unwrap(), "working"));
    fake.set_snapshot(two);
    let detected = Instant::now();
    fake.emit(&["pane.agent_detected"]);
    let subscribed = wait_subscribed(&fake, 2);
    assert!(detected.elapsed() < Duration::from_millis(140), "at once: {:?}", detected.elapsed());
    let last = subscribed.last().unwrap().as_array().unwrap();
    assert!(last.iter().any(|s| s["pane_id"] == "w1:p3"), "the new agent's status is subscribed");
}

#[test]
fn every_layout_event_updates_the_members_once_settled() {
    let (_r, root) = agent_repo();
    let cwd = root.to_str().unwrap();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (_c, _rx) = connect_for(&fake, &root);
    let with = |extra: Option<&str>| {
        let mut s = session_with_agent(&root, "idle");
        if let Some(pane) = extra {
            s["agents"].as_array_mut().unwrap().push(agent(pane, cwd, "idle"));
        }
        s
    };
    let steps = [
        ("pane.created", Some("w1:p3")),
        ("pane.closed", None),
        ("pane.moved", Some("w1:p4")),
        ("pane.agent_detected", None),
    ];
    for (n, (event, member)) in steps.into_iter().enumerate() {
        fake.set_snapshot(with(member));
        fake.emit(&[event]);
        let subscribed = wait_subscribed(&fake, n + 2);
        let panes: Vec<&str> = subscribed[n + 1]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["pane_id"].as_str())
            .collect();
        let expected: Vec<&str> = std::iter::once("w1:p2").chain(member).collect();
        assert_eq!(panes, expected, "{event} updates the members");
    }
}

#[test]
fn a_subscribe_refused_for_a_closed_pane_reads_the_session_again() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    // Two panes close one after the other, each refusing a subscribe that named it.
    fake.state().refuse_pane_subscribes = 2;
    let (tx, rx) = channel(&Waker::detached());
    let socket = fake.path.clone().into_os_string();
    let _c = Connection::start(socket, "w1:p1".into(), root.clone(), tx).unwrap();
    let subscribed = wait_subscribed(&fake, 3);
    assert!(subscribed.len() >= 3, "refused twice, then subscribed again: {subscribed:?}");
    let lost = events_within(&rx, Duration::from_millis(300))
        .into_iter()
        .any(|e| matches!(e, HerdrEvent::Lost));
    assert!(!lost, "a refused subscribe is no outage");
}

#[test]
fn an_odd_agent_entry_holds_turn_tracking_only_when_it_may_work_here() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    let elsewhere = tempfile::tempdir().unwrap();
    let with_odd = |cwd: &str| {
        let mut s = session_with_agent(&root, "idle");
        let odd = json!({"pane_id": "w9:p1", "agent": "codex", "cwd": cwd});
        s["agents"].as_array_mut().unwrap().push(odd);
        s
    };
    fake.set_snapshot(with_odd(elsewhere.path().to_str().unwrap()));
    let (_c, rx) = connect_for(&fake, &root);
    let first = turn_until(&rx, |t| t.agents_present.is_some());
    assert_eq!(first.agents_present, Some(true), "an odd agent in another folder holds nothing");

    // Unknown members report no agent presence at all.
    fake.set_snapshot(with_odd(root.to_str().unwrap()));
    fake.emit(&["pane.agent_detected"]);
    let held = turn_until(&rx, |t| t.agents_present.is_none());
    assert_eq!(held.agents_present, None, "an odd agent that may work here holds membership");
}

#[test]
fn an_agent_first_seen_working_starts_no_turn() {
    let (r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (c, rx) = connect_for(&fake, &root);
    // Its turn began before anyone watched it: its first writes may already be on disk.
    let mut two = session_with_agent(&root, "idle");
    two["agents"].as_array_mut().unwrap().push(agent("w1:p3", root.to_str().unwrap(), "working"));
    fake.set_snapshot(two);
    fake.emit(&["pane.agent_detected"]);
    wait_subscribed(&fake, 2);
    r.write("a.rs", "two\n");
    feed_write(&c, Instant::now() + Duration::from_secs(5), &["a.rs"]);
    fake.push(&[status("w1:p3", "idle")]);
    let events = events_within(&rx, Duration::from_millis(500));
    let adopted =
        events.iter().any(|e| matches!(e, HerdrEvent::Turn(t) if t.last.tree().is_some()));
    assert!(!adopted, "a turn nobody saw start is never adopted: {events:?}");
}

#[test]
fn lost_events_are_a_gap_in_turn_tracking_and_no_outage() {
    let (r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (c, rx) = connect_for(&fake, &root);
    // The agent starts working inside the lost events: nobody saw its turn start.
    fake.set_snapshot(session_with_agent(&root, "working"));
    fake.push(&[json!({"error": {"code": "events_lost", "message": "subscriber lagged"}})]);
    std::thread::sleep(Duration::from_millis(300));
    r.write("a.rs", "two\n");
    feed_write(&c, Instant::now() + Duration::from_secs(5), &["a.rs"]);
    fake.push(&[status("w1:p2", "idle")]);
    let events = events_within(&rx, Duration::from_millis(500));
    let adopted =
        events.iter().any(|e| matches!(e, HerdrEvent::Turn(t) if t.last.tree().is_some()));
    assert!(!adopted, "a turn whose start fell in the gap is never adopted: {events:?}");
    assert!(
        !events.iter().any(|e| matches!(e, HerdrEvent::Lost)),
        "the connection stays: {events:?}"
    );
    fake.push(&[status("w1:p2", "working"), status("w1:p2", "idle")]);
    assert!(turn_until(&rx, |t| t.ended).ended, "the same subscription tracks the next turn");
}

#[test]
fn the_old_subscription_counts_until_the_new_one_is_live() {
    let (_r, root) = agent_repo();
    let fake = FakeHerdr::start("0.9.3");
    fake.set_snapshot(session_with_agent(&root, "idle"));
    let (_c, rx) = connect_for(&fake, &root);
    fake.state().delay_next_ack = Some(Duration::from_secs(1));
    let mut two = session_with_agent(&root, "idle");
    two["agents"].as_array_mut().unwrap().push(agent("w1:p3", root.to_str().unwrap(), "idle"));
    fake.set_snapshot(two);
    fake.emit(&["pane.agent_detected"]);
    wait_subscribed(&fake, 2);
    // The new subscription is not acknowledged yet: only the old one carries this turn.
    fake.push(&[status("w1:p2", "working"), status("w1:p2", "idle")]);
    assert!(turn_until(&rx, |t| t.ended).ended, "the old subscription's turn still counts");
}
