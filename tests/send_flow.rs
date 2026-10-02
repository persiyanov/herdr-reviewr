//! End-to-end send dispatch through a fake herdr binary. This file is its own test process, so the HERDR_* environment it
//! sets can never leak into another test binary, and no real herdr pane is ever addressed.
#![cfg(unix)]

mod common;

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{Repo, app_on};
use herdr_reviewr::app::{App, Focus, Mode, Tab};
use herdr_reviewr::forge::{Comment, CommentKind, FindingPlace, PrSnapshot, PrView};
use herdr_reviewr::keymap::Keymap;
use herdr_reviewr::ui;
use herdr_reviewr::{handle_key, handle_mouse};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

// `cwd` rides every real `agent list` entry (api notes). Send ignores it and resolves from
// the workspace, so it is here to keep the fixture honest rather than to steer the send.
const TWO_AGENTS: &str = r#"{"result":{"agents":[
  {"agent":"claude","agent_status":"idle","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","cwd":"/w/one"},
  {"agent":"codex","agent_status":"working","pane_id":"w8:p2","tab_id":"w8:t1","workspace_id":"w8","cwd":"/w/two"}
]}}"#;
const ONE_AGENT: &str = r#"{"result":{"agents":[
  {"agent":"claude","agent_status":"idle","pane_id":"w8:p1","tab_id":"w8:t1","workspace_id":"w8","cwd":"/w/one"}
]}}"#;

