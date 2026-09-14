//! Send dispatch end to end: a fake herdr CLI and socket, each test re-run in its own child.

mod common;

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use common::{Repo, UnifiedRow as _, app_on, fake_herdr, herdr_calls, herdr_error};
use herdr_reviewr::app::{App, Focus, Mode, Tab};
use herdr_reviewr::forge::{Comment, CommentKind, FindingPlace, PrSnapshot, PrView};
use herdr_reviewr::keymap::Keymap;
use herdr_reviewr::ui;
use herdr_reviewr::{handle_key, handle_mouse};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tempfile::TempDir;

// `cwd` keeps the fixture honest; the send resolves from the workspace and ignores it.
const TWO_AGENTS: &str = r#"{"result":{"agents":[
  {"agent":"claude","agent_status":"idle","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","cwd":"/w/one"},
  {"agent":"codex","agent_status":"working","pane_id":"w8:p2","tab_id":"w8:t1","workspace_id":"w8","cwd":"/w/two"}
]}}"#;
const ONE_AGENT: &str = r#"{"result":{"agents":[
  {"agent":"claude","agent_status":"idle","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","cwd":"/w/one"}
]}}"#;

/// A line break as the request's JSON spells it, CRLF on Windows.
const NL: &str = if cfg!(windows) { r"\r\n" } else { r"\n" };

/// The review's worktree line as the request's JSON spells it.
fn worktree_line(app: &App) -> String {
    let line = serde_json::to_string(&format!("Worktree: {}", app.repo.display())).unwrap();
    line.trim_matches('"').to_owned()
}

/// What the fake socket does with each request it reads.
#[derive(Clone, Copy, Debug)]
enum Reply {
    /// herdr's success reply.
    Result,
    /// herdr's error reply for a pane that closed after it was resolved.
    PaneGone,
    /// herdr's error reply for a failure of its own, the pane still there.
    Internal,
    /// Close the connection without answering.
    Drop,
    /// Hold the connection open and never answer.
    Hang,
}

/// A fake herdr socket: one request line per connection, answered and recorded.
#[derive(Clone)]
struct FakeSocket {
    requests: Arc<Mutex<Vec<String>>>,
    reply: Arc<Mutex<Reply>>,
}

impl FakeSocket {
    fn serve(path: &Path) -> Self {
        let socket = Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            reply: Arc::new(Mutex::new(Reply::Result)),
        };
        listen(path, socket.clone());
        socket
    }

    fn reply(&self, reply: Reply) {
        *self.reply.lock().unwrap() = reply;
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// The requests that addressed `pane`.
    fn sends_to(&self, pane: &str) -> usize {
        let field = format!(r#""pane_id":"{pane}""#);
        self.requests().iter().filter(|request| request.contains(&field)).count()
    }
}

/// Bind a Unix domain socket at `path`, as herdr does on unix, and serve it on its own thread.
#[cfg(unix)]
fn listen(path: &Path, socket: FakeSocket) {
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    thread::spawn(move || answer_each(listener.incoming(), &socket));
}

/// Bind the named pipe herdr binds for a socket path on Windows.
#[cfg(windows)]
fn listen(path: &Path, socket: FakeSocket) {
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, prelude::*};
    let name = path.to_string_lossy().into_owned().to_ns_name::<GenericNamespaced>().unwrap();
    let listener = ListenerOptions::new().name(name).create_sync().unwrap();
    thread::spawn(move || answer_each(listener.incoming(), &socket));
}

fn answer_each<S: Read + Write>(
    incoming: impl Iterator<Item = io::Result<S>>,
    socket: &FakeSocket,
) {
    let mut held = Vec::new();
    for stream in incoming {
        let mut reader = BufReader::new(stream.unwrap());
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let request = request.trim_end_matches('\n').to_owned();
        let id = serde_json::from_str::<serde_json::Value>(&request).unwrap()["id"].clone();
        socket.requests.lock().unwrap().push(request);
        let answer = match *socket.reply.lock().unwrap() {
            Reply::Result => serde_json::json!({"id": id, "result": {"type": "ok"}}),
            Reply::PaneGone => serde_json::json!({"id": id, "error": {
                "code": "pane_not_found", "message": "pane w8:p1 not found",
            }}),
            Reply::Internal => serde_json::json!({"id": id, "error": {
                "code": "internal", "message": "boom",
            }}),
            Reply::Drop => continue,
            Reply::Hang => {
                held.push(reader);
                continue;
            }
        };
        writeln!(reader.get_mut(), "{answer}").unwrap();
    }
}