/// A fake herdr: answers `agent list` from `agents.json`, `tab list` with one label, logs
/// every invocation, and succeeds at everything else (`pane send-text`, `pane focus`). It
/// fails whatever `fail` holds, so a dead pane and a broken enumeration both have a shape.
fn write_fake_herdr(dir: &Path) -> PathBuf {
    let script = dir.join("herdr");
    fs::write(
        &script,
        "#!/bin/sh\n\
         dir=$(dirname \"$0\")\n\
         echo \"$@\" >> \"$dir/log\"\n\
         case \"$*\" in\n\
           $(cat \"$dir/fail\" 2>/dev/null || echo __none__)*)\n\
             echo '{\"error\":{\"code\":\"pane_not_found\",\"message\":\"pane w8:p1 not found\"},\"id\":\"cli:request\"}' >&2\n\
             exit 1 ;;\n\
         esac\n\
         case \"$1 $2\" in\n\
           \"agent list\") cat \"$dir/agents.json\" ;;\n\
           \"tab list\") echo '{\"result\":{\"tabs\":[{\"tab_id\":\"w8:t1\",\"label\":\"Grip\"}]}}' ;;\n\
           *) : ;;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Make the fake herdr exit non-zero for every invocation starting with `prefix`.
fn fail_on(dir: &Path, prefix: &str) {
    fs::write(dir.join("fail"), prefix).unwrap();
}

fn fail_on_nothing(dir: &Path) {
    let _ = fs::remove_file(dir.join("fail"));
}

fn log(dir: &Path) -> String {
    fs::read_to_string(dir.join("log")).unwrap_or_default()
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

/// The crate forbids `unsafe`, which rules out in-process `env::set_var`, so the parent
/// run re-executes this same test in a child process with the HERDR_* seam applied at
/// spawn — env applied to a child is safe, and the child alone runs the body.
#[test]
fn send_dispatches_one_agent_directly_and_several_through_the_picker() {
    if env::var("SEND_FLOW_CHILD").is_err() {
        let staging = tempfile::TempDir::new().expect("tempdir");
        let script = write_fake_herdr(staging.path());
        let out = std::process::Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "send_dispatches_one_agent_directly_and_several_through_the_picker",
                "--nocapture",
            ])
            .env("SEND_FLOW_CHILD", "1")
            .env("FAKE_HERDR_DIR", staging.path())
            .env("HERDR_BIN_PATH", &script)
            .env("HERDR_WORKSPACE_ID", "w8")
            .env("HERDR_PANE_ID", "w8:p9")
            .output()
            .expect("re-exec the test with the fake herdr env");
        assert!(
            out.status.success(),
            "child run failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        // libtest exits 0 when `--exact` matches nothing, so the status alone cannot tell a
        // passing body from a filter that selected no test. The fake herdr's log is the proof
        // the body actually ran and delivered.
        assert!(
            log(staging.path()).contains("pane send-text"),
            "the child ran no send — did the test name and the `--exact` filter drift apart?\n{}",
            String::from_utf8_lossy(&out.stdout),
        );
        return;
    }

    let r = Repo::init();
    r.write("a.rs", "alpha\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nbeta\n");

    let fake_dir = PathBuf::from(env::var("FAKE_HERDR_DIR").expect("set by the parent run"));
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);
    let mut app = app_on(&r);

    // Several agents: `s` opens the picker over both rows, labelled from `tab list`, and with
    // nothing sent yet the highlight arms on the first row.
    fs::write(fake_dir.join("agents.json"), TWO_AGENTS).unwrap();
    write_comment(&mut app, "one");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Picker, "several agents open the picker");
    assert_eq!(app.picker_rows.len(), 2);
    assert_eq!(app.picker_rows[0].tab, "Grip", "the tab label joins on tab_id");
    assert_eq!(app.picker_cursor, 0, "nothing sent this session arms the first row");

    // A chosen pane that closed while the picker was open fails the send, and every comment
    // stays. Nothing arms, since nothing was delivered.
    fail_on(&fake_dir, "pane send-text w8:p1");
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "the picker closes whatever the outcome");
    assert_eq!(app.store.len(), 1, "a failed send keeps every comment");
    // One short sentence a reviewer can read. herdr's own wording is a JSON envelope around a
    // pane id, and the argv it came from carries the whole review in its last argument — both
    // would fill a 40-column footer without naming anything.
    assert_eq!(app.status, "agent not found");
    assert_eq!(app.last_sent_pane, None, "a failed send arms nothing");
    fail_on_nothing(&fake_dir);

    // One agent: `s` sends straight through, no picker frame in between — and the direct
    // send arms its agent like a picker send.
    fs::write(fake_dir.join("agents.json"), ONE_AGENT).unwrap();
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "one agent sends directly");
    assert!(app.store.is_empty(), "a successful send consumes the whole set");
    assert_eq!(app.status, "added 1 comment to claude");
    assert_eq!(app.last_sent_pane.as_deref(), Some("w8:p1"));

    // `enter` sends to the digit-selected agent and consumes the set.
    fs::write(fake_dir.join("agents.json"), TWO_AGENTS).unwrap();
    write_comment(&mut app, "two");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    press(&mut app, KeyCode::Char('2'), area, &keymap);
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.store.is_empty(), "a successful send consumes the whole set");
    assert_eq!(app.status, "added 1 comment to codex");
    assert_eq!(app.last_sent_pane.as_deref(), Some("w8:p2"));
    assert!(log(&fake_dir).contains("pane send-text w8:p2"), "log: {}", log(&fake_dir));
    // The start marker opens the payload at the CLI boundary; `pasted()` owns the rationale.
    assert!(
        log(&fake_dir).contains("pane send-text w8:p2 \u{1b}[200~"),
        "the send is framed as a bracketed paste: {}",
        log(&fake_dir)
    );
    // The batch's last bytes are the comment text "two", so this pins the terminator to the
    // end of a delivered payload.
    assert!(
        log(&fake_dir).contains("two\u{1b}[201~"),
        "the frame terminator closes the batch: {}",
        log(&fake_dir)
    );
    assert!(log(&fake_dir).contains("agent focus w8:p2"), "a send focuses its pane");

    // Several again: the last-sent agent outranks the first row, and a first click on that
    // armed row sends immediately.
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
    let sends = log(&fake_dir).matches("pane send-text w8:p2").count();
    assert_eq!(
        sends,
        2,
        "the digit-selected send and the armed-row click addressed the same pane: {}",
        log(&fake_dir)
    );

    // No agent, and an enumeration herdr never answered, both refuse and name the clipboard —
    // and neither opens a picker.
    fs::write(fake_dir.join("agents.json"), r#"{"result":{"agents":[]}}"#).unwrap();
    write_comment(&mut app, "four");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "an empty workspace opens no picker");
    assert_eq!(app.store.len(), 1, "a refusal keeps every comment");
    assert_eq!(app.status, "no agent here — copy to the clipboard instead");

    fail_on(&fake_dir, "agent list");
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "a failed enumeration opens no picker");
    assert_eq!(app.store.len(), 1, "a refusal keeps every comment");
    // A failed enumeration says so rather than claiming a count. The argv and herdr's stderr go
    // to the log, so the sentence still fits a 40-column footer.
    assert_eq!(app.status, "herdr did not answer — copy to the clipboard instead");
}