/// Re-run test `name` in a child with the herdr env applied at spawn; returns its fixture dir.
fn run_in_child(name: &str, socket: bool) -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut child = Command::new(env::current_exe().unwrap());
    child
        .args(["--exact", name, "--nocapture"])
        .env("SEND_FLOW_CHILD", "1")
        .env("FAKE_HERDR_DIR", dir.path())
        .env("HERDR_BIN_PATH", fake_herdr())
        .env("HERDR_WORKSPACE_ID", "w8")
        .env("HERDR_PANE_ID", "w8:p9");
    if socket {
        child.env("HERDR_SOCKET_PATH", dir.path().join("herdr.sock"));
    } else {
        child.env_remove("HERDR_SOCKET_PATH");
    }
    let out = child.output().expect("re-exec the test with the fake herdr env");
    assert!(
        out.status.success(),
        "child run failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    dir
}

fn in_child() -> bool {
    env::var("SEND_FLOW_CHILD").is_ok()
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env::var("FAKE_HERDR_DIR").expect("set by the parent run"))
}

fn socket_path() -> PathBuf {
    PathBuf::from(env::var("HERDR_SOCKET_PATH").expect("set by the parent run"))
}

fn agents(dir: &Path, json: &str) {
    fs::write(dir.join("agents.json"), json).unwrap();
}

/// Make the fake herdr's `agent list` fail the way herdr does, or stop failing.
fn agent_list_fails(dir: &Path, fails: bool) {
    let path = dir.join("agentsfail");
    if fails {
        fs::write(path, herdr_error("internal")).unwrap();
    } else {
        let _ = fs::remove_file(path);
    }
}

/// The one-file repo every send runs over: the first added line is `a.rs:2`, `+beta`.
fn app() -> (Repo, App) {
    let r = Repo::init();
    r.write("a.rs", "alpha\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nbeta\n");
    let app = app_on(&r);
    (r, app)
}

/// Save one comment on the first added line, so `Send` has something to deliver.
fn write_comment(app: &mut App, text: &str) {
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
    app.input = text.to_string();
    app.submit_comment();
}

fn press(app: &mut App, code: KeyCode, area: Rect, keymap: &Keymap) {
    handle_key(app, KeyEvent::from(code), area, keymap).unwrap();
}