#[test]
fn the_pr_tab_sends_the_selected_comment_and_all_of_them() {
    if env::var("SEND_FLOW_CHILD").is_err() {
        let staging = tempfile::TempDir::new().expect("tempdir");
        let script = write_fake_herdr(staging.path());
        let out = std::process::Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "the_pr_tab_sends_the_selected_comment_and_all_of_them",
                "--nocapture",
            ])
            .env("SEND_FLOW_CHILD", "1")
            .env("FAKE_HERDR_DIR", staging.path())
            .env("HERDR_BIN_PATH", &script)
            .env("HERDR_WORKSPACE_ID", "w8")
            .env("HERDR_PANE_ID", "w8:p9")
            .output()
            .expect("re-exec the test with the fake herdr env");
        assert!(
            out.status.success(),
            "child run failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        // The libtest exit status alone cannot tell a passing body from a filter that selected
        // no test — the fake herdr's log is the proof the body ran and delivered.
        assert!(
            log(staging.path()).contains("pane send-text"),
            "the child ran no send — did the test name and the `--exact` filter drift apart?\n{}",
            String::from_utf8_lossy(&out.stdout),
        );
        return;
    }

    let r = Repo::init();
    r.write("a.rs", "alpha\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nbeta\n");

    let fake_dir = PathBuf::from(env::var("FAKE_HERDR_DIR").expect("set by the parent run"));
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);
    let mut app = app_on(&r);
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
    fs::write(fake_dir.join("agents.json"), ONE_AGENT).unwrap();

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
    let sent = log(&fake_dir);
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
    let sent = log(&fake_dir);
    let at_finding = sent.rfind("@alice: the beta line needs a test").expect("finding sent");
    let at_review = sent.rfind("@ann (review): approved").expect("review sent");
    assert!(at_finding < at_review, "the snapshot's newest-first order is the send order: {sent}");

    // Several agents: `a` opens the picker over the staged PR bytes. A cancel delivers nothing
    // and drops the bytes — the next open re-stages fresh ones.
    fs::write(fake_dir.join("agents.json"), TWO_AGENTS).unwrap();
    let sends_before = log(&fake_dir).matches("pane send-text").count();
    press(&mut app, KeyCode::Char('a'), area, &keymap);
    assert_eq!(app.mode, Mode::Picker, "several agents open the picker");
    press(&mut app, KeyCode::Esc, area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "esc cancels the PR picker");
    assert_eq!(
        log(&fake_dir).matches("pane send-text").count(),
        sends_before,
        "a cancel delivers nothing"
    );

    press(&mut app, KeyCode::Char('a'), area, &keymap);
    assert_eq!(app.mode, Mode::Picker, "the next open re-stages the list");
    press(&mut app, KeyCode::Char('2'), area, &keymap);
    press(&mut app, KeyCode::Enter, area, &keymap);
    assert_eq!(app.status, "sent 2 PR comments to codex");
    assert_eq!(app.last_sent_pane.as_deref(), Some("w8:p2"), "a PR send arms like any other");

    // A refusal names the clipboard, opens no picker, and keeps the list.
    fs::write(fake_dir.join("agents.json"), r#"{"result":{"agents":[]}}"#).unwrap();
    press(&mut app, KeyCode::Char('a'), area, &keymap);
    assert_eq!(app.mode, Mode::Normal, "an empty workspace opens no picker");
    assert_eq!(app.status, "no agent here — copy to the clipboard instead");
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
    let calls_before = log(&fake_dir).lines().count();
    press(&mut app, KeyCode::Char('s'), area, &keymap);
    assert_eq!(app.status, "no PR comment selected");
    assert_eq!(log(&fake_dir).lines().count(), calls_before, "no herdr call behind the refusal");
}