#[test]
fn send_dispatches_one_agent_directly_and_several_through_the_picker() {
    if !in_child() {
        let dir =
            run_in_child("send_dispatches_one_agent_directly_and_several_through_the_picker", true);
        assert!(herdr_calls(dir.path()).contains("agent focus"), "the child delivered no send");
        return;
    }

    let fake_dir = fixture_dir();
    let socket = FakeSocket::serve(&socket_path());
    let (_repo, mut app) = app();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);
    fs::write(
        fake_dir.join("tabs.json"),
        r#"{"result":{"tabs":[{"tab_id":"w8:t1","label":"Grip"}]}}"#,
    )
    .unwrap();

    // Several agents: the picker opens over both, labelled by tab, on the first row.
    agents(&fake_dir, TWO_AGENTS);
    write_comment(&mut app, "one");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Picker, "several agents open the picker");
    assert_eq!(app.picker_rows.len(), 2);
    assert_eq!(app.picker_rows[0].tab, "Grip", "the tab label joins on tab_id");
    assert_eq!(app.picker_cursor, 0, "nothing sent this session arms the first row");

    // A pane closed while the picker was open fails the send; comments stay.
    socket.reply(Reply::PaneGone);
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "the picker closes whatever the outcome");
    assert_eq!(app.store.len(), 1, "a failed send keeps every comment");
    // One short sentence, never herdr's JSON envelope.
    assert_eq!(app.status, "claude closed, press y to copy");
    assert_eq!(app.last_sent_pane, None, "a failed send arms nothing");
    socket.reply(Reply::Result);

    // One agent: `s` sends with no picker, and arms that agent as a picker send would.
    agents(&fake_dir, ONE_AGENT);
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "one agent sends directly");
    assert!(app.store.is_empty(), "a successful send consumes the whole set");
    assert_eq!(app.status, "sent 1 comment to claude");
    assert_eq!(app.last_sent_pane.as_deref(), Some("w8:p1"));

    // `S` opens the picker even over that one agent, and `esc` leaves the comments unsent.
    write_comment(&mut app, "chosen");
    press(&mut app, KeyCode::Char('S'), area, &keymap);
    assert_eq!(app.mode, Mode::Picker, "`send-to` always picks");
    assert_eq!(app.picker_rows.len(), 1);
    press(&mut app, KeyCode::Esc, area, &keymap);
    assert_eq!((app.mode.clone(), app.store.len()), (Mode::Normal, 1));
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert!(app.store.is_empty(), "`s` still sends straight to the one agent");

    // An agent at a prompt takes no send, read at the send itself.
    agents(&fake_dir, TWO_AGENTS);
    write_comment(&mut app, "two");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    press(&mut app, KeyCode::Char('2'), area, &keymap);
    agents(&fake_dir, &TWO_AGENTS.replace("\"working\"", "\"blocked\""));
    let sends = socket.sends_to("w8:p2");
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.store.len(), 1, "an agent at a prompt keeps every comment");
    assert_eq!(app.status, "answer codex's prompt first");
    assert_eq!(socket.sends_to("w8:p2"), sends, "nothing was pasted");

    // `enter` sends to the digit-picked agent mid-turn, and consumes the set.
    agents(&fake_dir, TWO_AGENTS);
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    press(&mut app, KeyCode::Char('2'), area, &keymap);
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.store.is_empty(), "a successful send consumes the whole set");
    assert_eq!(app.status, "sent 1 comment to codex");
    assert_eq!(app.last_sent_pane.as_deref(), Some("w8:p2"));
    // The whole request herdr received.
    let worktree = worktree_line(&app);
    assert_eq!(
        socket.requests().last().unwrap(),
        &format!(
            r#"{{"id":"reviewr:send","method":"pane.send_text","params":{{"pane_id":"w8:p2","text":"\u001b[200~{worktree}{NL}{NL}a.rs:2{NL}+beta{NL}two\u001b[201~"}}}}"#
        )
    );
    assert!(herdr_calls(&fake_dir).contains("agent focus w8:p2"), "a send focuses its pane");

    // The last-sent agent outranks the first row and sends on one click.
    write_comment(&mut app, "three");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Picker);
    assert_eq!(app.picker_cursor, 1, "the last-sent agent outranks the first row");
    let (col, row) = (0..area.height)
        .flat_map(|y| (0..area.width).map(move |x| (x, y)))
        .find(|&(x, y)| ui::hit_picker_row(area, &app, x, y) == Some(1))
        .expect("the armed row is clickable");
    handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        },
        area,
        &[],
        &keymap,
        &herdr_reviewr::export::Clipboard,
    )
    .unwrap();
    assert_eq!(app.mode, Mode::Normal, "a first click on the armed row sends");
    assert!(app.store.is_empty());
    assert_eq!(
        socket.sends_to("w8:p2"),
        2,
        "the digit-selected send and the armed-row click addressed the same pane"
    );

    // No agent, or no answer from herdr: both refuse, name the clipboard, and open no picker.
    agents(&fake_dir, r#"{"result":{"agents":[]}}"#);
    write_comment(&mut app, "four");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "an empty workspace opens no picker");
    assert_eq!(app.store.len(), 1, "a refusal keeps every comment");
    assert_eq!(app.status, "no agent in this workspace, press y to copy");

    agent_list_fails(&fake_dir, true);
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "a failed enumeration opens no picker");
    assert_eq!(app.store.len(), 1, "a refusal keeps every comment");
    // A refused enumeration says herdr refused, never a count.
    assert_eq!(app.status, "herdr refused the send, press y to copy");

    // The sole agent at a prompt is refused the same way.
    agent_list_fails(&fake_dir, false);
    agents(&fake_dir, &ONE_AGENT.replace("\"idle\"", "\"blocked\""));
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.store.len(), 1, "a sole agent at a prompt keeps every comment");
    assert_eq!(app.status, "answer claude's prompt first");

    // A chosen agent gone from the list by the time `enter` lands takes nothing.
    agents(&fake_dir, TWO_AGENTS);
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    agents(&fake_dir, ONE_AGENT);
    press(&mut app, KeyCode::Char('2'), area, &keymap);
    let sends = socket.requests().len();
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.store.len(), 1, "a gone agent keeps every comment");
    assert_eq!(app.status, "codex closed, press y to copy");
    assert_eq!(socket.requests().len(), sends, "nothing was pasted");

    // A herdr that refuses by then says so, rather than claiming the agent is gone.
    agents(&fake_dir, TWO_AGENTS);
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    agent_list_fails(&fake_dir, true);
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.store.len(), 1);
    assert_eq!(app.status, "herdr refused the send, press y to copy");
    agent_list_fails(&fake_dir, false);

    // The quit question's `s` sends, and the pane stays open.
    agents(&fake_dir, ONE_AGENT);
    for tab in ['1', '3'] {
        press(&mut app, KeyCode::Char(tab), area, &keymap);
        if app.store.is_empty() {
            press(&mut app, KeyCode::Char('1'), area, &keymap);
            write_comment(&mut app, "five");
            press(&mut app, KeyCode::Char(tab), area, &keymap);
        }
        press(&mut app, KeyCode::Char('q'), area, &keymap);
        let asked = app.question == Some(herdr_reviewr::app::Question::Quit);
        assert!(asked && !app.should_quit, "unsent comments make `q` ask: {tab}");
        press(&mut app, KeyCode::Char('s'), area, &keymap);
        assert!(app.question.is_none() && !app.should_quit, "{tab}");
        assert!(app.store.is_empty(), "the answer sent them: {tab}");
        assert_eq!(app.status, "sent 1 comment to claude", "{tab}");
    }
}

/// Comments leave only on a `result` reply.
#[test]
fn a_send_consumes_the_comments_only_on_a_result_reply() {
    if !in_child() {
        let dir = run_in_child("a_send_consumes_the_comments_only_on_a_result_reply", true);
        assert!(herdr_calls(dir.path()).contains("agent focus"), "the child delivered no send");
        return;
    }

    let socket = FakeSocket::serve(&socket_path());
    agents(&fixture_dir(), ONE_AGENT);
    let (_repo, mut app) = app();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);
    write_comment(&mut app, "naming");
    let worktree = worktree_line(&app);
    let request = format!(
        r#"{{"id":"reviewr:send","method":"pane.send_text","params":{{"pane_id":"w8:p1","text":"\u001b[200~{worktree}{NL}{NL}a.rs:2{NL}+beta{NL}naming\u001b[201~"}}}}"#
    );

    for (reply, status) in [
        (Reply::PaneGone, "claude closed, press y to copy"),
        // A refusal of herdr's own never claims the agent closed: its pane is still there.
        (Reply::Internal, "herdr refused the send, press y to copy"),
        (Reply::Drop, "herdr didn't answer, press y to copy"),
        // Waits out the whole send bound, past herdr's own read deadline.
        (Reply::Hang, "herdr didn't answer, press y to copy"),
    ] {
        socket.reply(reply);
        press(&mut app, KeyCode::Char('s'), area, &keymap);
        assert_eq!(socket.requests().last(), Some(&request), "{reply:?}");
        assert_eq!(app.store.len(), 1, "{reply:?} keeps every comment");
        assert_eq!(app.status, status, "{reply:?}");
    }

    socket.reply(Reply::Result);
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(socket.requests().last(), Some(&request));
    assert!(app.store.is_empty(), "a result reply consumes the whole set");
    assert_eq!(app.status, "sent 1 comment to claude");
}

/// A review past Windows' command-line cap sends; one past the send cap refuses.
#[test]
fn a_long_review_sends_whole_and_one_over_the_cap_refuses() {
    if !in_child() {
        let dir = run_in_child("a_long_review_sends_whole_and_one_over_the_cap_refuses", true);
        assert!(herdr_calls(dir.path()).contains("agent focus"), "the child delivered no send");
        return;
    }

    let socket = FakeSocket::serve(&socket_path());
    agents(&fixture_dir(), ONE_AGENT);
    let (_repo, mut app) = app();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);

    let long = "x".repeat(250 * 1024);
    write_comment(&mut app, &long);
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert!(app.store.is_empty(), "a review just under the cap sends");
    assert_eq!(app.status, "sent 1 comment to claude");
    let worktree = worktree_line(&app);
    assert_eq!(
        socket.requests().last().unwrap(),
        &format!(
            r#"{{"id":"reviewr:send","method":"pane.send_text","params":{{"pane_id":"w8:p1","text":"\u001b[200~{worktree}{NL}{NL}a.rs:2{NL}+beta{NL}{long}\u001b[201~"}}}}"#
        )
    );

    // The cap counts JSON escaping: each `"` is two bytes.
    for review in ["x".repeat(300 * 1024), "\"".repeat(150 * 1024)] {
        write_comment(&mut app, &review);
        let sent = socket.requests().len();
        press(&mut app, KeyCode::Char('s'), area, &keymap);
        assert_eq!(app.status, "review too large to send, press y to copy");
        assert_eq!(app.store.len(), 1, "an over-cap review keeps every comment");
        assert_eq!(socket.requests().len(), sent, "nothing reached herdr");
        app.store = herdr_reviewr::model::CommentStore::default();
    }

    // The cap counts the line breaks as sent: 192 KiB escaped with LF, 320 KiB with CRLF.
    write_comment(&mut app, &"x\n".repeat(64 * 1024));
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    let want = if cfg!(windows) {
        "review too large to send, press y to copy"
    } else {
        "sent 1 comment to claude"
    };
    assert_eq!(app.status, want);
}

/// No `HERDR_SOCKET_PATH` is no herdr to send to.
#[test]
fn a_send_without_a_socket_refuses_as_herdr_not_answering() {
    if !in_child() {
        let dir = run_in_child("a_send_without_a_socket_refuses_as_herdr_not_answering", false);
        assert!(herdr_calls(dir.path()).contains("agent list"), "the child asked herdr nothing");
        return;
    }

    agents(&fixture_dir(), ONE_AGENT);
    let (_repo, mut app) = app();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);
    write_comment(&mut app, "naming");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.status, "herdr didn't answer, press y to copy");
    assert_eq!(app.store.len(), 1, "a refusal keeps every comment");
}

/// The `PR` tab sends: `s` the comment under the cursor, `a` the whole list; neither consumes
/// the read-only snapshot.
#[test]
fn the_pr_tab_sends_the_selected_comment_and_all_of_them() {
    if !in_child() {
        let dir = run_in_child("the_pr_tab_sends_the_selected_comment_and_all_of_them", true);
        assert!(herdr_calls(dir.path()).contains("agent focus"), "the child delivered no send");
        return;
    }

    let socket = FakeSocket::serve(&socket_path());
    agents(&fixture_dir(), ONE_AGENT);
    let (_repo, mut app) = app();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);
    app.set_tab(Tab::Pr).unwrap();

    let finding = Comment {
        kind: CommentKind::Finding,
        author: "alice".into(),
        anchor: "a.rs:2".into(),
        place: Some(FindingPlace { path: "a.rs".into(), range: Some((2, 2)), side: None }),
        body: "the beta line needs a test".into(),
        snippet: Some("@@ -1 +1,2 @@\n-alpha\n+beta".into()),
        ..common::comment()
    };
    let review = Comment {
        kind: CommentKind::Review,
        author: "ann".into(),
        body: "approved".into(),
        ..common::comment()
    };
    // Newest first: the cursor lands on the finding.
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot {
        comments: vec![finding, review],
        ..common::pr_snapshot()
    })));

    // `s` sends the comment under the cursor — anchor, hunk, and quoted body — and the list
    // survives: a PR send never consumes the snapshot.
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "one agent sends directly");
    assert_eq!(app.status, "sent 1 PR comment to claude");
    assert_eq!(app.last_sent_pane.as_deref(), Some("w8:p1"));
    assert_eq!(
        app.pr_snapshot().map(|s| s.comments.len()),
        Some(2),
        "a send never consumes the PR list"
    );
    let sent = socket.requests().last().cloned().unwrap_or_default();
    assert!(
        sent.contains("a.rs:2")
            && sent.contains("+beta")
            && sent.contains("@alice: the beta line needs a test"),
        "the finding rides its anchor, hunk, and quoted body: {sent}"
    );
    assert!(!sent.contains("approved"), "the review is not part of a selected send: {sent}");

    // `a` sends every comment in the snapshot's order, and the list survives that too.
    press(&mut app, KeyCode::Char('a'), area, &keymap);
    assert_eq!(app.status, "sent 2 PR comments to claude");
    assert_eq!(
        app.pr_snapshot().map(|s| s.comments.len()),
        Some(2),
        "send-all never consumes the list either"
    );
    let sent = socket.requests().last().cloned().unwrap_or_default();
    let at_finding = sent.rfind("@alice: the beta line needs a test").expect("finding sent");
    let at_review = sent.rfind("@ann (review): approved").expect("review sent");
    assert!(at_finding < at_review, "the snapshot's newest-first order is the send order: {sent}");

    // Several agents: `a` opens the picker over the staged PR bytes. A cancel delivers nothing
    // and drops the bytes — the next open re-stages fresh ones.
    agents(&fixture_dir(), TWO_AGENTS);
    press(&mut app, KeyCode::Char('a'), area, &keymap);
    assert_eq!(app.mode, Mode::Picker, "several agents open the picker");
    press(&mut app, KeyCode::Esc, area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "esc cancels the PR picker");
    assert_eq!(socket.requests().len(), 2, "a cancel delivers nothing");

    press(&mut app, KeyCode::Char('a'), area, &keymap);
    assert_eq!(app.mode, Mode::Picker, "the next open re-stages the list");
    press(&mut app, KeyCode::Char('2'), area, &keymap);
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.status, "sent 2 PR comments to codex");
    assert_eq!(app.last_sent_pane.as_deref(), Some("w8:p2"), "a PR send arms like any other");
    assert_eq!(socket.sends_to("w8:p2"), 1, "the pick addressed the highlighted agent");

    // A refusal names the clipboard, opens no picker, and keeps the list.
    agents(&fixture_dir(), r#"{"result":{"agents":[]}}"#);
    press(&mut app, KeyCode::Char('a'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "an empty workspace opens no picker");
    assert_eq!(app.status, "no agent in this workspace, press y to copy");
    assert_eq!(app.pr_snapshot().map(|s| s.comments.len()), Some(2), "a refusal keeps the list");

    // The description row selects nothing, so `s` refuses without a herdr call.
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot {
        body: "why".into(),
        comments: vec![
            Comment { body: "one".into(), ..common::comment() },
            Comment { body: "two".into(), ..common::comment() },
        ],
        ..common::pr_snapshot()
    })));
    app.pr_move(-100); // clamps to the top row — the pinned description
    let calls_before = herdr_calls(&fixture_dir()).lines().count();
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.status, "no PR comment selected");
    assert_eq!(
        herdr_calls(&fixture_dir()).lines().count(),
        calls_before,
        "no herdr call behind the refusal"
    );
}
