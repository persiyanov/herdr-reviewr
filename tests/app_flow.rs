//! End-to-end review loop tests: `App` against real repos, exporting to a fake target.

mod common;

use std::cell::RefCell;
use std::path::Path;

use anyhow::{Result, bail};
use common::{Repo, app_on, app_on_rendered, enter_tab, typed};
use herdr_reviewr::app::{App, Band, Focus, FooterAction, Mode};
use herdr_reviewr::config::NavigatorPosition;
use herdr_reviewr::diff::{Bar, MarkerKind, RenderedKind, Row};
use herdr_reviewr::export::ExportTarget;
use herdr_reviewr::herdr::{AgentChoice, AgentSample};
use herdr_reviewr::keymap::{Action, Key, KeyCode as BindingCode, Keymap};
use herdr_reviewr::model::{Scope, Side};
use herdr_reviewr::turn::Status;
use herdr_reviewr::{handle_key, handle_mouse};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

/// An export target that records what it was handed and can be made to fail.
struct FakeTarget {
    ok: bool,
    captured: RefCell<Vec<String>>,
}

impl FakeTarget {
    fn ok() -> Self {
        Self { ok: true, captured: RefCell::new(Vec::new()) }
    }
    fn failing() -> Self {
        Self { ok: false, captured: RefCell::new(Vec::new()) }
    }
    fn last(&self) -> String {
        self.captured.borrow().last().cloned().unwrap_or_default()
    }
}

impl ExportTarget for FakeTarget {
    fn label(&self) -> &'static str {
        "fake"
    }
    fn success_message(&self, count: usize) -> String {
        let noun = if count == 1 { "comment" } else { "comments" };
        format!("exported {count} {noun}")
    }
    fn failure_message(&self, _error: &anyhow::Error, _copy: &str) -> String {
        "fake not found".to_string()
    }
    fn export(&self, text: &str) -> Result<()> {
        self.captured.borrow_mut().push(text.to_string());
        if self.ok { Ok(()) } else { bail!("fake export failure") }
    }
}

/// A repo whose single tracked file `a.rs` has an edit and an appended line.
fn edited_repo() -> Repo {
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\ngamma\ndelta\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nBETA\ngamma\ndelta\nepsilon\n");
    r
}

/// Reveal and bound the diff scroll as the loop does, one display row per row.
fn clamp(app: &mut App, viewport: usize) {
    let heights = vec![1usize; app.visible.len()];
    app.reveal_diff_cursor(&heights, viewport);
    app.bound_diff_scroll(&heights, viewport);
}

#[test]
fn the_file_list_decouples_viewport_scroll_from_selection() {
    let r = Repo::init();
    for i in 0..20 {
        r.write(&format!("f{i:02}.txt"), "one\n");
    }
    r.commit_all("init");
    for i in 0..20 {
        r.write(&format!("f{i:02}.txt"), "two\n");
    }
    let mut app = app_on(&r);
    assert_eq!(app.file_rows.len(), 20);
    let viewport = 6;

    // The first file is selected and its diff is open.
    assert_eq!(app.file_cursor, 0);
    let opened = app.diff_path.clone();
    assert!(opened.is_some());

    // The wheel moves only the viewport, even leaving the cursor off screen.
    app.reveal_files = false; // clear the flag the initial reload set (no event loop here)
    app.wheel_files(5);
    app.bound_file_scroll(viewport);
    assert_eq!(app.file_scroll, 5);
    assert_eq!(app.file_cursor, 0);
    assert_eq!(app.diff_path, opened);
    assert!(app.file_cursor < app.file_scroll);
    assert!(!app.reveal_files, "the wheel does not request a reveal");

    // Moving the selection reveals it and opens that one file.
    app.move_cursor(1).unwrap();
    app.reveal_file_cursor(viewport);
    assert_eq!(app.file_cursor, 1);
    assert!(app.file_cursor >= app.file_scroll && app.file_cursor < app.file_scroll + viewport);
    assert_ne!(app.diff_path, opened);

    // Keyboard nav to the bottom keeps the cursor visible (reveal on each move).
    for _ in 0..18 {
        app.move_cursor(1).unwrap();
    }
    app.reveal_file_cursor(viewport);
    assert_eq!(app.file_cursor, 19);
    assert!(app.file_cursor < app.file_scroll + viewport);
    assert_eq!(app.file_scroll, 20 - viewport);

    // An over-scroll is bounded so the window never shows a blank tail.
    app.wheel_files(100);
    app.bound_file_scroll(viewport);
    assert_eq!(app.file_scroll, 20 - viewport);
}

/// A repo whose single file has `n` lines, all changed, so the diff has many visible rows.
fn long_diff_app(n: usize) -> App {
    use std::fmt::Write as _;
    let r = Repo::init();
    let (mut old, mut new) = (String::new(), String::new());
    for i in 0..n {
        let _ = writeln!(old, "line {i}");
        let _ = writeln!(new, "LINE {i}");
    }
    r.write("a.rs", &old);
    r.commit_all("init");
    r.write("a.rs", &new);
    let mut app = app_on(&r);
    app.reload().unwrap();
    app
}

#[test]
fn bound_diff_scroll_keeps_a_wrapped_bottom_reachable() {
    // 30 rows 3 tall in 20: a row-count cap would stop at 10 and hide the end.
    let mut app = long_diff_app(5);
    let heights = vec![3usize; 30];
    app.diff_scroll = 999; // wheel over-scroll
    app.bound_diff_scroll(&heights, 20);
    assert!(
        app.diff_scroll > 10,
        "height-aware bound passes the row-count cap: {}",
        app.diff_scroll
    );
    assert!(app.diff_scroll <= 29);
}

#[test]
fn the_wheel_scrolls_the_diff_without_moving_its_cursor() {
    let mut app = long_diff_app(40);
    app.focus = Focus::Diff;
    app.diff_cursor = 3;
    app.reveal_diff = false;
    app.wheel_diff(10);
    let h = vec![1usize; app.visible.len()];
    app.bound_diff_scroll(&h, 8);
    assert_eq!(app.diff_cursor, 3, "the wheel leaves the comment cursor put");
    assert!(app.diff_scroll > 0, "the wheel moved the viewport");
    assert!(!app.reveal_diff, "the wheel does not request a reveal");
}

#[test]
fn a_boundary_move_reveals_the_cursor_after_wheeling() {
    // The B1 regression: a navigation that clamps to the same index must still reveal.
    let r = Repo::init();
    for i in 0..20 {
        r.write(&format!("f{i:02}.txt"), "one\n");
    }
    r.commit_all("init");
    for i in 0..20 {
        r.write(&format!("f{i:02}.txt"), "two\n");
    }
    let mut app = app_on(&r);
    let vp = 6;
    app.wheel_files(10);
    app.bound_file_scroll(vp);
    assert!(app.file_cursor < app.file_scroll, "cursor (row 0) is wheeled off-screen above");
    app.reveal_files = false;
    app.move_cursor(-1).unwrap(); // `k` at row 0 — index stays 0
    assert_eq!(app.file_cursor, 0);
    assert!(app.reveal_files, "a clamp-to-same-index move still requests a reveal");
    app.reveal_file_cursor(vp);
    assert_eq!(app.file_scroll, 0, "the cursor is pulled back into view");
}

#[test]
fn toggling_a_directory_requests_a_reveal() {
    let r = folder_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Files;
    let dir = app.file_rows.iter().position(|row| row.dir_path() == Some("src")).unwrap();
    app.file_cursor = dir;
    app.reveal_files = false;
    app.collapse_dir();
    assert!(app.reveal_files, "collapsing a directory requests a reveal (even at the same index)");
}

/// `a.rs` with two inserted hunks, a binary with none, and `c.rs` with one.
fn traversal_repo() -> Repo {
    use std::fmt::Write as _;
    let body = |n: usize| {
        (0..n).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "line {i}");
            s
        })
    };
    let r = Repo::init();
    r.write("a.rs", &body(30));
    r.write("c.rs", &body(10));
    r.write("bin.dat", "\0\0old\n");
    r.commit_all("init");
    r.write(
        "a.rs",
        &body(30).replacen("line 2\n", "line 2\nEDIT ONE\n", 1).replacen(
            "line 25\n",
            "line 25\nEDIT TWO\n",
            1,
        ),
    );
    r.write("c.rs", &body(10).replacen("line 5\n", "line 5\nEDIT THREE\n", 1));
    r.write("bin.dat", "\0\0new\n");
    r
}

/// The text under the diff cursor — where a hunk step landed.
fn cursor_text(app: &App) -> String {
    painted(app, app.diff_cursor)
}

/// The file the list has selected, which tracks the open file through every traversal.
fn selected_path(app: &App) -> Option<&str> {
    let i = app.file_rows[app.file_cursor].file_index()?;
    Some(&app.entries[i].path)
}

#[test]
fn hunk_steps_walk_the_changeset_and_pass_over_hunkless_files() {
    let r = traversal_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    // Files sort alphabetically, so the changeset reads a.rs, bin.dat, c.rs.
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));

    app.reveal_diff = false;
    app.next_hunk();
    assert_eq!(cursor_text(&app), "EDIT ONE");
    assert!(app.reveal_diff, "a jumped-to hunk is scrolled into view");
    app.next_hunk();
    assert_eq!(cursor_text(&app), "EDIT TWO");

    // Past a.rs's last hunk the first press arms the crossing and holds the cursor still.
    app.next_hunk();
    assert_eq!(cursor_text(&app), "EDIT TWO", "the arming press does not move the cursor");
    assert_eq!(app.armed_cross(), Some(true));

    // The second press takes it, crossing over the binary file, which has no hunk.
    (app.reveal_diff, app.reveal_files) = (false, false);
    app.next_hunk();
    assert_eq!(app.diff_path.as_deref(), Some("c.rs"));
    assert_eq!(cursor_text(&app), "EDIT THREE");
    assert!(app.reveal_diff && app.reveal_files, "a crossing reveals the hunk and the file row");
    assert_eq!(app.armed_cross(), None, "the crossing consumed the arm");
    assert_eq!(selected_path(&app), Some("c.rs"), "the list selection follows the crossing");
    assert_eq!(app.focus, Focus::Diff, "crossing keeps the focused pane");

    // The last hunk of the changeset: no file to cross to, so nothing arms and nothing moves.
    app.next_hunk();
    assert_eq!(app.armed_cross(), None, "the footer never offers a crossing that cannot happen");
    app.next_hunk();
    assert_eq!(app.diff_path.as_deref(), Some("c.rs"));
    assert_eq!(cursor_text(&app), "EDIT THREE");

    // Backward arms and crosses the same way, landing on the previous file's *last* hunk.
    app.prev_hunk();
    assert_eq!(app.armed_cross(), Some(false));
    app.prev_hunk();
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
    assert_eq!(cursor_text(&app), "EDIT TWO");
    app.prev_hunk();
    assert_eq!(cursor_text(&app), "EDIT ONE");
    // The first hunk of the changeset: nothing behind it either.
    app.prev_hunk();
    app.prev_hunk();
    assert_eq!(cursor_text(&app), "EDIT ONE");
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
}

#[test]
fn an_armed_crossing_takes_the_footer_and_dies_on_any_other_input() {
    let r = traversal_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    app.next_hunk();
    app.next_hunk();

    // Armed: the offer leads, comment stays demoted, the hunk pair leaves `move`.
    press(&mut app, &keymap, KeyCode::Char(']'));
    assert_eq!(app.armed_cross(), Some(true));
    let bar = app.footer_bands();
    assert_eq!(bar.first(), Some(&(FooterAction::CrossFile { forward: true }, Band::Primary)));
    assert!(bar.iter().any(|&(a, b)| a == FooterAction::Comment && b == Band::Do));
    assert!(
        !bar.iter().any(|&(a, _)| a == FooterAction::MoveHunk),
        "the armed hunk key is not repeated"
    );

    // Any other key drops the arm and still does its own work — here `j` moves the cursor.
    let cursor = app.diff_cursor;
    press(&mut app, &keymap, KeyCode::Char('j'));
    assert_eq!(app.armed_cross(), None, "another key disarms");
    assert_eq!(app.diff_cursor, cursor + 1, "and still moves the cursor");
    assert!(!app.footer_bands().iter().any(|(a, _)| matches!(a, FooterAction::CrossFile { .. })));

    // Disarmed, `]` arms again rather than crossing, so the file boundary always costs two.
    press(&mut app, &keymap, KeyCode::Char(']'));
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"), "the re-arming press stays in the file");
    press(&mut app, &keymap, KeyCode::Char(']'));
    assert_eq!(app.diff_path.as_deref(), Some("c.rs"));

    // A step the other way is not the repeat the arm waits for.
    app.next_hunk();
    assert_eq!(app.armed_cross(), None, "c.rs is the last file: nothing to arm");
    app.prev_hunk();
    assert_eq!(app.armed_cross(), Some(false), "armed backward");
    app.next_hunk();
    assert_eq!(app.armed_cross(), None, "the opposite step disarms");
    assert_eq!(app.diff_path.as_deref(), Some("c.rs"), "and does not cross");
}

#[test]
fn a_resting_pointer_keeps_the_arm_but_a_gesture_drops_it() {
    let r = traversal_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    app.next_hunk();
    app.next_hunk();
    app.next_hunk();
    assert_eq!(app.armed_cross(), Some(true), "armed at a.rs's last hunk");

    // Bare pointer motion is no input, so the crossing stays armed.
    mouse(&mut app, &keymap, MouseEventKind::Moved);
    assert_eq!(app.armed_cross(), Some(true), "pointer motion is not a gesture");

    // A real gesture is: the reviewer reached for the mouse and left the file's edge behind.
    mouse(&mut app, &keymap, MouseEventKind::ScrollDown);
    assert_eq!(app.armed_cross(), None, "a wheel scroll disarms");
}

#[test]
fn file_skips_jump_file_to_file_from_either_pane() {
    let r = Repo::init();
    // Two files under `src/` so it stays a real directory row rather than folding into its child.
    r.write("src/b.rs", "x\n");
    r.write("src/c.rs", "w\n");
    r.write("a.rs", "y\n");
    r.commit_all("init");
    r.write("src/b.rs", "x2\n");
    r.write("src/c.rs", "w2\n");
    r.write("a.rs", "y2\n");
    let mut app = app_on(&r);

    // Directories sort first; the cursor starts on the first file row.
    assert_eq!(app.diff_path.as_deref(), Some("src/b.rs"));

    app.next_file();
    assert_eq!(app.diff_path.as_deref(), Some("src/c.rs"));
    app.next_file();
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
    // The last file: no target, so nothing moves.
    app.next_file();
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));

    app.prev_file();
    assert_eq!(app.diff_path.as_deref(), Some("src/c.rs"));
    app.prev_file();
    assert_eq!(app.diff_path.as_deref(), Some("src/b.rs"));
    // The first file: the directory row above it is never landed on.
    app.prev_file();
    assert_eq!(app.diff_path.as_deref(), Some("src/b.rs"));

    // From a directory row, the skip finds the nearest file forward.
    app.file_cursor = app.file_rows.iter().position(|row| row.dir_path() == Some("src")).unwrap();
    app.next_file();
    assert_eq!(app.diff_path.as_deref(), Some("src/b.rs"));

    // And it works from the diff pane, where it opens the file without moving the focus.
    app.focus = Focus::Diff;
    app.next_file();
    assert_eq!(app.diff_path.as_deref(), Some("src/c.rs"));
    assert_eq!(app.focus, Focus::Diff);
    assert_eq!(selected_path(&app), Some("src/c.rs"), "the list selection follows the skip");
}

#[test]
fn file_skips_land_on_a_file_the_hunk_steps_pass_over() {
    let r = traversal_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.next_file();
    assert_eq!(app.diff_path.as_deref(), Some("bin.dat"), "the binary file is reachable");
    assert!(app.visible.is_empty(), "a notice diff has no rows");
    // A hunk step from a rowless notice arms, then crosses to the next file's hunk.
    app.next_hunk();
    app.next_hunk();
    assert_eq!(app.diff_path.as_deref(), Some("c.rs"));
    assert_eq!(cursor_text(&app), "EDIT THREE");
}

#[test]
fn traversals_step_from_the_open_file_not_a_parked_list_cursor() {
    use std::fmt::Write as _;
    let body = |n: usize| {
        (0..n).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "line {i}");
            s
        })
    };
    let r = Repo::init();
    r.write("src/a.rs", &body(30));
    r.write("src/z.rs", &body(10));
    r.commit_all("init");
    r.write(
        "src/a.rs",
        &body(30).replacen("line 2\n", "line 2\nEDIT ONE\n", 1).replacen(
            "line 25\n",
            "line 25\nEDIT TWO\n",
            1,
        ),
    );
    r.write("src/z.rs", &body(10).replacen("line 5\n", "line 5\nEDIT THREE\n", 1));
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.next_hunk();
    app.next_hunk();
    assert_eq!(cursor_text(&app), "EDIT TWO", "the diff sits on a.rs's last hunk");

    // Park the cursor on the directory above the open file.
    app.file_cursor = app.file_rows.iter().position(|row| row.dir_path() == Some("src")).unwrap();

    // Steps go from the open file, not the parked cursor.
    app.next_hunk();
    app.next_hunk();
    assert_eq!(app.diff_path.as_deref(), Some("src/z.rs"));
    assert_eq!(cursor_text(&app), "EDIT THREE");

    app.file_cursor = app.file_rows.iter().position(|row| row.dir_path() == Some("src")).unwrap();
    app.prev_file();
    assert_eq!(app.diff_path.as_deref(), Some("src/a.rs"), "the skip opens a file, never re-opens");
}

#[test]
fn a_live_selection_holds_both_traversals_still() {
    let r = traversal_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.next_hunk();
    app.toggle_select();
    let (path, cursor) = (app.diff_path.clone(), app.diff_cursor);

    app.next_hunk();
    app.next_file();
    assert_eq!(app.diff_path, path, "neither traversal drops the selection by opening a file");
    assert_eq!(app.diff_cursor, cursor, "nor moves the cursor out from under it");
}

#[test]
fn hunk_steps_are_inert_where_no_change_rows_are_painted() {
    let r = traversal_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    // `All files` renders whole-file content: every row is context, so a step has no target.
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    let (path, cursor) = (app.diff_path.clone(), app.diff_cursor);
    app.next_hunk();
    assert_eq!((app.diff_path.clone(), app.diff_cursor), (path, cursor));

    // The file skips still work there.
    app.next_file();
    assert_ne!(app.diff_path.as_deref(), Some("a.rs"));
}

#[test]
fn the_next_markdown_file_follows_m_on_the_previous_one() {
    let r = Repo::init();
    r.write("a.md", "# title\n");
    r.write("b.md", "# other\n");
    r.commit_all("init");
    r.write("a.md", "# title\n\nbody\n");
    r.write("b.md", "# other\n\nmore\n");
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    assert_eq!(app.diff_path.as_deref(), Some("a.md"));
    assert!(app.rendered_active(), "a markdown file opens rendered");
    app.toggle_rendered();
    assert!(!app.rendered_active(), "`m` flips it to source");

    // The choice is the pane's: the next file, and a return to the first, open as source.
    app.next_file();
    assert_eq!(app.diff_path.as_deref(), Some("b.md"));
    assert!(!app.rendered_active(), "the next markdown file opens as source");
    app.prev_file();
    assert_eq!(app.diff_path.as_deref(), Some("a.md"));
    assert!(!app.rendered_active(), "and so does the flipped one again");
}

#[test]
fn page_keys_move_the_cursor_in_both_panes() {
    let mut app = long_diff_app(40);
    // File pane: page moves the selection (not just the viewport).
    app.focus = Focus::Files;
    app.file_cursor = 0;
    app.reveal_files = false;
    app.move_cursor(5).unwrap();
    assert_eq!(app.file_cursor, 5usize.min(app.file_rows.len() - 1));
    assert!(app.reveal_files);
    // Diff pane: page moves the cursor.
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    app.reveal_diff = false;
    app.move_cursor(5).unwrap();
    assert_eq!(app.diff_cursor, 5);
    assert!(app.reveal_diff);
}

#[test]
fn horizontal_scroll_is_inert_while_wrapping() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.wrap = true;
    app.scroll_h(8);
    assert_eq!(app.h_scroll, 0, "h-scroll does nothing while wrap is on, so it can't accumulate");
    app.wrap = false;
    app.scroll_h(8);
    assert_eq!(app.h_scroll, 8, "h-scroll moves once wrap is off");
}

#[test]
fn a_poll_preserves_the_wheel_scroll_in_both_panes() {
    use std::fmt::Write as _;
    let r = Repo::init();
    let (mut old, mut new) = (String::new(), String::new());
    for i in 0..60 {
        let _ = writeln!(old, "line {i}");
        let _ = writeln!(new, "LINE {i}");
    }
    r.write("big.rs", &old);
    for i in 0..20 {
        r.write(&format!("f{i:02}.txt"), "one\n");
    }
    r.commit_all("init");
    r.write("big.rs", &new);
    for i in 0..20 {
        r.write(&format!("f{i:02}.txt"), "two\n");
    }
    let mut app = app_on(&r);

    // Open the long file and wheel its diff down; the cursor stays at the top.
    app.select_file(file_row(&app, "big.rs")).unwrap();
    app.focus = Focus::Diff;
    app.wheel_diff(20);
    let h = vec![1usize; app.visible.len()];
    app.bound_diff_scroll(&h, 10);
    let diff_scroll = app.diff_scroll;
    assert!(diff_scroll > 0);
    // Wheel the file list down too.
    app.wheel_files(8);
    app.bound_file_scroll(6);
    let file_scroll = app.file_scroll;
    assert!(file_scroll > 0);

    // An unchanged poll requests no reveal, so wheel scrolls stay.
    app.reveal_diff = false;
    app.reveal_files = false;
    app.reload().unwrap();
    assert!(!app.reveal_diff, "a poll does not reveal the diff cursor");
    assert!(!app.reveal_files, "a poll does not reveal the file cursor");
    let h = vec![1usize; app.visible.len()];
    app.bound_diff_scroll(&h, 10);
    app.bound_file_scroll(6);
    assert_eq!(app.diff_scroll, diff_scroll, "the diff wheel scroll survives the poll");
    assert_eq!(app.file_scroll, file_scroll, "the file-list wheel scroll survives the poll");
}

/// The index of the first diff row with the given marker (`'+'`, `'-'`, or `' '`).
fn row_with(app: &App, marker: char) -> usize {
    app.diff.rows.iter().position(|r| r.marker() == marker).expect("a row with that marker")
}

/// The visible file-list row index for `path`.
fn file_row(app: &App, path: &str) -> usize {
    app.file_rows
        .iter()
        .position(|r| r.file_index().is_some_and(|i| app.entries[i].path == path))
        .expect("a file row for the path")
}

#[test]
fn editing_a_comment_surfaces_its_file_from_a_collapsed_directory() {
    let r = Repo::init();
    r.write("src/foo.rs", "a\nb\nc\n");
    r.write("src/bar.rs", "x\n");
    r.write("root.rs", "1\n");
    r.commit_all("init");
    r.write("src/foo.rs", "a\nB\nc\n");
    r.write("src/bar.rs", "y\n");
    r.write("root.rs", "2\n");
    let mut app = app_on(&r);

    // Open src/foo.rs and comment on its changed line.
    app.select_file(file_row(&app, "src/foo.rs")).unwrap();
    comment_on(&mut app, '+', "note on foo");
    let commented_line = app.store.get(0).unwrap().start;

    // Switch the open diff to root.rs, then collapse `src` so foo's row is hidden.
    app.select_file(file_row(&app, "root.rs")).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("root.rs"));
    app.file_cursor = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    app.collapse_dir();
    assert!(
        !app.file_rows
            .iter()
            .any(|r| r.file_index().is_some_and(|i| app.entries[i].path == "src/foo.rs")),
        "foo's row is hidden under the collapsed src/"
    );

    // Editing from the list opens foo at its line, rowless as it is.
    app.open_list();
    app.start_edit();
    assert_eq!(app.diff_path.as_deref(), Some("src/foo.rs"), "edit surfaced the comment's file");
    let row = app.visible.get(app.diff_cursor).expect("cursor on a row");
    assert_eq!(row.new_no(), Some(commented_line), "cursor landed on the commented line");
    assert!(matches!(app.mode, Mode::Composing { editing: Some(_) }));
}

/// Expand the fold under the cursor with synthetic geometry (these tests don't render).
fn expand_fold(app: &mut App) {
    let heights = vec![1usize; app.visible.len()];
    app.expand_fold(&heights, 80);
}

/// Place the diff cursor on the first row with `marker` and write a comment there.
fn comment_on(app: &mut App, marker: char, text: &str) {
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(app, marker);
    app.start_comment();
    assert!(
        !app.footer_bands().iter().any(|&(a, _)| a == FooterAction::NavigatorPosition),
        "the composer owns its footer"
    );
    for ch in text.chars() {
        app.input_push(ch);
    }
    app.submit_comment();
}

/// An app sitting in the comment composer on the first changed line, caret at 0.
fn composing_app() -> App {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    app
}

#[test]
fn the_editor_inserts_and_deletes_at_the_caret() {
    let mut app = composing_app();
    typed(&mut app, "ac");
    assert_eq!((app.input.as_str(), app.caret), ("ac", 2));
    app.caret_left();
    app.input_push('b'); // insert mid-text, not at the end
    assert_eq!((app.input.as_str(), app.caret), ("abc", 2));
    app.input_backspace(); // deletes the char before the caret ('b')
    assert_eq!((app.input.as_str(), app.caret), ("ac", 1));
    app.input_delete_forward(); // deletes the char at the caret ('c')
    assert_eq!((app.input.as_str(), app.caret), ("a", 1));
}

#[test]
fn the_editor_moves_by_char_word_and_line() {
    let mut app = composing_app();
    typed(&mut app, "hello world");
    app.caret_home();
    assert_eq!(app.caret, 0);
    app.caret_end();
    assert_eq!(app.caret, 11);
    app.caret_word_left();
    assert_eq!(app.caret, 6, "to the start of 'world'");
    app.caret_word_left();
    assert_eq!(app.caret, 0, "to the start of 'hello'");
    app.caret_word_right();
    assert_eq!(app.caret, 5, "to the end of 'hello'");
}

#[test]
fn the_editor_kills_to_line_bounds_and_pastes_multiline() {
    let mut app = composing_app();
    typed(&mut app, "alpha beta");
    app.caret_home();
    app.caret_word_right(); // caret after "alpha"
    app.input_kill_to_end();
    assert_eq!(app.input, "alpha");
    app.input_kill_to_start();
    assert_eq!((app.input.as_str(), app.caret), ("", 0));
    // A multi-line paste lands as one unit with normalized newlines.
    app.input_paste("x\r\ny");
    assert_eq!((app.input.as_str(), app.caret), ("x\ny", 3));
}

#[test]
fn a_paste_outside_the_editor_is_ignored() {
    let r = edited_repo();
    let mut app = app_on(&r); // Normal mode, not composing
    app.input_paste("ignored");
    assert!(app.input.is_empty(), "paste does nothing outside the comment editor");
}

/// The primary (first) footer action for the current context.
fn primary(app: &App) -> FooterAction {
    app.footer_bands().first().expect("a footer action").0
}

#[test]
fn the_footer_offers_the_action_for_what_the_cursor_is_on() {
    let mut app = composing_app(); // diff focus, on a changed line, composer open
    app.cancel_comment(); // back to Normal, still on the changed line
    assert_eq!(primary(&app), FooterAction::Comment, "a diff line offers comment");

    app.toggle_select();
    assert_eq!(primary(&app), FooterAction::Comment, "a live selection still leads with comment");
    assert!(
        app.footer_bands().iter().any(|&(a, _)| a == FooterAction::ClearSelection),
        "and offers to clear the selection"
    );
    app.toggle_select();

    comment_on(&mut app, '+', "note");
    // The cursor now sits on the line it just commented.
    assert_eq!(primary(&app), FooterAction::EditComment, "a commented line offers edit");
    assert!(
        app.footer_bands().iter().any(|&(a, _)| a == FooterAction::Send),
        "a written comment surfaces send wherever the cursor is"
    );
}

#[test]
fn the_footer_offers_edit_file_wherever_the_key_opens_one() {
    // The repo has to outlive the app: the footer asks the worktree whether the file is there.
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    let has = |a: &App| a.footer_bands().iter().any(|&(x, _)| x == FooterAction::EditFile);
    assert!(has(&app), "an uncommented diff line offers edit file");

    comment_on(&mut app, '+', "note");
    assert_eq!(primary(&app), FooterAction::EditComment, "a commented line leads with the comment");
    assert!(!has(&app), "and never advertises the file, since the key edits the comment there");

    app.focus = Focus::Files;
    assert_eq!(
        app.footer_bands().iter().filter(|&&(x, _)| x == FooterAction::EditFile).count(),
        1,
        "a navigator file row offers it too, once"
    );
}

/// A rowless notice diff still offers `edit` for its file.
#[test]
fn the_footer_offers_edit_file_on_a_diff_with_no_rows() {
    let r = Repo::init();
    r.write("seed.rs", "x\n");
    r.commit_all("init");
    r.write("blob.bin", "\u{0}\u{1}\u{2}binary\u{0}\n");
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    assert_eq!(app.diff_path.as_deref(), Some("blob.bin"));
    assert!(app.visible.is_empty(), "a binary file paints no rows");
    assert!(
        app.footer_bands().iter().any(|&(x, _)| x == FooterAction::EditFile),
        "the file is there, so the footer offers the key"
    );

    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('e'));
    let request = app.editor_request.expect("and the press opens it");
    assert_eq!(request.path, "blob.bin");
    assert_eq!(request.line, 1, "at its start, since no row names a line");
}

/// `git rm --cached` calls a file deleted that is still on disk; the press asks the disk.
#[test]
fn edit_reaches_a_file_the_changeset_only_calls_deleted() {
    let r = Repo::init();
    r.write("here.rs", "one\ntwo\n");
    r.commit_all("init");
    r.git(&["rm", "--cached", "-q", "here.rs"]);
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    assert_eq!(app.diff_path.as_deref(), Some("here.rs"), "the changeset names it");
    assert!(
        app.entries.iter().any(|e| {
            e.path == "here.rs"
                && e.annotation.as_ref().map(|a| a.kind)
                    == Some(herdr_reviewr::model::ChangeKind::Deleted)
        }),
        "and calls it deleted"
    );
    assert!(r.path().join("here.rs").is_file(), "but the file is right there");

    assert!(
        app.footer_bands().iter().any(|&(x, _)| x == FooterAction::EditFile),
        "so the footer offers the key"
    );
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('e'));
    assert_eq!(
        app.editor_request.as_ref().map(|t| t.path.as_str()),
        Some("here.rs"),
        "and the press names it"
    );
}

#[test]
fn edit_opens_the_file_under_the_cursor_and_the_comment_when_one_is_there() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;

    // `e` on an uncommented line names the worktree file, at that line, absolutely.
    press(&mut app, &keymap, KeyCode::Char('e'));
    let request = app.editor_request.take().expect("`e` names the file under the cursor");
    assert_eq!(request.path, "a.rs");
    assert!(!app.composing(), "and no comment box opens");

    // The same key on a commented line edits the comment instead.
    comment_on(&mut app, '+', "note");
    press(&mut app, &keymap, KeyCode::Char('e'));
    assert!(app.composing(), "the comment under the cursor claims the key");
    assert!(app.editor_request.is_none(), "so no file is requested");
    app.cancel_comment();

    // A directory row names nothing.
    app.focus = Focus::Files;
    press(&mut app, &keymap, KeyCode::Char('e'));
    let from_row = app.editor_request.take().expect("a file row names its file");
    assert_eq!(from_row.path, "a.rs");
    assert_eq!(from_row.line, 1, "a navigator row names no line, so the file opens at its start");

    // `All files` builds and anchors differently, so check it too.
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    app.focus = Focus::Diff;
    app.diff_cursor = 2;
    press(&mut app, &keymap, KeyCode::Char('e'));
    let in_file_view = app.editor_request.take().expect("the File view names its file too");
    assert_eq!(in_file_view.path, "a.rs");
    assert_eq!(in_file_view.line, 3, "at the line the cursor is on, not the file's start");

    comment_on(&mut app, ' ', "content note");
    press(&mut app, &keymap, KeyCode::Char('e'));
    assert!(app.composing(), "and a content comment claims the key the same way");
    assert!(app.editor_request.is_none());
}

#[test]
fn edit_is_inert_while_a_line_selection_is_live() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let keymap = Keymap::default();

    app.toggle_select();
    assert!(app.select_anchor.is_some(), "v starts a selection");
    press(&mut app, &keymap, KeyCode::Char('e'));
    assert!(app.editor_request.is_none(), "a press cannot abandon the range being selected");
    assert!(app.select_anchor.is_some(), "and the selection survives it");
    assert!(
        !app.footer_bands().iter().any(|&(a, _)| a == FooterAction::EditFile),
        "the footer never names a key that would not work here"
    );

    app.clear_selection();
    press(&mut app, &keymap, KeyCode::Char('e'));
    assert!(app.editor_request.is_some(), "and works again once the selection is cleared");
}

#[test]
fn a_rebound_edit_key_carries_the_file_editor_with_it() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let keymap = Keymap::resolve(&[(Action::Edit, vec![Key::plain('E')])]).unwrap();

    press(&mut app, &keymap, KeyCode::Char('e'));
    assert!(app.editor_request.is_none(), "the default key no longer acts");
    press(&mut app, &keymap, KeyCode::Char('E'));
    assert!(app.editor_request.is_some(), "one action moves both meanings together");
}

/// The whole point of the key: the edit you just made is on screen when you come back.
#[test]
fn the_diff_shows_an_edit_made_in_the_editor_after_the_refresh() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let keymap = Keymap::default();

    press(&mut app, &keymap, KeyCode::Char('e'));
    let target = app.editor_request.take().expect("`e` names the open file");

    // Stand in for the editor: write the file the way it was handed to one.
    let on_disk = r.path().join(&target.path);
    let body = std::fs::read_to_string(&on_disk).unwrap();
    std::fs::write(&on_disk, format!("{body}sixth-line-from-the-editor\n")).unwrap();

    let text = |a: &App| {
        a.visible
            .iter()
            .flat_map(|row| row.spans().iter().map(|s| s.text.clone()))
            .collect::<String>()
    };
    assert!(!text(&app).contains("sixth-line-from-the-editor"), "not on screen before the refresh");

    // `reload` reconciles as the editor's worker refresh does.
    app.reload().unwrap();
    assert!(
        text(&app).contains("sixth-line-from-the-editor"),
        "the open diff rebuilds its content, not only the file list"
    );
}

#[test]
fn esc_clears_a_live_selection() {
    let mut app = composing_app();
    app.cancel_comment();
    app.toggle_select();
    assert!(app.select_anchor.is_some(), "v starts a selection");
    app.clear_selection();
    assert!(app.select_anchor.is_none(), "esc clears the selection");
}

#[test]
fn the_footer_offers_scope_everywhere_on_a_file_tab() {
    let mut app = composing_app();
    app.cancel_comment(); // diff focus, on a content line
    let has_scope = |a: &App| a.footer_bands().iter().any(|&(x, _)| x == FooterAction::Scope);
    assert!(has_scope(&app), "scope shows while reviewing a diff line");
    app.focus = Focus::Files;
    assert!(has_scope(&app), "scope shows in the file list too");
}

#[test]
fn the_pr_footer_offers_open_for_any_resolved_pr() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{PrSnapshot, PrView};

    let r = edited_repo();
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    // No resolved PR (still loading): nothing to open.
    assert!(
        !app.footer_bands().iter().any(|&(a, _)| a == FooterAction::OpenPr),
        "no resolved PR → no open action"
    );

    // A resolved PR with zero comments still offers `o open` — `o` opens the PR URL, not a comment.
    app.pr = PrView::Pr(Box::new(PrSnapshot { number: 7, ..common::pr_snapshot() }));
    assert!(app.pr_selected_comment().is_none(), "zero comments → nothing selected");
    assert_eq!(
        app.footer_bands().first().map(|&(a, _)| a),
        Some(FooterAction::OpenPr),
        "a resolved PR offers open even with no comments"
    );
}

#[test]
fn opening_the_pr_refuses_a_url_that_is_not_http() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{PrSnapshot, PrView};

    let r = edited_repo();
    let mut app = app_on(&r);
    // An opener that is nowhere, so an accepted URL reports reaching it and opens nothing.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "url_opener = \"reviewr-no-such-opener\"\n")
        .unwrap();
    app.set_plugin_config(herdr_reviewr::config::plugin_config_in(dir.path()).unwrap());
    app.set_tab(Tab::Pr).unwrap();
    let with_url =
        |url: &str| PrView::Pr(Box::new(PrSnapshot { url: url.into(), ..common::pr_snapshot() }));

    app.pr = with_url("file:///C:/Windows/System32/calc.exe");
    app.pr_open();
    assert_eq!(app.status, "unsupported link scheme");

    app.pr = with_url("https://github.com/o/r/pull/7");
    app.pr_open();
    assert_eq!(app.status, "`url_opener` not found: reviewr-no-such-opener");
}

#[test]
fn the_footer_offers_send_only_once_a_comment_exists() {
    let mut app = composing_app();
    app.cancel_comment();
    assert!(
        !app.footer_bands().iter().any(|&(a, _)| a == FooterAction::Send),
        "no comments yet → no send action"
    );
    comment_on(&mut app, '+', "note");
    assert!(
        app.footer_bands().iter().any(|&(a, _)| a == FooterAction::Send),
        "a comment written → send appears"
    );
}

#[test]
fn the_expansion_toggles_from_normal_and_survives_a_poll() {
    let mut app = composing_app();
    app.cancel_comment(); // Normal mode, diff focus
    let keymap = Keymap::default();
    assert!(!app.keys_expanded, "the footer opens collapsed");
    press(&mut app, &keymap, KeyCode::Char('?'));
    assert!(app.keys_expanded, "`?` opens the expansion");
    // A poll re-derives the footer's content but never moves the toggle (Continuity).
    common::land_world(&mut app);
    assert!(app.keys_expanded, "a refresh keeps the expansion open");
    press(&mut app, &keymap, KeyCode::Char('?'));
    assert!(!app.keys_expanded, "`?` again collapses it");
}

#[test]
fn the_expansion_is_inert_in_the_comments_list() {
    let mut app = composing_app();
    app.cancel_comment();
    comment_on(&mut app, '+', "note");
    let keymap = Keymap::default();
    app.open_list();
    assert_eq!(app.mode, Mode::List);
    press(&mut app, &keymap, KeyCode::Char('?'));
    assert!(!app.keys_expanded, "`?` is inert while the comments list owns the bar");
}

#[test]
fn the_keys_char_is_text_in_the_comment_editor() {
    // While composing, `?` types itself.
    let mut app = composing_app(); // composing
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('?'));
    assert!(app.input.contains('?'), "`?` types into the comment editor");
    assert!(!app.keys_expanded, "and never toggles the footer expansion");
}

#[test]
fn esc_peels_one_layer_per_press() {
    let r = traversal_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;

    // One `esc` per layer: the selection, then the expansion.
    app.keys_expanded = true;
    app.toggle_select();
    assert!(app.select_anchor.is_some(), "v starts a selection");
    press(&mut app, &keymap, KeyCode::Esc);
    assert!(app.select_anchor.is_none(), "the first esc clears the selection");
    assert!(app.keys_expanded, "and leaves the expansion open");
    press(&mut app, &keymap, KeyCode::Esc);
    assert!(!app.keys_expanded, "the next esc closes the expansion");

    // An armed crossing is the middle rung: esc drops the arm before the expansion.
    app.keys_expanded = true;
    app.next_hunk();
    app.next_hunk();
    press(&mut app, &keymap, KeyCode::Char(']')); // arm at the file's last hunk
    assert_eq!(app.armed_cross(), Some(true));
    press(&mut app, &keymap, KeyCode::Esc);
    assert_eq!(app.armed_cross(), None, "esc drops the armed crossing");
    assert!(app.keys_expanded, "and leaves the expansion open");
    press(&mut app, &keymap, KeyCode::Esc);
    assert!(!app.keys_expanded, "the next esc closes the expansion");
}

#[test]
fn esc_on_pr_closes_the_expansion_and_spares_the_frozen_file_tab() {
    use herdr_reviewr::app::Tab;
    let r = traversal_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();

    // Select on a file tab and open the expansion, then move to PR — the selection freezes in place.
    app.focus = Focus::Diff;
    app.toggle_select();
    assert!(app.select_anchor.is_some(), "v starts a selection on the file tab");
    app.keys_expanded = true;
    app.set_tab(Tab::Pr).unwrap();

    // `esc` on PR closes the expansion and never disturbs the frozen file-tab selection.
    press(&mut app, &keymap, KeyCode::Esc);
    assert!(!app.keys_expanded, "esc closes the expansion on PR");
    assert!(app.select_anchor.is_some(), "the frozen file-tab selection is spared");
}

#[test]
fn the_pr_move_band_drops_the_hunk_and_file_steps() {
    use herdr_reviewr::app::Tab;
    let r = edited_repo();
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    let acts: Vec<_> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
    assert!(acts.contains(&FooterAction::MoveLine), "the PR moves line by line");
    assert!(acts.contains(&FooterAction::MovePage), "and pages the read pane");
    assert!(!acts.contains(&FooterAction::MoveHunk), "the PR has no hunk step");
    assert!(!acts.contains(&FooterAction::MoveFile), "and no file step");
}

#[test]
fn the_all_files_move_band_drops_the_inert_hunk_step() {
    use herdr_reviewr::app::Tab;
    // `All files` has no hunk steps, so `move` doesn't list them.
    let r = edited_repo();
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let acts: Vec<_> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
    assert!(acts.contains(&FooterAction::MoveFile), "file stepping still works on All files");
    assert!(!acts.contains(&FooterAction::MoveHunk), "but hunk stepping is inert there");
}

#[test]
fn the_go_band_never_repeats_the_empty_scope_primary() {
    // Empty, row 1 leads with scopes, so `go` doesn't repeat it.
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("c"); // nothing uncommitted → the empty state
    let app = app_on(&r);
    let bands = app.footer_bands();
    assert_eq!(
        bands.first().map(|&(a, _)| a),
        Some(FooterAction::ScopeOther),
        "the other scopes lead the empty state"
    );
    let scopes = bands
        .iter()
        .filter(|&&(a, _)| matches!(a, FooterAction::Scope | FooterAction::ScopeOther))
        .count();
    assert_eq!(scopes, 1, "scope is not repeated in the go band");
}

/// `big.rs`: 40 lines, one change in the middle, both ends folded.
fn folded_repo() -> Repo {
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut old = String::new();
    for i in 0..40 {
        writeln!(old, "line {i}").unwrap();
    }
    r.write("big.rs", &old);
    r.commit_all("init");
    r.write("big.rs", &old.replace("line 20", "LINE 20"));
    r
}

#[test]
fn a_fold_expands_permanently_and_keeps_the_cursor_in_range() {
    let r = folded_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let folded = app.visible.len();
    assert!(app.visible.iter().any(|row| row.hidden() > 0), "opens folded");

    // Land on the leading fold and expand it (the `→` action) — the visible count grows.
    app.diff_cursor = app.visible.iter().position(|row| row.hidden() > 0).unwrap();
    assert!(app.on_fold(), "`→` expands here");
    expand_fold(&mut app);
    let expanded = app.visible.len();
    assert!(expanded > folded, "expanding reveals the hidden lines");
    assert!(app.diff_cursor < app.visible.len(), "cursor stays in range");
    assert!(!app.on_fold(), "the fold is gone, so `→` now scrolls instead");

    // Expansion is permanent — pressing again on a revealed content line does nothing.
    expand_fold(&mut app);
    assert_eq!(app.visible.len(), expanded, "no collapse-back");
}

#[test]
fn a_selection_cannot_cross_a_fold() {
    let r = folded_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    // Anchor just above the trailing fold, then try to select well past it.
    let tail = app.visible.iter().rposition(|row| row.hidden() > 0).unwrap();
    app.diff_cursor = tail - 1;
    app.toggle_select();
    app.move_cursor(10).unwrap();
    assert_eq!(app.diff_cursor, tail - 1, "the cursor stops shy of the trailing fold");
    let (lo, hi) = app.selection_range();
    assert!((lo..=hi).all(|i| app.visible[i].is_content()), "no fold row is in the selection");

    // The same upward, across the leading fold.
    let head = app.visible.iter().position(|row| row.hidden() > 0).unwrap();
    app.select_anchor = None;
    app.diff_cursor = head + 1;
    app.toggle_select();
    app.move_cursor(-10).unwrap();
    assert_eq!(app.diff_cursor, head + 1, "the cursor stops just after the leading fold");
}

#[test]
fn paging_the_diff_cannot_cross_a_fold() {
    let r = folded_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let tail = app.visible.iter().rposition(|row| row.hidden() > 0).unwrap();
    app.diff_cursor = tail - 1;
    app.toggle_select();
    app.move_cursor(50).unwrap(); // a big page that would jump well past the trailing fold
    assert_eq!(app.diff_cursor, tail - 1, "page stops shy of the fold while selecting");
}

#[test]
fn expanding_a_fold_does_not_bleed_into_another_file() {
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut body = String::new();
    for i in 0..40 {
        let _ = writeln!(body, "line {i}");
    }
    r.write("a.rs", &body);
    r.write("b.rs", &body);
    r.commit_all("init");
    r.write("a.rs", &body.replace("line 20", "A20"));
    r.write("b.rs", &body.replace("line 20", "B20"));
    let mut app = app_on(&r); // a.rs opens first (sorted)

    // Expand a.rs's leading fold (its anchor is line 1, same as b.rs's leading fold).
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|row| row.hidden() > 0).unwrap();
    expand_fold(&mut app);
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));

    // Switching to b.rs must not carry a.rs's expansion across (shared line-number key).
    app.focus = Focus::Files;
    app.move_cursor(1).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"));
    assert!(app.visible[0].hidden() > 0, "b.rs's leading fold stays collapsed");
}

#[test]
fn expanding_a_fold_keeps_the_viewport_still() {
    let r = folded_repo();

    // A fold in the top half grows upward: scroll advances so the lines below hold position.
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let head = app.visible.iter().position(|row| row.hidden() > 0).unwrap();
    let shift = app.visible[head].hidden() - 1;
    app.diff_cursor = head;
    app.diff_scroll = 0;
    let heights = vec![1usize; app.visible.len()];
    app.expand_fold(&heights, 20);
    assert_eq!(app.diff_scroll, shift, "top-half fold grows upward");

    // A fold in the bottom half grows downward: scroll holds so the lines above stay put.
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let tail = app.visible.iter().rposition(|row| row.hidden() > 0).unwrap();
    app.diff_cursor = tail;
    app.diff_scroll = 0;
    let heights = vec![1usize; app.visible.len()];
    app.expand_fold(&heights, tail + 2); // the fold sits in the bottom half of the viewport
    assert_eq!(app.diff_scroll, 0, "bottom-half fold grows downward");
}

#[test]
fn a_comment_through_a_fold_anchors_to_gits_line_and_survives_a_poll() {
    let r = folded_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    // Comment on the changed line (new-side 21) while the rest is folded.
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("LINE 20")).unwrap();
    app.start_comment();
    for ch in "here".chars() {
        app.input_push(ch);
    }
    app.submit_comment();
    let c = app.store.iter().next().unwrap();
    assert_eq!((c.side, c.start), (Side::New, 21));

    // A fold expand plus a poll keeps the comment.
    app.diff_cursor = app.visible.iter().position(|row| row.hidden() > 0).unwrap();
    expand_fold(&mut app);
    app.reload().unwrap();
    assert_eq!(app.store.len(), 1, "the comment survives a fold expand and a poll");
    assert!(app.comment_marks().1.iter().any(|&i| app.visible[i].text().contains("LINE 20")));
}

#[test]
fn comment_anchors_to_gits_real_line_numbers() {
    // `edited_repo`: a.rs has beta→BETA on line 2 and epsilon appended as new line 5.
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    app.diff_cursor = app.diff.rows.iter().position(|r| r.text().contains("epsilon")).unwrap();
    app.start_comment();
    for ch in "appended".chars() {
        app.input_push(ch);
    }
    app.submit_comment();

    app.diff_cursor =
        app.diff.rows.iter().position(|r| r.marker() == '-' && r.text().contains("beta")).unwrap();
    app.start_comment();
    for ch in "removed".chars() {
        app.input_push(ch);
    }
    app.submit_comment();

    let appended = app.store.iter().find(|c| c.text == "appended").unwrap();
    assert_eq!((appended.side, appended.start, appended.end), (Side::New, 5, 5));
    let removed = app.store.iter().find(|c| c.text == "removed").unwrap();
    assert_eq!((removed.side, removed.start, removed.end), (Side::Old, 2, 2));
}

#[test]
fn comments_on_added_and_removed_lines_capture_the_snippet() {
    let r = edited_repo();
    let mut app = app_on(&r);
    assert_eq!(app.entries.len(), 1);

    comment_on(&mut app, '+', "this addition needs a test");
    comment_on(&mut app, '-', "why was this dropped?");
    assert_eq!(app.store.len(), 2);

    let removed = app
        .store
        .iter()
        .find(|c| c.location().ends_with("(removed)"))
        .expect("a removed-side comment");
    assert!(removed.lines.starts_with('-'), "snippet keeps the diff marker: {:?}", removed.lines);

    let added = app
        .store
        .iter()
        .find(|c| !c.location().ends_with("(removed)"))
        .expect("a new-side comment");
    assert!(added.lines.starts_with('+'));
}

#[test]
fn a_saved_comment_survives_a_refresh() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "keep me");
    assert_eq!(app.store.len(), 1);

    r.write("b.rs", "another change\n"); // the world moves on
    app.reload().unwrap();

    assert_eq!(app.store.len(), 1, "refresh must not drop a saved comment");
    assert_eq!(app.store.iter().next().unwrap().text, "keep me");
    assert!(app.entries.iter().any(|f| f.path == "b.rs"), "file list still refreshed");
}

#[test]
fn a_refresh_while_composing_freezes_input_and_diff() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    for ch in "half-written thought".chars() {
        app.input_push(ch);
    }
    let frozen_diff = app.diff.clone();

    r.write("a.rs", "alpha\nBETA\ngamma\ndelta\nepsilon\nzeta\n"); // diff shifts under us
    r.write("c.rs", "c\n");
    app.reload().unwrap();

    assert!(app.composing(), "still composing");
    assert_eq!(app.input, "half-written thought", "input untouched");
    assert_eq!(app.diff, frozen_diff, "the open diff is frozen while composing");
    assert!(app.entries.iter().any(|f| f.path == "c.rs"), "file list still refreshes");
}

#[test]
fn a_failed_export_keeps_comments_and_success_consumes_them() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "one");
    comment_on(&mut app, '-', "two");
    assert_eq!(app.store.len(), 2);

    app.export(&FakeTarget::failing());
    assert_eq!(app.store.len(), 2, "a failed export leaves every comment in place");

    let target = FakeTarget::ok();
    app.export(&target);
    assert!(app.store.is_empty(), "a successful export consumes the comments");
    assert_eq!(app.status, "exported 2 comments", "the target owns the success confirmation");

    // The sent text is the real export block format, end to end through App::export.
    let sent = target.last();
    assert!(sent.contains("one") && sent.contains("two"), "both comment texts present: {sent:?}");
    assert!(sent.lines().next().is_some_and(|l| l.starts_with("a.rs:")), "leads with a location");
    assert!(sent.contains("\n\n"), "blocks separated by a blank line: {sent:?}");
    assert!(
        sent.lines().any(|l| l.starts_with('+') || l.starts_with('-')),
        "each block carries its diff snippet: {sent:?}"
    );
}

#[test]
fn send_consumes_the_whole_set() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "first");
    comment_on(&mut app, '-', "second");

    app.export(&FakeTarget::ok());
    assert!(app.store.is_empty(), "send takes every comment, not just one");
}

#[test]
fn a_comment_of_only_blank_lines_is_cancelled() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    app.input_push(' ');
    app.input_push('\n');
    app.input_push('\n');
    app.submit_comment();

    assert!(app.store.is_empty(), "a whitespace-only comment is not saved");
    assert!(!app.composing(), "compose mode exits");
}

#[test]
fn the_composer_reserve_keeps_the_anchored_line_visible() {
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut original = String::new();
    for i in 0..60 {
        writeln!(original, "line {i}").unwrap();
    }
    r.write("big.rs", &original);
    r.commit_all("init");
    r.write("big.rs", &original.replace("line", "LINE"));

    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = 30;
    app.start_comment();
    for ch in "one\ntwo\nthree".chars() {
        app.input_push(ch);
    }

    // Reserve the box's rows; the anchor stays above it.
    let viewport = 12;
    let effective = viewport - herdr_reviewr::ui::composer_height(&app, 80);
    clamp(&mut app, effective);
    assert!(
        (app.diff_scroll..app.diff_scroll + effective).contains(&app.diff_cursor),
        "anchored line {} stays in the reserved viewport [{}, {})",
        app.diff_cursor,
        app.diff_scroll,
        app.diff_scroll + effective
    );
}

#[test]
fn a_comment_can_be_written_across_multiple_lines() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    for ch in "first line".chars() {
        app.input_push(ch);
    }
    app.input_push('\n'); // Alt/Shift+Enter inserts a newline
    for ch in "second line".chars() {
        app.input_push(ch);
    }
    app.submit_comment();

    let c = app.store.iter().next().unwrap();
    assert_eq!(c.text, "first line\nsecond line", "the body keeps its line break");

    let target = FakeTarget::ok();
    app.export(&target);
    let sent = target.last();
    assert!(sent.contains("first line\nsecond line"), "export preserves the break: {sent:?}");
    assert!(!sent.contains("\n\n\n"), "no blank-line run that could split a block");
}

#[test]
fn the_cursor_stays_on_a_folder_across_a_poll_and_toggle() {
    let r = Repo::init();
    r.write("src/a.rs", "x\n");
    r.write("src/b.rs", "y\n");
    r.write("root.rs", "z\n");
    r.commit_all("init");
    r.write("src/a.rs", "x2\n");
    r.write("src/b.rs", "y2\n"); // two changed files keep `src/` an expandable directory
    r.write("root.rs", "z2\n");
    let mut app = app_on(&r);
    app.focus = Focus::Files;

    // Land the cursor on the `src` directory row; the open diff is some file.
    let dir_row = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    app.file_cursor = dir_row;
    let open = app.diff_path.clone();
    assert!(open.is_some(), "a file diff is open");

    // A poll must not yank the cursor onto the open file, nor blank the diff.
    app.reload().unwrap();
    assert_eq!(app.file_cursor, dir_row, "cursor stays on the folder across a poll");
    assert_eq!(app.diff_path, open, "the open diff is unchanged");

    // Collapsing then a poll keeps the cursor on the (now collapsed) folder.
    app.collapse_dir();
    app.reload().unwrap();
    let dir_row = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    assert_eq!(app.file_cursor, dir_row, "cursor stays on the folder after collapse + poll");
    assert_eq!(app.diff_path, open, "the open diff is still unchanged");
}

/// A repo with a `src/` directory of two edited files, cursor parked on the directory row.
fn folder_repo() -> Repo {
    let r = Repo::init();
    r.write("src/a.rs", "x\n");
    r.write("src/b.rs", "y\n");
    r.commit_all("init");
    r.write("src/a.rs", "x2\n");
    r.write("src/b.rs", "y2\n");
    r
}

#[test]
fn default_arrows_fold_a_folder_and_scroll_the_diff_elsewhere() {
    let r = folder_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Files;

    let dir_row = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    app.file_cursor = dir_row;
    assert!(app.on_folder(), "the cursor is on the folder");
    let expanded = app.file_rows.len();

    press(&mut app, &keymap, KeyCode::Left);
    assert!(app.file_rows.len() < expanded, "`←` collapses the folder");
    assert!(app.on_folder(), "the cursor stays on the folder row");

    press(&mut app, &keymap, KeyCode::Right);
    assert_eq!(app.file_rows.len(), expanded, "`→` expands it again");

    // Off the collapsible, the same keys scroll the open diff sideways (wrap off).
    app.file_cursor = app.file_rows.iter().position(|r| r.dir_path().is_none()).unwrap();
    app.wrap = false;
    assert!(!app.on_folder());
    press(&mut app, &keymap, KeyCode::Right);
    assert_eq!(app.h_scroll, 8, "`→` scrolls the diff right");
    press(&mut app, &keymap, KeyCode::Left);
    assert_eq!(app.h_scroll, 0, "`←` scrolls it back");
}

#[test]
fn expand_rebinds_to_a_character_and_the_freed_arrow_goes_dead() {
    let r = folder_repo();
    let mut app = app_on(&r);
    // The vim shape: `l`/`h` fold, `comments` moves off `l` to make room.
    let keymap = Keymap::resolve(&[
        (Action::Expand, vec![Key::plain('l')]),
        (Action::Collapse, vec![Key::plain('h')]),
        (Action::Comments, vec![Key::plain('L')]),
    ])
    .unwrap();
    app.focus = Focus::Files;
    app.file_cursor = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    let expanded = app.file_rows.len();

    press(&mut app, &keymap, KeyCode::Char('h'));
    assert!(app.file_rows.len() < expanded, "the bound `h` collapses the folder");
    press(&mut app, &keymap, KeyCode::Char('l'));
    assert_eq!(app.file_rows.len(), expanded, "the bound `l` expands it");

    // The named-key defaults were replaced, so the arrows answer nothing anywhere.
    press(&mut app, &keymap, KeyCode::Left);
    assert_eq!(app.file_rows.len(), expanded, "the freed `←` no longer folds");
    app.file_cursor = app.file_rows.iter().position(|r| r.dir_path().is_none()).unwrap();
    press(&mut app, &keymap, KeyCode::Right);
    assert_eq!(app.h_scroll, 0, "the freed `→` no longer scrolls");
}

#[test]
fn page_down_rebinds_and_half_page_defaults_hold() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = 0;

    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::PageDown);
    let paged = app.diff_cursor;
    assert!(paged > 0, "`PageDown` moves the cursor by default");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        Rect::new(0, 0, 120, 40),
        &keymap,
    )
    .unwrap();
    assert!(app.diff_cursor < paged, "`ctrl+u` half-pages back up");

    let keymap = Keymap::resolve(&[(
        Action::PageDown,
        vec![Key { ctrl: true, alt: false, code: BindingCode::Char('n') }],
    )])
    .unwrap();
    app.diff_cursor = 0;
    press(&mut app, &keymap, KeyCode::PageDown);
    assert_eq!(app.diff_cursor, 0, "the freed `PageDown` answers nothing");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        Rect::new(0, 0, 120, 40),
        &keymap,
    )
    .unwrap();
    assert!(app.diff_cursor > 0, "the rebound chord pages down");
}

#[test]
fn divider_drag_math_and_keyboard_clamps_follow_all_four_positions() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let area = Rect::new(0, 0, 100, 102); // a 100-cell split axis in either direction
    let body = herdr_reviewr::ui::body_rect(area, &app);
    let heights = vec![1usize; app.visible.len()];
    let keymap = Keymap::default();
    let event = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };

    for (position, target_column, target_row) in [
        (NavigatorPosition::Right, body.x + 60, body.y + 50),
        (NavigatorPosition::Left, body.x + 40, body.y + 50),
        (NavigatorPosition::Bottom, body.x + 50, body.y + 60),
        (NavigatorPosition::Top, body.x + 50, body.y + 40),
    ] {
        app.navigator_position = position;
        app.navigator_side_pct = 32;
        app.navigator_stack_pct = 25;
        let divider = (body.y..body.y + body.height)
            .flat_map(|row| (body.x..body.x + body.width).map(move |column| (column, row)))
            .find(|&(column, row)| herdr_reviewr::ui::hit_divider(area, &app, column, row))
            .unwrap();
        handle_mouse(
            &mut app,
            event(MouseEventKind::Down(MouseButton::Left), divider.0, divider.1),
            area,
            &heights,
            &keymap,
            &herdr_reviewr::export::Clipboard,
        )
        .unwrap();
        handle_mouse(
            &mut app,
            event(MouseEventKind::Drag(MouseButton::Left), target_column, target_row),
            area,
            &heights,
            &keymap,
            &herdr_reviewr::export::Clipboard,
        )
        .unwrap();
        handle_mouse(
            &mut app,
            event(MouseEventKind::Up(MouseButton::Left), target_column, target_row),
            area,
            &heights,
            &keymap,
            &herdr_reviewr::export::Clipboard,
        )
        .unwrap();
        assert_eq!(app.navigator_share(), 40, "event-level drag math for {position:?}");
        assert!(!app.divider_drag_active());
        assert!(!app.divider_drag_cancelled(), "mouse-up releases capture for {position:?}");
        if position.stacked() {
            assert_eq!(app.navigator_side_pct, 32, "stacked drag leaves side share alone");
        } else {
            assert_eq!(app.navigator_stack_pct, 25, "side drag leaves stacked share alone");
        }

        for _ in 0..20 {
            app.resize_navigator(4);
        }
        assert_eq!(
            app.navigator_share(),
            if position.stacked() { 50 } else { 60 },
            "maximum for {position:?}"
        );
        for _ in 0..20 {
            app.resize_navigator(-4);
        }
        assert_eq!(app.navigator_share(), 15, "minimum for {position:?}");
    }

    // Even direct state mutation cannot reinterpret a captured drag on another axis.
    app.navigator_position = NavigatorPosition::Right;
    app.navigator_side_pct = 32;
    app.navigator_stack_pct = 25;
    app.start_divider_drag();
    app.navigator_position = NavigatorPosition::Bottom;
    app.drag_divider(100, 60);
    assert_eq!(app.navigator_side_pct, 32);
    assert_eq!(app.navigator_stack_pct, 25);
    assert!(app.divider_drag_cancelled());
}

#[test]
fn navigator_actions_cycle_remember_shares_and_respect_modes() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.diff_scroll = 1;
    let cursor = app.diff_cursor;

    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.navigator_position, NavigatorPosition::Bottom);
    assert_eq!(app.focus, Focus::Diff);
    assert_eq!(app.diff_cursor, cursor);
    assert_eq!(app.diff_scroll, 1);

    press(&mut app, &keymap, KeyCode::Char('<'));
    assert_eq!(app.navigator_stack_pct, 29);
    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.navigator_position, NavigatorPosition::Left);
    assert_eq!(app.navigator_side_pct, 32, "switching axis restores the side share");
    press(&mut app, &keymap, KeyCode::Char('<'));
    assert_eq!(app.navigator_side_pct, 36);
    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.navigator_position, NavigatorPosition::Top);
    assert_eq!(app.navigator_stack_pct, 29, "the stacked share is remembered");
    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.navigator_position, NavigatorPosition::Right);
    assert_eq!(app.navigator_side_pct, 36, "the side share is remembered");

    app.start_comment();
    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.input, "p", "the position key is text in the composer");
    assert_eq!(app.navigator_position, NavigatorPosition::Right);
    app.cancel_comment();

    app.mode = Mode::List;
    assert!(
        !app.footer_bands().iter().any(|&(a, _)| a == FooterAction::NavigatorPosition),
        "the comments list owns its footer"
    );
    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.navigator_position, NavigatorPosition::Right, "the action is inert in list");
    app.mode = Mode::Normal;
    app.set_tab(herdr_reviewr::app::Tab::Pr).unwrap();
    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.navigator_position, NavigatorPosition::Bottom, "the action works on PR");
    assert!(
        app.footer_bands().iter().any(|&(a, _)| a == FooterAction::NavigatorPosition),
        "the PR footer exposes the position action"
    );
}

#[test]
fn navigator_hide_toggles_full_width_and_respects_modes() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 120, 40);
    app.focus = Focus::Files;

    // Hide: focus pins to the read pane, the layout keys go inert, no divider remains.
    press(&mut app, &keymap, KeyCode::Char('z'));
    assert!(app.navigator_hidden);
    assert_eq!(app.focus, Focus::Diff, "hiding moves focus to the read pane");
    press(&mut app, &keymap, KeyCode::Char('p'));
    press(&mut app, &keymap, KeyCode::Char('<'));
    assert_eq!(app.navigator_position, NavigatorPosition::Right, "`p` is inert while hidden");
    assert_eq!(app.navigator_side_pct, 32, "`<` is inert while hidden");
    let body = herdr_reviewr::ui::body_rect(area, &app);
    let row = body.y + body.height / 2;
    assert!(
        (body.x..body.x + body.width)
            .all(|col| !herdr_reviewr::ui::hit_divider(area, &app, col, row)),
        "no divider exists while hidden"
    );

    // Show: the same key, kept position and share, focus staying on the read pane.
    app.reveal_files = false;
    press(&mut app, &keymap, KeyCode::Char('z'));
    assert!(!app.navigator_hidden);
    assert_eq!(app.focus, Focus::Diff, "showing leaves focus on the read pane");
    assert!(app.reveal_files, "showing re-reveals the cursor at the real viewport");

    press(&mut app, &keymap, KeyCode::Char('z'));
    press(&mut app, &keymap, KeyCode::Tab);
    assert!(!app.navigator_hidden, "`tab` shows the navigator");
    assert_eq!(app.focus, Focus::Files, "and focuses it");

    // The composer and the search input take `z` as text; the comments list keeps it inert.
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    press(&mut app, &keymap, KeyCode::Char('z'));
    assert_eq!(app.input, "z", "the hide key is text in the composer");
    assert!(!app.navigator_hidden);
    app.cancel_comment();

    app.mode = Mode::List;
    press(&mut app, &keymap, KeyCode::Char('z'));
    assert!(!app.navigator_hidden, "the action is inert in the comments list");
    app.mode = Mode::Normal;

    press(&mut app, &keymap, KeyCode::Char('/'));
    assert_eq!(app.mode, Mode::Search);
    press(&mut app, &keymap, KeyCode::Char('z'));
    assert_eq!(app.search.as_ref().unwrap().query, "z", "the hide key is text in search");
    assert!(!app.navigator_hidden);
    press(&mut app, &keymap, KeyCode::Esc);
    press(&mut app, &keymap, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);

    // `PR` is exempt: `z` inert there, and the state waits for the return to a file tab.
    press(&mut app, &keymap, KeyCode::Char('z'));
    assert!(app.navigator_hidden);
    app.set_tab(herdr_reviewr::app::Tab::Pr).unwrap();
    assert!(!app.navigator_hidden_here(), "`PR` always shows its navigator");
    press(&mut app, &keymap, KeyCode::Char('z'));
    assert!(app.navigator_hidden, "`z` is inert on `PR`");
    press(&mut app, &keymap, KeyCode::Tab);
    assert_eq!(app.focus, Focus::Files, "`PR` focuses its own navigator freely");
    app.set_tab(herdr_reviewr::app::Tab::Changes).unwrap();
    assert!(app.navigator_hidden_here(), "the hidden state survives the `PR` visit");
    assert_eq!(app.focus, Focus::Diff, "and the return restores read-pane focus");
}

#[test]
fn footer_swaps_the_hide_key_between_go_and_row_one() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    let bands = app.footer_bands();
    assert!(
        bands.contains(&(FooterAction::NavigatorHide, Band::Go)),
        "visible: `z hide` waits under `?`"
    );
    assert!(!bands.contains(&(FooterAction::NavigatorHide, Band::Do)));

    app.focus = Focus::Files;
    let bands = app.footer_bands();
    assert!(
        bands.contains(&(FooterAction::NavigatorHide, Band::Do)),
        "the files pane's calm row 1 carries `z hide`"
    );
    app.focus = Focus::Diff;

    app.toggle_navigator_hidden();
    let bands = app.footer_bands();
    assert!(
        bands.contains(&(FooterAction::NavigatorHide, Band::Do)),
        "hidden: `z show` joins row 1"
    );
    assert!(
        !bands.iter().any(|&(a, _)| a == FooterAction::NavigatorPosition),
        "`p layout` drops while hidden"
    );

    app.set_tab(herdr_reviewr::app::Tab::Pr).unwrap();
    let bands = app.footer_bands();
    assert!(
        !bands.iter().any(|&(a, _)| a == FooterAction::NavigatorHide),
        "`PR` never lists the hide key"
    );
    assert!(bands.contains(&(FooterAction::NavigatorPosition, Band::Go)), "`p` stays on `PR`");
}

#[test]
fn hidden_footer_keeps_the_way_back_on_an_empty_changeset() {
    let repo = Repo::init();
    repo.write("a.rs", "fn a() {}\n");
    repo.commit_all("c");
    let mut app = app_on(&repo);
    app.toggle_navigator_hidden();

    let bands = app.footer_bands();
    assert!(bands.contains(&(FooterAction::TogglePane, Band::Go)), "`tab files` stays offered");
    assert!(bands.contains(&(FooterAction::NavigatorHide, Band::Do)), "`z show` joins row 1");
}

#[test]
fn hidden_empty_read_pane_leads_with_show() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.toggle_navigator_hidden();
    app.visible.clear();

    let bands = app.footer_bands();
    assert_eq!(bands[0], (FooterAction::NavigatorHide, Band::Primary), "`z show` leads row 1");
    assert!(bands.contains(&(FooterAction::TogglePane, Band::Do)), "`tab files` sits beside it");
}

#[test]
fn divider_drag_cancels_until_mouse_up() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 120, 40);
    let body = herdr_reviewr::ui::body_rect(area, &app);
    let row = body.y + body.height / 2;
    let divider = (body.x..body.x + body.width)
        .find(|&col| herdr_reviewr::ui::hit_divider(area, &app, col, row))
        .unwrap();
    let heights = vec![1usize; app.visible.len()];
    let event = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };

    handle_mouse(
        &mut app,
        event(MouseEventKind::Down(MouseButton::Left), divider, row),
        area,
        &heights,
        &keymap,
        &herdr_reviewr::export::Clipboard,
    )
    .unwrap();
    handle_mouse(
        &mut app,
        event(MouseEventKind::Drag(MouseButton::Left), 70, row),
        area,
        &heights,
        &keymap,
        &herdr_reviewr::export::Clipboard,
    )
    .unwrap();
    let resized = app.navigator_side_pct;
    assert_ne!(resized, 32);

    press(&mut app, &keymap, KeyCode::Tab);
    assert!(app.divider_drag_cancelled());
    handle_mouse(
        &mut app,
        event(MouseEventKind::Drag(MouseButton::Left), 10, body.y + 2),
        area,
        &heights,
        &keymap,
        &herdr_reviewr::export::Clipboard,
    )
    .unwrap();
    assert_eq!(app.navigator_side_pct, resized);
    assert_eq!(app.select_anchor, None, "a cancelled resize never becomes a selection");

    handle_mouse(
        &mut app,
        event(MouseEventKind::Up(MouseButton::Left), 10, body.y + 2),
        area,
        &heights,
        &keymap,
        &herdr_reviewr::export::Clipboard,
    )
    .unwrap();
    assert!(!app.divider_drag_cancelled());

    // Opening a modal is a keypress cancellation; its mouse-up must still release capture.
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_divider_drag();
    press(&mut app, &keymap, KeyCode::Char('c'));
    assert!(app.composing());
    assert!(app.divider_drag_cancelled());
    handle_mouse(
        &mut app,
        event(MouseEventKind::Up(MouseButton::Left), divider, row),
        area,
        &heights,
        &keymap,
        &herdr_reviewr::export::Clipboard,
    )
    .unwrap();
    assert!(!app.divider_drag_cancelled());
}

#[test]
fn navigator_config_changes_override_only_when_the_config_value_changes() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let default = herdr_reviewr::config::PluginConfig::default();
    app.set_plugin_config(default.clone());
    app.cycle_navigator_position();
    assert_eq!(app.navigator_position, NavigatorPosition::Bottom);

    app.set_plugin_config(default);
    assert_eq!(
        app.navigator_position,
        NavigatorPosition::Bottom,
        "an unchanged config preserves the session override"
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "navigator_position = \"left\"\n").unwrap();
    let changed = herdr_reviewr::config::plugin_config_in(dir.path()).unwrap();
    app.start_divider_drag();
    app.set_plugin_config(changed);
    assert_eq!(app.navigator_position, NavigatorPosition::Left);
    assert!(app.divider_drag_cancelled(), "a config layout change cancels the old gesture");
}

#[test]
fn ctrl_w_deletes_the_previous_word_in_a_comment() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    for ch in "needs a closer look".chars() {
        app.input_push(ch);
    }
    app.input_delete_word(); // drops "look"
    assert_eq!(app.input, "needs a closer ");
    app.input_delete_word(); // drops the space then "closer"
    assert_eq!(app.input, "needs a ");
}

#[test]
fn the_comment_box_grows_as_a_long_line_wraps() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    // A single long line with no explicit newline must still report more than one row.
    let width = 30; // narrow diff pane
    let one_word = herdr_reviewr::ui::composer_height(&app, width);
    for ch in "the quick brown fox jumps over the lazy dog again and again".chars() {
        app.input_push(ch);
    }
    let wrapped = herdr_reviewr::ui::composer_height(&app, width);
    assert!(wrapped > one_word, "box grew from {one_word} to {wrapped} rows as text wrapped");
}

#[test]
fn a_comment_can_be_edited_then_deleted() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "original");
    let snippet_before = app.store.get(0).unwrap().lines.clone();

    app.open_list();
    app.start_edit();
    app.input.clear();
    for ch in "rewritten".chars() {
        app.input_push(ch);
    }
    app.submit_comment();
    assert_eq!(app.store.get(0).unwrap().text, "rewritten");
    assert_eq!(app.store.get(0).unwrap().lines, snippet_before, "edit changes only the text");

    app.open_list();
    app.delete_comment();
    assert!(app.store.is_empty());
}

#[test]
fn deleting_the_last_comment_closes_the_list_overlay() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "only one");
    app.open_list();
    assert_eq!(app.mode, Mode::List);
    app.delete_comment();
    assert!(app.store.is_empty());
    assert_eq!(app.mode, Mode::Normal, "an emptied overlay closes instead of stranding the user");
}

#[test]
fn finishing_an_edit_returns_to_its_origin() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "first");
    comment_on(&mut app, ' ', "second");

    // Edit from the comments-list overlay → returns to the list.
    app.open_list();
    app.start_edit();
    app.input_push('!');
    app.submit_comment();
    assert_eq!(app.mode, Mode::List, "a list-initiated edit returns to the list");

    // Edit from the diff → returns to Normal.
    app.close_list();
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_edit();
    app.submit_comment();
    assert_eq!(app.mode, Mode::Normal, "a diff-initiated edit returns to Normal");
}

#[test]
fn editing_from_the_list_navigates_to_the_comments_file() {
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\n");
    r.write("b.rs", "one\ntwo\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nBETA\n");
    r.write("b.rs", "one\nTWO\n");
    let mut app = app_on(&r);

    // Comment on b.rs, then move the view to a.rs.
    let bi = app.entries.iter().position(|f| f.path == "b.rs").unwrap();
    app.select_file(bi).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    for ch in "fix this".chars() {
        app.input_push(ch);
    }
    app.submit_comment();
    let ai = app.entries.iter().position(|f| f.path == "a.rs").unwrap();
    app.select_file(ai).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));

    // Editing from the list returns to the file, on a real line.
    app.open_list();
    app.start_edit();
    assert!(app.composing());
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"), "edit switched to the comment's file");
    let dl = &app.diff.rows[app.diff_cursor];
    assert!(dl.new_no().is_some() || dl.old_no().is_some(), "cursor sits on a real diff line");
}

#[test]
fn editing_a_range_comment_opens_the_box_at_the_ranges_last_row() {
    let r = selection_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('v'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Char('c'));
    for ch in "range note".chars() {
        app.input_push(ch);
    }
    app.submit_comment();

    // `e` reopens the box in the card's place, the range's last row.
    app.diff_cursor = 0;
    app.start_edit();
    assert!(app.composing());
    assert_eq!(app.diff_cursor, 2, "the edit box opens at the range's last row");
}

#[test]
fn a_comment_on_a_reverted_file_is_flagged_stale() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "note");

    r.write("a.rs", "alpha\nbeta\ngamma\ndelta\n"); // back to committed state
    app.reload().unwrap();

    assert!(app.entries.iter().all(|f| f.path != "a.rs"), "file left the changeset");
    assert_eq!(app.store.len(), 1, "the comment still exists");
    let c = app.store.get(0).unwrap();
    assert!(app.is_stale(c), "a diff comment whose file left the changeset is stale");
}

#[test]
fn switching_scope_swaps_the_changeset() {
    let r = Repo::init();
    r.write("base.rs", "b\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("committed.rs", "c\n");
    r.commit_all("feature work");
    r.write("dirty.rs", "d\n"); // uncommitted, untracked

    let mut app = App::new(r.path_buf(), Scope::Uncommitted, Some("main".to_string()));
    app.reload().unwrap();
    assert!(app.entries.iter().any(|f| f.path == "dirty.rs"));
    assert!(app.entries.iter().all(|f| f.path != "committed.rs"), "uncommitted omits commits");

    // Branch is a superset of uncommitted: it adds the committed work, keeps the dirty file.
    app.set_scope(Scope::Branch).unwrap();
    assert!(app.entries.iter().any(|f| f.path == "committed.rs"), "branch adds committed work");
    assert!(app.entries.iter().any(|f| f.path == "dirty.rs"), "branch keeps the working tree");
}

#[test]
fn changed_totals_follow_the_scope_across_every_change_kind() {
    let r = Repo::init();
    r.write("edited.rs", "one\ntwo\nthree\n");
    r.write("deleted.rs", "gone one\ngone two\n");
    r.write("old_name.rs", "stable rename contents\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("committed.rs", "branch one\nbranch two\n");
    r.commit_all("feature work");

    r.write("edited.rs", "one\nTWO\nthree\n");
    r.remove("deleted.rs");
    r.git(&["mv", "old_name.rs", "new_name.rs"]);
    r.write("untracked.rs", "new one\nnew two\nnew three\n");

    let mut app = App::new(r.path_buf(), Scope::Uncommitted, Some("main".to_string()));
    app.reload().unwrap();
    assert_eq!(app.changed_count(), 4, "edit, deletion, rename, and untracked file");
    assert_eq!(app.changed_totals(), (4, 3), "+1 edit, +3 untracked, -1 edit, -2 deletion");

    // Branch is a superset: the committed file's lines join the totals.
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(app.changed_totals(), (6, 3));

    r.write("untracked.rs", "new one\nnew two\nnew three\nnew four\n");
    app.reload().unwrap();
    assert_eq!(app.changed_totals(), (7, 3), "a refresh re-sums the changeset");
}

#[test]
fn a_multi_line_range_comment_spans_lines_and_keeps_the_whole_snippet() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    // Anchor on the first changed line, then extend the selection down two rows.
    let first = row_with(&app, '-');
    app.diff_cursor = first;
    app.toggle_select();
    app.move_cursor(1).unwrap();
    app.move_cursor(1).unwrap();
    let (lo, hi) = app.selection_range();
    assert!(hi > lo, "selection spans more than one line");

    app.start_comment();
    for ch in "this whole hunk is suspicious".chars() {
        app.input_push(ch);
    }
    app.submit_comment();

    assert_eq!(app.store.len(), 1);
    let c = app.store.iter().next().unwrap();
    assert!(c.end > c.start, "comment covers a line range: {}..{}", c.start, c.end);
    let snippet: Vec<&str> = c.lines.lines().collect();
    assert!(snippet.len() >= 2, "snippet keeps every selected line: {:?}", c.lines);
    assert!(
        snippet.iter().all(|l| l.starts_with(['+', '-', ' '])),
        "every snippet line keeps its diff marker: {:?}",
        c.lines
    );
}

#[test]
fn an_upward_range_comment_spans_the_same_lines_as_a_downward_one() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    // Anchor two rows below the first changed line, then extend the selection upward.
    let first = row_with(&app, '-');
    app.diff_cursor = first + 2;
    app.toggle_select();
    app.move_cursor(-1).unwrap();
    app.move_cursor(-1).unwrap();
    let (lo, hi) = app.selection_range();
    assert_eq!((lo, hi), (first, first + 2), "selection spans all three rows");

    app.start_comment();
    for ch in "this whole hunk is suspicious".chars() {
        app.input_push(ch);
    }
    app.submit_comment();

    let c = app.store.iter().next().expect("a comment");
    assert!(c.end > c.start, "comment covers a line range: {}..{}", c.start, c.end);
    assert!(c.lines.lines().count() >= 2, "snippet keeps every selected line: {:?}", c.lines);
}

#[test]
fn the_composer_reserve_follows_the_selections_last_line_not_the_cursor() {
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut original = String::new();
    for i in 0..60 {
        writeln!(original, "line {i}").unwrap();
    }
    r.write("big.rs", &original);
    r.commit_all("init");
    r.write("big.rs", &original.replace("line", "LINE"));

    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    // Anchor low, extend upward far enough that the two ends cannot share a viewport.
    app.diff_cursor = 40;
    app.toggle_select();
    for _ in 0..20 {
        app.move_cursor(-1).unwrap();
    }
    app.start_comment();
    for ch in "one\ntwo\nthree".chars() {
        app.input_push(ch);
    }

    let viewport = 12;
    let effective = viewport - herdr_reviewr::ui::composer_height(&app, 80);
    clamp(&mut app, effective);
    let anchored = app.selection_range().1;
    assert!(
        (app.diff_scroll..app.diff_scroll + effective).contains(&anchored),
        "the box's anchor line {} stays in the reserved viewport [{}, {})",
        anchored,
        app.diff_scroll,
        app.diff_scroll + effective
    );
}

#[test]
fn scope_cannot_change_while_composing() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    app.input_push('x');

    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(app.scope, Scope::Uncommitted, "scope is frozen mid-comment");
    assert!(app.composing(), "still composing");
    assert_eq!(app.input, "x", "input untouched");
}

#[test]
fn tab_cannot_change_while_composing() {
    use herdr_reviewr::app::Tab;
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    app.input_push('x');

    // Mid-comment, a tab switch is a no-op.
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.tab, Tab::Changes, "the tab is frozen mid-comment");
    assert!(app.composing(), "still composing");
    assert_eq!(app.input, "x", "input untouched");
}

#[test]
fn the_app_reads_branch_scoped_diffs_not_working_tree() {
    let r = Repo::init();
    r.write("shared.rs", "base\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("on_branch.rs", "committed on the feature branch\n");
    r.commit_all("feature work");

    let mut app = App::new(r.path_buf(), Scope::Branch, Some("main".to_string()));
    app.reload().unwrap();

    let idx =
        app.entries.iter().position(|f| f.path == "on_branch.rs").expect("branch file listed");
    app.select_file(idx).unwrap();

    // The branch scope shows committed content.
    let on_branch = app
        .diff
        .rows
        .iter()
        .any(|r| r.marker() == '+' && r.text().contains("committed on the feature branch"));
    assert!(on_branch, "branch diff carries the committed line");
}

#[test]
fn the_diff_scroll_is_sticky_and_only_follows_the_cursor_off_screen() {
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut original = String::new();
    for i in 0..60 {
        writeln!(original, "line {i}").unwrap();
    }
    r.write("big.rs", &original);
    r.commit_all("init");
    let edited = original.replace("line", "LINE");
    r.write("big.rs", &edited);

    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let height = 10;

    clamp(&mut app, height);
    assert_eq!(app.diff_scroll, 0);

    // Cursor moves but stays in view — the window does not scroll.
    app.diff_cursor = 5;
    clamp(&mut app, height);
    assert_eq!(app.diff_scroll, 0, "no scroll while the cursor is visible");

    // Cursor leaves the bottom — scroll just enough to reveal it, no recentering.
    app.diff_cursor = 12;
    clamp(&mut app, height);
    assert_eq!(app.diff_scroll, 12 + 1 - height);

    // Cursor jumps back above the window — scroll follows up to it.
    app.diff_cursor = 1;
    clamp(&mut app, height);
    assert_eq!(app.diff_scroll, 1);

    // A viewport taller than the whole diff never scrolls.
    app.diff_cursor = 0;
    let tall = app.visible.len() + 50;
    clamp(&mut app, tall);
    assert_eq!(app.diff_scroll, 0, "no scroll when the diff fits the viewport");
}

#[test]
fn a_refresh_keeps_the_diff_scroll_position() {
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut original = String::new();
    for i in 0..60 {
        writeln!(original, "line {i}").unwrap();
    }
    r.write("big.rs", &original);
    r.commit_all("init");
    r.write("big.rs", &original.replace("line", "LINE"));

    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = 25;
    clamp(&mut app, 10);
    let (cursor, scroll) = (app.diff_cursor, app.diff_scroll);
    assert!(scroll > 0, "we scrolled down into the diff");

    // A poll refresh of the same, still-changed file must not snap back to the top.
    app.reload().unwrap();
    assert_eq!(app.diff_cursor, cursor, "refresh keeps the cursor line");
    assert_eq!(app.diff_scroll, scroll, "refresh keeps the scroll position");
}

#[test]
fn the_diff_title_stays_on_the_composed_file_through_a_refresh() {
    let r = edited_repo(); // a.rs is the only changed file
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));

    app.start_comment();
    app.input_push('x');

    // While composing, a.rs leaves the changeset and another file appears.
    r.write("a.rs", "alpha\nbeta\ngamma\ndelta\n");
    r.write("z.rs", "new\n");
    app.reload().unwrap();

    // The frozen diff and title stay on the commented file.
    assert!(app.composing());
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"), "diff title frozen on composed file");
    assert_ne!(app.current_entry().map(|f| f.path.as_str()), Some("a.rs"));
}

#[test]
fn a_comment_submitted_after_its_file_left_the_changeset_anchors_to_that_file() {
    let r = edited_repo(); // a.rs is the only changed file
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    for ch in "note for a.rs".chars() {
        app.input_push(ch);
    }

    // a.rs leaves the changeset and another file appears, drifting the file cursor.
    r.write("a.rs", "alpha\nbeta\ngamma\ndelta\n");
    r.write("z.rs", "new\n");
    app.reload().unwrap();
    assert_ne!(app.current_entry().map(|f| f.path.as_str()), Some("a.rs"));

    app.submit_comment();
    let c = app.store.iter().next().unwrap();
    assert_eq!(c.file, "a.rs", "comment anchors to its diff's file, not the drifted cursor");
}

#[test]
fn deleting_the_last_listed_comment_clamps_the_list_cursor() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "one");
    comment_on(&mut app, '-', "two");

    app.open_list();
    app.list_move(1); // cursor on the last comment (index 1)
    assert_eq!(app.list_cursor, 1);

    app.delete_comment(); // removes index 1
    assert_eq!(app.store.len(), 1);
    assert_eq!(app.list_cursor, 0, "list cursor clamps back into range");
}

#[test]
fn a_non_repo_path_yields_an_empty_state_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(dir.path().to_path_buf(), Scope::Uncommitted, None);
    assert!(app.reload().is_ok(), "a non-repo reload is graceful, not an error");
    assert!(app.entries.is_empty());
    assert!(app.diff.rows.is_empty());
}

#[test]
fn jump_moves_the_cursor_onto_a_commented_line() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "note");

    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    app.jump_comment(1);
    assert!(app.comment_marks().1.contains(&app.diff_cursor), "cursor landed on a comment");
}

// --- last-turn scope -----------------------------------------------------------

/// One agent working in `cwd`, as `herdr agent list` would report it.
fn agent_in(cwd: &Path, status: Status) -> AgentSample {
    AgentSample { cwd: Some(cwd.to_string_lossy().into_owned()), status }
}

/// One enumeration on the turn host, mirrored into the app as a landing would.
fn observe_agents(
    app: &mut App,
    host: &mut herdr_reviewr::world::TurnHost,
    samples: Option<&[AgentSample]>,
) {
    let report = host.observe_agents(samples);
    app.sync_turn_baseline(host.baseline().map(str::to_string));
    app.sync_agents_present(report.agents_present);
}

/// An app and turn host on one resolved top level, as `run` builds them.
/// The raw temp path would split them: macOS resolves `/var` to `/private/var`.
fn turn_setup(r: &Repo) -> (App, herdr_reviewr::world::TurnHost) {
    let root = herdr_reviewr::git::toplevel(r.path()).expect("a repo");
    (App::new(root.clone(), Scope::LastTurn, None), herdr_reviewr::world::TurnHost::open(root))
}

/// The single-agent case the older tests drive: one agent at the worktree root.
fn observe_turn(
    app: &mut App,
    host: &mut herdr_reviewr::world::TurnHost,
    repo: &Path,
    status: Option<Status>,
) {
    match status {
        Some(status) => observe_agents(app, host, Some(&[agent_in(repo, status)])),
        None => observe_agents(app, host, None),
    }
}

#[test]
fn last_turn_is_empty_until_a_turn_is_observed() {
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.reload().unwrap();
    assert!(app.awaiting_turn(), "no baseline captured yet");
    assert!(app.entries.is_empty(), "the scope is empty before a turn");
}

#[test]
fn last_turn_shows_a_change_producing_turn() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Idle));
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Working)); // turn start: candidate = "one"
    r.write("a.rs", "one\ntwo\n");
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Working)); // first change promotes the baseline
    app.reload().unwrap();
    assert!(!app.awaiting_turn(), "the baseline is now set");
    assert!(app.entries.iter().any(|f| f.path == "a.rs"), "the turn's edit shows");
}

#[test]
fn a_question_only_turn_keeps_the_previous_turns_diff() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    // Turn A edits a file.
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Idle));
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Working));
    r.write("a.rs", "one\ntwo\n");
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Working));
    // Turn B is a question — no file change.
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Idle));
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Working));
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Idle));
    app.reload().unwrap();
    assert!(
        app.entries.iter().any(|f| f.path == "a.rs"),
        "A's diff persists across a question-only turn"
    );
}

#[test]
fn a_permission_pause_stays_one_turn() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Idle));
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Working)); // turn start: candidate = "one"
    r.write("a.rs", "one\nbefore\n"); // edit before the prompt
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Blocked)); // permission prompt promotes baseline = "one"
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Working)); // resume — must NOT re-baseline
    r.write("a.rs", "one\nbefore\nafter\n"); // edit after the prompt
    observe_turn(&mut app, &mut host, r.path(), Some(Status::Working));
    app.reload().unwrap();
    let a = app.entries.iter().find(|f| f.path == "a.rs").expect("a.rs changed");
    let annotation = a.annotation.as_ref().expect("a changed file is annotated");
    assert_eq!(annotation.additions, 2, "both the pre- and post-prompt edits belong to one turn");
}

#[test]
fn the_baseline_survives_a_restart() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    {
        let (mut app, mut host) = turn_setup(&r);
        observe_turn(&mut app, &mut host, r.path(), Some(Status::Idle));
        observe_turn(&mut app, &mut host, r.path(), Some(Status::Working));
        r.write("a.rs", "one\ntwo\n");
        observe_turn(&mut app, &mut host, r.path(), Some(Status::Working)); // promotes and persists the ref
    }
    // A restarted pane resumes the persisted baseline.
    let (mut restarted, _) = turn_setup(&r);
    restarted.reload().unwrap();
    assert!(!restarted.awaiting_turn(), "baseline resumed from the private ref");
    assert!(restarted.entries.iter().any(|f| f.path == "a.rs"), "the turn's edit still shows");
}

#[test]
fn no_agent_status_pauses_tracking() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    observe_turn(&mut app, &mut host, r.path(), None); // no herdr / no resolvable agent
    r.write("a.rs", "one\ntwo\n");
    observe_turn(&mut app, &mut host, r.path(), None);
    app.reload().unwrap();
    assert!(app.awaiting_turn(), "without a status signal the baseline never forms");
}

#[test]
fn two_agents_in_one_worktree_produce_one_turn() {
    // HH-TURN-PER-WORKTREE: a second agent joins the open turn, never restarts it.
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    let root = r.path().to_path_buf();

    observe_agents(&mut app, &mut host, Some(&[agent_in(&root, Status::Idle)]));
    // Agent A starts. Candidate = "one".
    observe_agents(
        &mut app,
        &mut host,
        Some(&[agent_in(&root, Status::Working), agent_in(&root, Status::Idle)]),
    );
    r.write("a.rs", "one\ntwo\n");
    // Agent B joins mid-turn. A restart here would drop the "two" edit from the diff.
    observe_agents(
        &mut app,
        &mut host,
        Some(&[agent_in(&root, Status::Working), agent_in(&root, Status::Working)]),
    );
    r.write("a.rs", "one\ntwo\nthree\n");
    observe_agents(
        &mut app,
        &mut host,
        Some(&[agent_in(&root, Status::Working), agent_in(&root, Status::Working)]),
    );

    app.reload().unwrap();
    let a = app.entries.iter().find(|f| f.path == "a.rs").expect("a.rs changed");
    let annotation = a.annotation.as_ref().expect("a changed file is annotated");
    assert_eq!(annotation.additions, 2, "both agents' edits belong to the one open turn");
}

#[test]
fn a_turn_ends_only_once_every_agent_rests() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    let root = r.path().to_path_buf();
    let both = |a, b| vec![agent_in(&root, a), agent_in(&root, b)];

    observe_agents(&mut app, &mut host, Some(&both(Status::Idle, Status::Idle)));
    observe_agents(&mut app, &mut host, Some(&both(Status::Working, Status::Working)));
    let still_working = host.observe_agents(Some(&both(Status::Idle, Status::Working)));
    assert!(!still_working.ended, "one agent still working keeps the turn open");
    let rested = host.observe_agents(Some(&both(Status::Idle, Status::Done)));
    assert!(rested.ended, "the turn ends once every agent rests");
}

#[test]
fn a_prompt_answered_into_rest_still_ends_the_turn() {
    // working → blocked → idle still ends the turn.
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    let root = r.path().to_path_buf();

    observe_agents(&mut app, &mut host, Some(&[agent_in(&root, Status::Idle)]));
    observe_agents(&mut app, &mut host, Some(&[agent_in(&root, Status::Working)]));
    let held = host.observe_agents(Some(&[agent_in(&root, Status::Blocked)]));
    assert!(!held.ended, "the permission prompt holds the turn open");
    let rested = host.observe_agents(Some(&[agent_in(&root, Status::Idle)]));
    assert!(rested.ended, "answering the prompt into idle ends the turn");
}

#[test]
fn an_empty_worktree_rests_so_the_first_agent_starts_a_turn() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);

    observe_agents(&mut app, &mut host, Some(&[]));
    assert_eq!(app.turn_wait_message(), "no agent works here", "no agents means empty");
    // The first agent to arrive and work starts a turn, since the empty worktree rested.
    observe_agents(&mut app, &mut host, Some(&[agent_in(r.path(), Status::Working)]));
    assert_eq!(app.turn_wait_message(), "waiting for the first turn", "the agent is a member");
    r.write("a.rs", "one\ntwo\n");
    observe_agents(&mut app, &mut host, Some(&[agent_in(r.path(), Status::Working)]));

    app.reload().unwrap();
    assert!(!app.awaiting_turn(), "the arriving agent's turn formed a baseline");
    assert!(app.entries.iter().any(|f| f.path == "a.rs"), "its edit shows");
}

#[test]
fn an_agent_in_a_second_worktree_of_the_repository_is_not_a_member() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let elsewhere = tempfile::TempDir::new().expect("tempdir");
    let sibling = elsewhere.path().join("wt");
    r.git(&["worktree", "add", "-q", sibling.to_str().unwrap(), "-b", "other"]);

    let (mut app, mut host) = turn_setup(&r);
    // Rest first, so an admitted sibling would visibly start a turn.
    observe_agents(&mut app, &mut host, Some(&[agent_in(&sibling, Status::Idle)]));
    observe_agents(&mut app, &mut host, Some(&[agent_in(&sibling, Status::Working)]));
    assert_eq!(
        app.turn_wait_message(),
        "no agent works here",
        "a second worktree resolves to its own top level"
    );

    r.write("a.rs", "one\ntwo\n");
    observe_agents(&mut app, &mut host, Some(&[agent_in(&sibling, Status::Working)]));
    app.reload().unwrap();
    assert!(app.awaiting_turn(), "a non-member's work never forms this worktree's baseline");
}

#[test]
fn an_agent_whose_cwd_is_not_an_absolute_path_is_not_a_member() {
    // `git -C ""` answers for reviewr's own directory, so blank or relative cwds are refused.
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    let nowhere = |cwd: &str| AgentSample { cwd: Some(cwd.to_string()), status: Status::Working };

    observe_agents(&mut app, &mut host, Some(&[nowhere(""), nowhere("sub")]));
    assert_eq!(app.agents_present(), Some(false), "neither spelling names a worktree");
    r.write("a.rs", "one\ntwo\n");
    observe_agents(&mut app, &mut host, Some(&[nowhere("")]));
    app.reload().unwrap();
    assert!(app.awaiting_turn(), "a non-member never forms this worktree's baseline");
}

#[test]
fn an_agent_in_a_subdirectory_belongs_to_the_worktree() {
    // A subdirectory, resolved through git.
    let r = Repo::init();
    r.write("sub/a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);
    let sub = r.path().join("sub");

    observe_agents(&mut app, &mut host, Some(&[agent_in(&sub, Status::Idle)]));
    assert_eq!(
        app.turn_wait_message(),
        "waiting for the first turn",
        "a subdirectory resolves to the same top level"
    );
    observe_agents(&mut app, &mut host, Some(&[agent_in(&sub, Status::Working)]));
    r.write("sub/a.rs", "one\ntwo\n");
    observe_agents(&mut app, &mut host, Some(&[agent_in(&sub, Status::Working)]));

    app.reload().unwrap();
    assert!(!app.awaiting_turn(), "the subdirectory agent's turn counts");
}

#[test]
fn a_failed_enumeration_keeps_the_previous_membership() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let (mut app, mut host) = turn_setup(&r);

    // A first-poll hiccup observes nothing, so claims nothing.
    observe_agents(&mut app, &mut host, None);
    assert_eq!(app.agents_present(), None, "a failed enumeration observes nothing");
    assert_eq!(app.turn_wait_message(), "waiting for the first turn", "so it waits");

    observe_agents(&mut app, &mut host, Some(&[agent_in(r.path(), Status::Idle)]));
    assert_eq!(app.turn_wait_message(), "waiting for the first turn");
    observe_agents(&mut app, &mut host, None); // herdr hiccup
    assert_eq!(
        app.turn_wait_message(),
        "waiting for the first turn",
        "a failed enumeration never flips the empty state"
    );

    // A mid-turn hiccup neither ends nor re-baselines the turn.
    observe_agents(&mut app, &mut host, Some(&[agent_in(r.path(), Status::Working)]));
    r.write("a.rs", "one\ntwo\n");
    let hiccup = host.observe_agents(None);
    assert!(!hiccup.ended, "a failed enumeration never ends the turn");
    assert_eq!(hiccup.agents_present, None, "a failed enumeration observes nothing");
    observe_agents(&mut app, &mut host, Some(&[agent_in(r.path(), Status::Working)]));
    app.reload().unwrap();
    let a = app.entries.iter().find(|f| f.path == "a.rs").expect("a.rs changed");
    assert_eq!(a.annotation.as_ref().unwrap().additions, 1, "the mid-hiccup edit is in the turn");

    let emptied = host.observe_agents(Some(&[]));
    assert_eq!(emptied.agents_present, Some(false), "a successful empty enumeration observes it");
}

/// The visible-row index of the file at `path`, or `None` when it is hidden/absent.
fn file_row_of(app: &App, path: &str) -> Option<usize> {
    app.file_rows
        .iter()
        .position(|row| row.file_index().is_some_and(|i| app.entries[i].path == path))
}

#[test]
fn all_files_tab_browses_the_whole_worktree_and_renders_content() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::View;
    let r = Repo::init();
    r.write("src/app.rs", "fn main() {}\n");
    r.write("src/ui.rs", "fn render() {}\n");
    r.write("README.md", "# hi\n");
    r.commit_all("init");
    r.write("README.md", "# changed\n"); // change a top-level file (no dir to reveal)
    let mut app = app_on(&r);

    // Changes lists only the changed file and opens its diff.
    assert_eq!(app.tab, Tab::Changes);
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));

    // All files opens README, with `src/` collapsed.
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.tab, Tab::AllFiles);
    assert!(app.entries.iter().any(|e| e.path == "src/ui.rs"), "an unchanged file is listed");
    assert_eq!(app.diff_path.as_deref(), Some("README.md"), "All files opens its first file");
    assert!(app.file_rows.iter().any(|row| row.dir_path() == Some("src")), "src/ is a dir row");
    assert!(file_row_of(&app, "src/ui.rs").is_none(), "a collapsed dir hides its children");

    // Expanding src/ (a click on the directory) then opening a file shows its full content.
    let src_row = app.file_rows.iter().position(|row| row.dir_path() == Some("src")).unwrap();
    app.select_file(src_row).unwrap();
    let ui_row = file_row_of(&app, "src/ui.rs").expect("src/ui.rs visible once src/ is expanded");
    app.select_file(ui_row).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("src/ui.rs"));
    assert_eq!(app.diff.view, View::File);
    assert!(app.diff.rows.iter().any(|row| row.text().contains("fn render")));
}

#[test]
fn a_tab_switch_paints_the_stashed_frame_and_requests_its_refresh() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "fn a() {}\n");
    r.commit_all("base");
    let mut app = app_on(&r);

    // A first visit loads before its frame, leaving no pending request.
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(app.world_request.is_none(), "a first visit loads synchronously");
    assert!(app.entries.iter().any(|e| e.path == "a.rs"), "the first frame is populated");

    // A return paints the stash and requests a refresh (Continuity).
    enter_tab(&mut app, Tab::Changes);
    r.write("b.rs", "fn b() {}\n");
    app.set_tab(Tab::AllFiles).unwrap();
    let request = app.world_request.expect("a return visit requests its refresh");
    assert!(request.reveal, "the landing will re-reveal the re-anchored cursor");
    assert!(
        !app.entries.iter().any(|e| e.path == "b.rs"),
        "the switch frame is the stash, not the current worktree"
    );

    // The completion lands: the built snapshot reconciles and the view catches up.
    let snapshot = herdr_reviewr::world::build(&app.world_input()).unwrap();
    app.reconcile_world(snapshot);
    assert!(app.entries.iter().any(|e| e.path == "b.rs"), "the landing caught up");
}

#[test]
fn switching_tabs_restores_each_tab_selection() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::View;
    let r = Repo::init();
    r.write("src/app.rs", "fn main() {}\n");
    r.write("README.md", "# hi\n");
    r.commit_all("init");
    r.write("src/app.rs", "fn main() { run() }\n");
    let mut app = app_on(&r);
    let changes_open = app.diff_path.clone();
    assert_eq!(changes_open.as_deref(), Some("src/app.rs"));

    // In All files, open README.md.
    enter_tab(&mut app, Tab::AllFiles);
    let readme_row = file_row_of(&app, "README.md").expect("README.md at the top level");
    app.select_file(readme_row).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert_eq!(app.diff.view, View::File);

    // Back to Changes: its own selection and diff are restored, not All files'.
    enter_tab(&mut app, Tab::Changes);
    assert_eq!(app.tab, Tab::Changes);
    assert_eq!(app.entries.len(), 1, "Changes still lists only the changed file");
    assert_eq!(app.diff_path, changes_open);
    assert_eq!(app.diff.view, View::Diff);

    // Forward again: All files restored README.md, not the Changes selection.
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert_eq!(app.diff.view, View::File);
}

#[test]
fn changed_count_and_staleness_stay_scope_based_on_all_files() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::model::Comment;
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.write("b.rs", "two\n");
    r.commit_all("init");
    r.write("a.rs", "ONE\n"); // exactly one changed file
    let mut app = app_on(&r);
    assert_eq!(app.changed_count(), 1, "Changes counts the one changed file");

    // A diff comment on b.rs, which is in the worktree but not in the changeset.
    let comment = Comment {
        file: "b.rs".into(),
        side: Side::New,
        start: 1,
        end: 1,
        lines: " two".into(),
        text: "?".into(),
        diff_anchored: true,
        rev: herdr_reviewr::model::Rev::Worktree,
        draft: None,
    };
    app.store.add(comment.clone());

    enter_tab(&mut app, Tab::AllFiles);
    assert!(app.entries.len() >= 2, "All files lists the whole worktree");
    assert_eq!(app.changed_count(), 1, "the count is the changeset, not the worktree total");
    assert!(
        app.is_stale(&comment),
        "a diff comment keys on the changeset even while All files lists b.rs"
    );
}

/// The `All files` row for `path`, if visible, and its annotation.
#[allow(clippy::option_option)] // outer = row found, inner = its annotation
fn annotation_of(app: &App, path: &str) -> Option<Option<herdr_reviewr::model::ChangedFile>> {
    use herdr_reviewr::file_list::RowKind;
    app.file_rows.iter().find_map(|row| match &row.kind {
        RowKind::File { index } if app.entries[*index].path == path => {
            Some(app.entries[*index].annotation.clone())
        }
        _ => None,
    })
}

#[test]
fn all_files_annotates_changed_files_only() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::model::ChangeKind;
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.write("b.rs", "two\n");
    r.commit_all("init");
    r.write("a.rs", "ONE\n"); // a.rs changed, b.rs unchanged
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert!(
        matches!(annotation_of(&app, "a.rs"), Some(Some(a)) if a.kind == ChangeKind::Modified),
        "a changed file carries its marker"
    );
    assert_eq!(
        annotation_of(&app, "b.rs"),
        Some(None),
        "an unchanged file is listed without a marker"
    );
}

#[test]
fn switching_scope_on_all_files_remarks_in_place() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.write("b.rs", "two\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("b.rs", "TWO\n");
    r.commit_all("committed change to b"); // committed on the branch
    r.write("a.rs", "ONE\n"); // one uncommitted change
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Files;
    app.move_cursor(1).unwrap();
    let cursor = app.file_cursor;
    assert_eq!(app.changed_count(), 1, "uncommitted marks only the dirty file");
    assert!(
        matches!(annotation_of(&app, "a.rs"), Some(Some(_))),
        "a.rs is marked under uncommitted"
    );
    assert_eq!(annotation_of(&app, "b.rs"), Some(None), "b.rs is unmarked under uncommitted");

    // The changed set rebuilds now; the tree's annotations follow.
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(app.file_cursor, cursor, "the cursor holds across a scope re-mark");
    assert_eq!(app.changed_count(), 2, "branch marks both the committed and the dirty file");
    assert!(app.world_request.is_some(), "the tree's annotations refresh behind the switch");
    common::land_world(&mut app);
    assert_eq!(app.file_cursor, cursor, "the cursor still holds after the landing");
    assert!(matches!(annotation_of(&app, "a.rs"), Some(Some(_))), "a.rs stays marked");
    assert!(matches!(annotation_of(&app, "b.rs"), Some(Some(_))), "b.rs is now marked");
}

#[test]
fn all_files_lazily_loads_an_expanded_ignored_directory() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("src/app.rs", "fn main() {}\n");
    r.commit_all("init");
    r.write(".gitignore", "target/\n");
    r.write("target/build.o", "x\n");
    r.write("target/sub/y.o", "y\n");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Files;

    // target/ is a collapsed, ignored placeholder; its contents are not loaded yet.
    assert!(app.entries.iter().any(|e| e.path == "target" && e.is_dir && e.ignored));
    assert!(!app.entries.iter().any(|e| e.path.starts_with("target/")), "children not loaded yet");

    // Expand it → immediate children load (one level only), still ignored/dimmed.
    let row = |a: &App| a.file_rows.iter().position(|r| r.dir_path() == Some("target")).unwrap();
    app.file_cursor = row(&app);
    app.expand_dir();
    assert!(
        app.entries.iter().any(|e| e.path == "target/build.o" && e.ignored),
        "file child loads"
    );
    assert!(
        app.entries.iter().any(|e| e.path == "target/sub" && e.is_dir),
        "subdir placeholder loads"
    );
    assert!(!app.entries.iter().any(|e| e.path == "target/sub/y.o"), "deeper level stays lazy");

    // Collapse → children drop back out of the entry set.
    app.file_cursor = row(&app);
    app.collapse_dir();
    assert!(
        !app.entries.iter().any(|e| e.path.starts_with("target/")),
        "collapsing unloads children"
    );
}

#[test]
fn content_comment_is_stale_only_when_its_file_is_deleted() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let row = file_row_of(&app, "a.rs").expect("a.rs at the top level");
    app.select_file(row).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    app.start_comment();
    for ch in "note".chars() {
        app.input_push(ch);
    }
    app.submit_comment();
    let c = app.store.get(0).expect("a comment was made").clone();
    assert!(!c.diff_anchored, "a File-view comment is content-anchored");

    app.reload().unwrap();
    assert!(!app.is_stale(&c), "a content comment on an existing, unchanged file is not stale");
    r.remove("a.rs");
    app.reload().unwrap();
    assert!(app.is_stale(&c), "it becomes stale only once its file is deleted");
}

#[test]
fn the_tabs_keep_independent_selections() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init"); // a clean worktree — no changes
    let mut app = app_on(&r);
    assert_eq!(app.changed_count(), 0);
    assert!(app.diff_path.is_none(), "Changes opens nothing with an empty changeset");

    enter_tab(&mut app, Tab::AllFiles);
    let row = file_row_of(&app, "a.rs").unwrap();
    app.select_file(row).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"), "viewing a.rs in All files");

    // Back to Changes: nothing carries over, so its own (empty) state stands.
    enter_tab(&mut app, Tab::Changes);
    assert!(app.diff_path.is_none(), "the All files selection does not carry into Changes");
}

#[test]
fn a_file_view_comment_exports_as_path_line_with_a_context_snippet() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\ngamma\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let row = file_row_of(&app, "a.rs").expect("a.rs listed");
    app.select_file(row).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = 1; // the second line, "beta"
    app.start_comment();
    for ch in "why".chars() {
        app.input_push(ch);
    }
    app.submit_comment();

    let target = FakeTarget::ok();
    app.export(&target);
    let out = target.last();
    assert!(out.contains("a.rs:2"), "header is path:line:\n{out}");
    assert!(!out.contains("(removed)"), "a content comment never carries (removed):\n{out}");
    assert!(out.contains(" beta"), "the snippet is the space-prefixed content line:\n{out}");
}

#[test]
fn a_diff_unset_file_reads_as_the_binary_notice_not_a_text_diff() {
    // A `-diff` text file takes git's no-text-diff verdict.
    use herdr_reviewr::diff::{Notice, View};
    let r = Repo::init();
    r.write(".gitattributes", "flake.lock -diff\n");
    r.write("flake.lock", "one\ntwo\n");
    r.commit_all("init");
    r.write("flake.lock", "one\nTWO\n");

    let mut app = app_on(&r);
    let row = file_row_of(&app, "flake.lock").expect("flake.lock listed");
    app.select_file(row).unwrap();

    assert_eq!(app.diff.notice, Some(Notice::Binary));
    assert_eq!(app.diff.view, View::Diff);
    assert!(app.diff.rows.is_empty(), "a notice has no rows to comment on");
    assert!(app.visible.is_empty());
}

#[test]
fn a_file_crossing_steps_over_a_diff_unset_file() {
    let r = Repo::init();
    r.write(".gitattributes", "b.lock -diff\n");
    r.write("a.rs", "one\n");
    r.commit_all("init");
    r.write("a.rs", "ONE\n");
    r.write("b.lock", "one\ntwo\nthree\n");
    r.write("c.rs", "fn c() {}\n");

    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let row = file_row_of(&app, "a.rs").expect("a.rs listed");
    app.select_file(row).unwrap();
    app.focus = Focus::Diff;
    for _ in 0..4 {
        press(&mut app, &keymap, KeyCode::Char(']'));
        if app.diff_path.as_deref() != Some("a.rs") {
            break;
        }
    }
    assert_eq!(app.diff_path.as_deref(), Some("c.rs"), "a crossing always lands on a change");
}

#[test]
fn a_diff_unset_file_in_all_files_still_reads_its_content() {
    // `-diff` governs diffing, not reading, so `All files` shows it.
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::View;
    let r = Repo::init();
    r.write(".gitattributes", "flake.lock -diff\n");
    r.write("flake.lock", "one\ntwo\n");
    r.commit_all("init");

    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let row = file_row_of(&app, "flake.lock").expect("flake.lock listed");
    app.select_file(row).unwrap();

    assert_eq!(app.diff.notice, None);
    assert_eq!(app.diff.view, View::File);
    assert!(app.diff.rows.iter().any(|row| row.text().contains("two")));
}

#[test]
fn an_oversize_file_in_all_files_degrades_to_a_notice() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::{Notice, View};
    let r = Repo::init();
    r.write("small.rs", "fn main() {}\n");
    r.write("big.bin", &"x\n".repeat(1_100_000)); // ~2.2 MB, over the 2 MB budget
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let row = file_row_of(&app, "big.bin").expect("big.bin listed");
    app.select_file(row).unwrap();
    assert_eq!(app.diff.notice, Some(Notice::TooLarge), "an over-budget file is not read whole");
    assert_eq!(app.diff.view, View::File);
    assert!(app.visible.is_empty());
}

#[test]
fn switching_to_an_empty_file_view_focuses_the_tree() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "alpha\n");
    r.commit_all("init");
    r.remove("a.rs"); // deleted: still tracked (in ls-files) but empty on disk
    let mut app = app_on(&r);
    app.focus = Focus::Diff; // reader is in the diff pane on the deletion
    enter_tab(&mut app, Tab::AllFiles);
    assert!(app.visible.is_empty(), "the deleted file's content view is empty");
    assert_eq!(app.focus, Focus::Files, "an empty read pane focuses the tree, not traps the keys");
}

#[test]
fn a_diff_comment_does_not_render_in_the_file_view() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::View;
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\ngamma\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nBETA\ngamma\n"); // a.rs changed
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+'); // the +BETA insertion
    app.start_comment();
    app.input_push('x');
    app.submit_comment();
    assert!(app.store.get(0).unwrap().diff_anchored, "made in the Changes diff");
    assert!(!app.comment_marks().1.is_empty(), "renders in its own diff view");

    // In All files, open a.rs as content: the diff-anchored comment must not bleed in.
    enter_tab(&mut app, Tab::AllFiles);
    let row = file_row_of(&app, "a.rs").expect("a.rs listed");
    app.select_file(row).unwrap();
    assert_eq!(app.diff.view, View::File);
    assert!(
        app.comment_marks().1.is_empty(),
        "a diff-anchored comment does not render in the File view"
    );
}

#[test]
fn editing_a_comment_on_all_files_opens_the_file_view() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::View;
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\n");
    r.write("b.rs", "one\ntwo\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    // A content comment on a.rs.
    let arow = file_row_of(&app, "a.rs").expect("a.rs listed");
    app.select_file(arow).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    app.start_comment();
    app.input_push('x');
    app.submit_comment();
    // Open b.rs, so the comment's file is not the one shown.
    let brow = file_row_of(&app, "b.rs").expect("b.rs listed");
    app.select_file(brow).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"));

    // Edit the comment from the list: it must bring a.rs back as a File view, not a diff.
    app.open_list();
    app.start_edit();
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
    assert_eq!(app.diff.view, View::File, "editing on All files opens the File view, not a diff");
    assert!(app.composing());
}

#[test]
fn changing_scope_on_all_files_snaps_the_changes_diff_to_the_top() {
    use std::fmt::Write as _;

    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    let mut body = String::new();
    for i in 0..40 {
        writeln!(body, "line {i}").unwrap();
    }
    r.write("a.rs", &body);
    r.commit_all("base");
    r.set_origin_default("main", "main");
    r.git(&["checkout", "-b", "feature"]);
    r.write("a.rs", &body.replace("line 5", "LINE 5"));
    r.commit_all("feature edit"); // a.rs differs from base → changed in branch scope
    r.write("a.rs", &body.replace("line 5", "LINE 5").replace("line 30", "LINE 30")); // uncommitted

    let mut app = app_on(&r); // Uncommitted scope; a.rs open in Changes
    app.focus = Focus::Diff;
    app.diff_cursor = 2;
    app.diff_scroll = 1;

    // Change scope while on All files, then return to Changes.
    enter_tab(&mut app, Tab::AllFiles);
    app.set_scope(Scope::Branch).unwrap();
    enter_tab(&mut app, Tab::Changes);

    assert!(app.entries.iter().any(|e| e.path == "a.rs"), "a.rs is in the branch changeset");
    assert_eq!(app.diff_scroll, 0, "an explicit scope switch snaps the Changes diff to the top");
    assert_eq!(app.diff_cursor, 0);
}

/// A `PR` detour keeps each file tab's open file.
#[test]
fn the_pr_tab_detour_preserves_each_file_tab_state() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.write("b.rs", "two\n");
    r.commit_all("init");
    r.write("a.rs", "ONE\n"); // a.rs is the only changed file
    let mut app = app_on(&r);

    assert_eq!(app.tab, Tab::Changes);
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));

    // All files can open b.rs, which Changes can never show (b.rs is unchanged).
    enter_tab(&mut app, Tab::AllFiles);
    app.select_file(file_row(&app, "b.rs")).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"));

    // Detour through the PR tab; the file tabs stay frozen.
    app.set_tab(Tab::Pr).unwrap();
    assert_eq!(app.tab, Tab::Pr);

    // Returning to All files restores b.rs (active file tab unchanged → no swap).
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"), "All files restored after the PR detour");

    // Returning to Changes swaps its state back — a.rs, never All files' b.rs.
    enter_tab(&mut app, Tab::Changes);
    assert_eq!(app.tab, Tab::Changes);
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"), "Changes restored without bleeding b.rs");
}

/// The PR cursor walks comments only, clamped; the read pane follows it.
#[test]
fn pr_navigator_walks_comments_only_and_clamps() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Check, CheckStatus, Comment, CommentKind, PrSnapshot, PrView};

    let finding = |author: &str| Comment {
        kind: CommentKind::Finding,
        author: author.into(),
        author_is_bot: true,
        anchor: "a.rs:1".into(),
        ..common::comment()
    };
    let snap = PrSnapshot {
        checks: vec![
            Check { name: "build".into(), status: CheckStatus::Success },
            Check { name: "test".into(), status: CheckStatus::Failure },
        ],
        comments: vec![finding("first"), finding("second")],
        ..common::pr_snapshot()
    };

    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr = PrView::Pr(Box::new(snap));

    // The cursor starts on the first comment — checks are skipped entirely.
    assert_eq!(app.pr_row_count(), 2, "two comments; the two checks are not cursor stops");
    assert_eq!(app.pr_selected_comment().map(|c| c.author.as_str()), Some("first"));
    app.pr_move(1);
    assert_eq!(app.pr_selected_comment().map(|c| c.author.as_str()), Some("second"));
    app.pr_move(5);
    assert_eq!(
        app.pr_selected_comment().map(|c| c.author.as_str()),
        Some("second"),
        "clamps at the last comment"
    );
    app.pr_move(-10);
    assert_eq!(
        app.pr_selected_comment().map(|c| c.author.as_str()),
        Some("first"),
        "clamps at the first comment"
    );
}

#[test]
fn apply_pr_follows_the_selected_comment_across_a_refresh() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Comment, PrSnapshot, PrView};

    let comment = |author: &str, created: &str| Comment {
        author: author.into(),
        created_at: created.into(),
        ..common::comment()
    };
    let snap = |comments: Vec<Comment>| {
        PrView::Pr(Box::new(PrSnapshot { comments, ..common::pr_snapshot() }))
    };

    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();

    // Newest-first [ann@10:00, bob@09:00]; the cursor lands on the newest, then move to bob.
    app.apply_pr(snap(vec![
        comment("ann", "2026-06-27T10:00:00Z"),
        comment("bob", "2026-06-27T09:00:00Z"),
    ]));
    assert_eq!(app.pr_selected_comment().map(|c| c.author.as_str()), Some("ann"));
    app.pr_move(1);
    assert_eq!(app.pr_selected_comment().map(|c| c.author.as_str()), Some("bob"));

    // A refresh prepends a newer comment: the cursor follows bob to its new index, not index 1.
    app.apply_pr(snap(vec![
        comment("cara", "2026-06-27T11:00:00Z"),
        comment("ann", "2026-06-27T10:00:00Z"),
        comment("bob", "2026-06-27T09:00:00Z"),
    ]));
    assert_eq!(
        app.pr_selected_comment().map(|c| c.author.as_str()),
        Some("bob"),
        "the cursor follows the same comment by identity, not its old index"
    );

    // A refresh where bob is gone clamps the now-dangling cursor back into range.
    app.apply_pr(snap(vec![
        comment("cara", "2026-06-27T11:00:00Z"),
        comment("ann", "2026-06-27T10:00:00Z"),
    ]));
    assert_eq!(
        app.pr_selected_comment().map(|c| c.author.as_str()),
        Some("ann"),
        "a vanished selection clamps to the last row"
    );
}

#[test]
fn a_held_resolution_and_a_transient_detach_keep_the_painted_pr() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};

    let repo = Repo::init();
    let mut app = app_on(&repo);
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot { number: 42, ..common::pr_snapshot() })));
    assert!(matches!(&app.pr, PrView::Pr(s) if s.number == 42));

    // A resolution that found nothing but proved the head still contained holds the story
    app.apply_pr(PrView::Held);
    assert!(matches!(&app.pr, PrView::Pr(s) if s.number == 42), "held keeps the snapshot");

    // A transient detach mid-rebase keeps it too.
    app.apply_pr(PrView::Detached);
    assert!(matches!(&app.pr, PrView::Pr(s) if s.number == 42), "detach keeps the snapshot");

    // With nothing painted, a detached HEAD is its own calm state.
    let mut fresh = app_on(&repo);
    fresh.apply_pr(PrView::Detached);
    assert!(matches!(fresh.pr, PrView::Detached));

    // An honest empty resolution replaces the story.
    app.apply_pr(PrView::NoPr);
    assert!(matches!(app.pr, PrView::NoPr), "an unheld empty resolution paints");
}

#[test]
fn same_input_failure_preserves_any_visible_pr_snapshot_and_remedy() {
    use herdr_reviewr::forge::PrView;

    let repo = Repo::init();
    let mut app = app_on(&repo);
    let no_pr = PrView::NoPr;
    app.apply_pr(no_pr.clone());

    app.apply_pr(PrView::NotAuthed(
        herdr_reviewr::git::Forge::GitHub,
        "github.example.com".to_string(),
    ));

    assert_eq!(app.pr, no_pr);
    assert_eq!(
        app.pr_notice(),
        Some(
            "Not signed in to github.example.com. Run `gh auth login --hostname github.example.com`, then press r."
        )
    );
}

#[test]
fn theme_selection_swaps_the_palette_and_falls_back() {
    use herdr_reviewr::theme;
    let repo = Repo::init();
    let mut app = App::new(repo.path_buf(), Scope::Uncommitted, None);

    // The default theme is catppuccin (Mocha).
    assert_eq!(*app.palette(), theme::resolve(Some("catppuccin")).palette);

    // A --theme override (highest precedence) swaps the whole palette.
    app.set_cli_theme(Some("catppuccin-latte".to_string()));
    assert_eq!(*app.palette(), theme::resolve(Some("catppuccin-latte")).palette);

    // An unknown name falls back to the default — never a half-applied palette.
    app.set_cli_theme(Some("nope".to_string()));
    assert_eq!(*app.palette(), theme::resolve(Some("catppuccin")).palette);
}

/// Dispatch one key through the event loop's dispatcher, under `keymap` as the frame keymap.
fn press(app: &mut App, keymap: &Keymap, code: KeyCode) {
    handle_key(app, KeyEvent::from(code), Rect::new(0, 0, 120, 40), keymap).unwrap();
}

/// Dispatch one mouse event over the diff pane through the event loop's dispatcher.
fn mouse(app: &mut App, keymap: &Keymap, kind: MouseEventKind) {
    let area = Rect::new(0, 0, 120, 40);
    let heights = vec![1usize; app.visible.len()];
    let event = MouseEvent { kind, column: 10, row: 10, modifiers: KeyModifiers::NONE };
    handle_mouse(app, event, area, &heights, keymap, &herdr_reviewr::export::Clipboard).unwrap();
}

#[test]
fn the_traversal_keys_dispatch_and_rebind() {
    let r = traversal_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;

    press(&mut app, &keymap, KeyCode::Char(']'));
    assert_eq!(cursor_text(&app), "EDIT ONE");
    press(&mut app, &keymap, KeyCode::Char(']'));
    assert_eq!(cursor_text(&app), "EDIT TWO");
    press(&mut app, &keymap, KeyCode::Char(']')); // arms
    press(&mut app, &keymap, KeyCode::Char(']'));
    assert_eq!(app.diff_path.as_deref(), Some("c.rs"), "`]` twice crosses into the next file");
    press(&mut app, &keymap, KeyCode::Char('[')); // arms
    press(&mut app, &keymap, KeyCode::Char('['));
    assert_eq!(cursor_text(&app), "EDIT TWO", "`[` crosses back to the previous file's last hunk");

    press(&mut app, &keymap, KeyCode::Char('f'));
    assert_eq!(app.diff_path.as_deref(), Some("bin.dat"));
    press(&mut app, &keymap, KeyCode::Char('F'));
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));

    // `<` moves the divider left, `>` back.
    let start = app.navigator_side_pct;
    press(&mut app, &keymap, KeyCode::Char('<'));
    assert!(app.navigator_side_pct > start, "`<` grows the navigator");
    press(&mut app, &keymap, KeyCode::Char('>'));
    assert_eq!(app.navigator_side_pct, start, "`>` shrinks it again");

    // Every traversal action is rebindable, like the rest of the keymap.
    let rebound = Keymap::resolve(&[(Action::NextFile, vec![Key::plain('ㅁ')])]).unwrap();
    press(&mut app, &rebound, KeyCode::Char('ㅁ'));
    assert_eq!(app.diff_path.as_deref(), Some("bin.dat"));
    press(&mut app, &rebound, KeyCode::Char('f'));
    assert_eq!(app.diff_path.as_deref(), Some("bin.dat"), "the replaced default is inert");
}

#[test]
fn rebound_keys_dispatch_and_replaced_defaults_go_inert() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::resolve(&[(Action::Comment, vec![Key::plain('ㅊ')])]).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');

    press(&mut app, &keymap, KeyCode::Char('c'));
    assert!(!app.composing(), "`c` was replaced and is inert");

    press(&mut app, &keymap, KeyCode::Char('ㅊ'));
    assert!(app.composing(), "the bound key opens the composer");
}

#[test]
fn rebinding_down_frees_the_arrow_and_tab_stays_fixed() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::resolve(&[
        (Action::Down, vec![Key::plain('x')]),
        (Action::Up, vec![Key::plain('X')]),
    ])
    .unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = 0;

    // The arrows are the `down`/`up` defaults, replaced like any other key.
    press(&mut app, &keymap, KeyCode::Down);
    assert_eq!(app.diff_cursor, 0, "the freed down arrow answers nothing");

    press(&mut app, &keymap, KeyCode::Char('x'));
    assert!(app.diff_cursor > 0, "the bound key moves the cursor");

    press(&mut app, &keymap, KeyCode::Tab);
    assert_eq!(app.focus, Focus::Files, "tab is structural and never rebinds");
}

// ---- in-file find -----------------------------------------------

/// A new `m.rs`: all insertions, one row per line.
fn find_repo() -> Repo {
    let r = Repo::init();
    r.write("base.txt", "x\n");
    r.commit_all("init");
    r.write("m.rs", "let total = 1;\ncompute();\ntotal += 2;\nprint(total);\n");
    r
}

fn open_find(app: &mut App, keymap: &Keymap) {
    let ev = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
    handle_key(app, ev, Rect::new(0, 0, 120, 40), keymap).unwrap();
}

fn find_type(app: &mut App, keymap: &Keymap, text: &str) {
    for ch in text.chars() {
        press(app, keymap, KeyCode::Char(ch));
    }
}

#[test]
fn find_match_ranges_is_smart_case_and_non_overlapping() {
    use herdr_reviewr::app::find_match_ranges;
    // A lowercase query ignores case; the ranges are char indices.
    assert_eq!(find_match_ranges("Total total", "total", false), vec![(0, 5), (6, 11)]);
    // Any uppercase makes it case-sensitive.
    assert_eq!(find_match_ranges("Total total", "Total", true), vec![(0, 5)]);
    // Occurrences are non-overlapping.
    assert_eq!(find_match_ranges("aaaa", "aa", false), vec![(0, 2), (2, 4)]);
    assert!(find_match_ranges("abc", "", false).is_empty());
}

#[test]
fn ctrl_f_opens_the_find_band_and_esc_closes_it_empty() {
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    assert_eq!(app.diff_path.as_deref(), Some("m.rs"));

    open_find(&mut app, &keymap);
    assert_eq!(app.mode, Mode::Find);
    assert_eq!(app.focus, Focus::Diff, "find focuses the read pane");

    find_type(&mut app, &keymap, "total");
    assert_eq!(app.find.as_ref().unwrap().query, "total");

    press(&mut app, &keymap, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal, "esc closes the band");
    assert!(app.find.is_none());

    open_find(&mut app, &keymap);
    assert_eq!(app.find.as_ref().unwrap().query, "", "reopening starts empty");
}

#[test]
fn find_counts_matches_with_the_cursor_ordinal() {
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    app.diff_cursor = 0; // "let total = 1;" — a match
    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "total");
    assert_eq!(app.diff_cursor, 0, "typing lights matches but never moves the cursor");

    // "total" is on rows 0, 2, 3 → three matching rows; the cursor sits on the first.
    assert_eq!(app.find_count(), Some((Some(1), 3)));

    // Off a match, the count shows the total alone.
    app.diff_cursor = 1; // "compute()" — no match
    assert_eq!(app.find_count(), Some((None, 3)));

    // A query with no matches says so; an empty query blanks the count.
    app.find.as_mut().unwrap().query = "zzz".to_string();
    assert_eq!(app.find_count(), Some((None, 0)));
    app.find.as_mut().unwrap().query = String::new();
    assert_eq!(app.find_count(), None);
}

#[test]
fn find_steps_between_matches_and_wraps() {
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "total"); // rows 0, 2, 3

    press(&mut app, &keymap, KeyCode::Enter); // next → row 2
    assert_eq!(app.diff_cursor, 2);
    press(&mut app, &keymap, KeyCode::Down); // next → row 3
    assert_eq!(app.diff_cursor, 3);
    press(&mut app, &keymap, KeyCode::Down); // next wraps → row 0
    assert_eq!(app.diff_cursor, 0);
    press(&mut app, &keymap, KeyCode::Up); // prev wraps → row 3
    assert_eq!(app.diff_cursor, 3);
}

#[test]
fn find_opens_on_a_match_reading_its_ordinal_at_once() {
    // The motivating scenario: land on a symbol, `ctrl+f` it, see its ordinal with no step.
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    app.diff_cursor = 2; // the second "total"
    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "total");
    assert_eq!(app.find_count(), Some((Some(2), 3)));
}

#[test]
fn find_searches_folded_content_and_a_step_expands_the_fold() {
    use herdr_reviewr::diff::Row;
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut base = String::from("total = 0\n");
    for i in 0..10 {
        writeln!(base, "filler{i}").unwrap();
    }
    base.push_str("last = 1\n");
    r.write("m.rs", &base);
    r.commit_all("init");
    r.write("m.rs", &base.replace("last = 1", "last = total"));
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;

    // A leading fold hides line 1 ("total = 0"); the change at line 12 shows "last = total".
    assert!(app.visible.iter().any(|row| matches!(row, Row::Fold { .. })), "a fold hides the head");

    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "total");
    // The folded match and the visible one both count.
    assert_eq!(app.find_count().unwrap().1, 2);

    // From the visible match, a prev step reaches the folded one, expanding its fold.
    let visible = app.visible.iter().position(|row| row.text().contains("last = total")).unwrap();
    app.diff_cursor = visible;
    let before = app.visible.len();
    press(&mut app, &keymap, KeyCode::Up);
    assert!(app.visible.len() > before, "the step expanded the fold");
    assert!(app.visible[app.diff_cursor].text().contains("total = 0"), "the cursor lands on it");
}

#[test]
fn find_is_inert_in_wrong_contexts() {
    use herdr_reviewr::app::Tab;
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;

    // While composing a comment.
    app.diff_cursor = row_with(&app, '+');
    press(&mut app, &keymap, KeyCode::Char('c'));
    assert!(app.composing());
    open_find(&mut app, &keymap);
    assert!(app.composing() && app.mode != Mode::Find, "inert while composing");
    app.cancel_comment();

    // In the comments list.
    comment_on(&mut app, '+', "note");
    app.open_list();
    assert_eq!(app.mode, Mode::List);
    open_find(&mut app, &keymap);
    assert_eq!(app.mode, Mode::List, "inert in the comments list");
    app.close_list();

    // On the `PR` tab, which has no read-pane file.
    app.set_tab(Tab::Pr).unwrap();
    open_find(&mut app, &keymap);
    assert_ne!(app.mode, Mode::Find, "no find on the PR tab");
}

#[test]
fn find_is_inert_without_content_rows() {
    use herdr_reviewr::diff::Row;
    // An empty file has no content rows, so `find_available` is false.
    let r = Repo::init();
    r.write("base.txt", "x\n");
    r.commit_all("init");
    r.write("empty.rs", "");
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    assert_eq!(app.diff_path.as_deref(), Some("empty.rs"));
    app.focus = Focus::Diff;
    assert!(!app.visible.iter().any(Row::is_content), "the empty file has no content rows");
    open_find(&mut app, &keymap);
    assert_ne!(app.mode, Mode::Find, "find is inert on a file with no content rows");
}

#[test]
fn find_in_rendered_markdown_reads_the_rendered_text() {
    let r = Repo::init();
    r.write("doc.md", "# Title\n\nfirst total here\n\ngone para\n\nkeep\n\n**total** second\n");
    r.commit_all("init");
    r.write("doc.md", "# Title\n\nfirst total here\n\nkeep\n\n**total** second\n");
    let mut app = app_on_rendered(&r);
    let keymap = Keymap::default();
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.focus = Focus::Diff;
    assert!(app.rendered_active(), "the markdown file opens rendered");
    open_find(&mut app, &keymap);
    assert_eq!(app.mode, Mode::Find, "find opens over the rendered view");

    // The rendered text matches — `**` is gone — and a marker's text never does.
    find_type(&mut app, &keymap, "total second");
    app.diff_cursor = 0;
    app.find_step(1);
    assert_eq!(app.visible[app.diff_cursor].text(), "total second");
    for q in ["**total**", "removed"] {
        app.find.as_mut().unwrap().query = q.to_string();
        assert_eq!(app.find_count(), Some((None, 0)), "{q}");
    }

    // A flip keeps the query and the cursor's block; hits recompute.
    app.find.as_mut().unwrap().query = "total".to_string();
    app.diff_cursor = rendered_row(&app, "keep");
    app.toggle_rendered();
    assert!(!app.rendered_active());
    assert_eq!(app.find.as_ref().unwrap().query, "total");
    assert_eq!(app.visible[app.diff_cursor].text(), "keep", "the block's own line");
    assert_eq!(app.find_count().map(|(_, n)| n), Some(2), "both source lines match");
    app.toggle_rendered();
    assert!(app.rendered_active());
    assert_eq!(app.visible[app.diff_cursor].text(), "keep");
    assert_eq!(app.find_count(), Some((None, 2)));
}

#[test]
fn a_poll_that_drops_the_open_file_force_closes_find() {
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "total");
    assert_eq!(app.mode, Mode::Find);

    // The agent removes `m.rs`: it leaves the changeset, so the read pane reconciles away.
    r.remove("m.rs");
    let snapshot = herdr_reviewr::world::build(&app.world_input()).unwrap();
    app.reconcile_world(snapshot);
    assert_ne!(app.mode, Mode::Find, "the band force-closes when its file is gone");
    assert!(app.find.is_none());
}

#[test]
fn a_poll_keeping_the_open_file_leaves_find_open_and_re_derives() {
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "total");
    let before = app.find_count().unwrap().1; // 3 matches

    // The agent edits `m.rs` but keeps it in the changeset, adding another `total`.
    r.write("m.rs", "let total = 1;\ncompute();\ntotal += 2;\nprint(total);\nreturn total;\n");
    let snapshot = herdr_reviewr::world::build(&app.world_input()).unwrap();
    app.reconcile_world(snapshot);

    assert_eq!(app.mode, Mode::Find, "a same-file poll keeps the band open (Continuity)");
    assert_eq!(app.find_count().unwrap().1, before + 1, "the count re-derives from new content");
}

#[test]
fn find_opens_on_a_rebound_alt_chord_through_the_dispatcher() {
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::resolve(&[(
        Action::Find,
        vec![Key { ctrl: false, alt: true, code: BindingCode::Char('x') }],
    )])
    .unwrap();
    app.focus = Focus::Diff;
    let area = Rect::new(0, 0, 120, 40);

    // The freed default chord no longer opens find.
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), area, &keymap)
        .unwrap();
    assert_ne!(app.mode, Mode::Find, "the freed default does not open find");

    // A real `alt+x` event dispatched through `handle_key` does.
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), area, &keymap)
        .unwrap();
    assert_eq!(app.mode, Mode::Find, "the rebound alt chord opens find through the dispatcher");
}

#[test]
fn the_comments_list_ignores_quit_and_closes_on_the_comments_binding() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "note");
    let keymap = Keymap::default();

    app.open_list();
    assert_eq!(app.mode, Mode::List);
    press(&mut app, &keymap, KeyCode::Char('q'));
    assert_eq!(app.mode, Mode::List, "`q` does not close the list");
    assert!(!app.should_quit, "and does not quit");

    press(&mut app, &keymap, KeyCode::Char('l'));
    assert_eq!(app.mode, Mode::Normal, "the `comments` binding closes it");

    app.open_list();
    press(&mut app, &keymap, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal, "`esc` closes it");
}

#[test]
fn the_comments_list_acts_through_the_same_bindings() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "note");
    let keymap = Keymap::resolve(&[(Action::Delete, vec![Key::plain('x')])]).unwrap();

    app.open_list();
    press(&mut app, &keymap, KeyCode::Char('d'));
    assert_eq!(app.store.len(), 1, "the replaced default is inert in the list too");
    press(&mut app, &keymap, KeyCode::Char('x'));
    assert!(app.store.is_empty(), "the rebound `delete` acts on the highlighted row");
}

#[test]
fn the_pr_remedy_names_the_rebound_refresh_key() {
    use herdr_reviewr::forge::PrView;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[keybindings]\nrefresh = [\"R\"]\n").unwrap();
    let config = herdr_reviewr::config::plugin_config_in(dir.path()).unwrap();

    let repo = Repo::init();
    let mut app = app_on(&repo);
    app.set_plugin_config(config);
    app.apply_pr(PrView::NoPr);

    app.apply_pr(PrView::NotAuthed(
        herdr_reviewr::git::Forge::GitHub,
        "github.example.com".to_string(),
    ));

    assert!(
        app.pr_notice().is_some_and(|notice| notice.ends_with("then press R.")),
        "the remedy follows the active refresh binding: {:?}",
        app.pr_notice()
    );
}

/// A markdown and a code file on `All files`; keep the `Repo` alive.
fn markdown_app() -> (Repo, App) {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("README.md", "# Title\n\nalpha beta gamma\n");
    r.write("code.rs", "fn main() {}\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("README.md"), "first file opens");
    (r, app)
}

#[test]
fn a_markdown_file_opens_rendered_with_a_cursor() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::Row;
    let r = Repo::init();
    r.write("README.md", "# Title\n");
    r.commit_all("init");
    r.write("README.md", "# Title\n\nalpha **beta** gamma\n\n- one\n- two\n");
    let mut app = app_on_rendered(&r);

    // `Changes` opens markdown rendered.
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert!(app.rendered_active(), "a markdown file opens rendered on the Changes tab");
    assert!(app.visible.iter().all(|row| matches!(row, Row::Rendered { .. })));
    assert!(app.visible[0].text().contains("Title"));
    assert!(!app.visible.iter().any(|row| row.text().contains("**")), "emphasis is consumed");
    assert!(
        matches!(app.visible[0], Row::Rendered { src: 1, .. }),
        "a rendered row names its block"
    );
    assert_eq!(app.visible[0].new_no(), None, "and is no source line itself");

    // The rows take the cursor, the page keys, and the wheel like any read pane.
    app.focus = Focus::Diff;
    app.move_cursor(1).unwrap();
    assert_eq!(app.diff_cursor, 1, "the cursor steps over rendered rows");
    assert!(app.reveal_diff);
    app.move_cursor(100).unwrap();
    assert_eq!(app.diff_cursor, app.visible.len() - 1, "a page clamps at the last row");
    let cursor = app.diff_cursor;
    app.wheel_diff(1);
    assert_eq!(app.diff_scroll, 1, "the wheel scrolls the rendered rows");
    assert_eq!(app.diff_cursor, cursor, "and leaves the cursor put");

    // `All files` opens the same file rendered the same way.
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert!(app.rendered_active(), "a markdown file opens rendered in All files");
    assert!(app.visible.iter().all(|row| matches!(row, Row::Rendered { .. })));
}

#[test]
fn a_non_markdown_file_never_renders() {
    let (_repo, mut app) = markdown_app();
    app.move_cursor(1).unwrap(); // the file list is focused; move opens code.rs
    assert_eq!(app.diff_path.as_deref(), Some("code.rs"));
    assert!(!app.rendered_active(), "a non-markdown file shows its source");
    app.toggle_rendered();
    assert!(!app.rendered_active(), "the toggle is inert on a non-markdown file");
}

#[test]
fn the_rendered_view_takes_comments_and_clears_a_selection_on_entry() {
    let (_repo, mut app) = markdown_app();
    app.focus = Focus::Diff;
    app.toggle_rendered(); // to source
    assert!(!app.rendered_active());
    app.diff_cursor = 2;
    app.toggle_select();
    assert!(app.select_anchor.is_some());

    app.toggle_rendered();
    assert!(app.rendered_active());
    assert!(app.select_anchor.is_none(), "entering the rendered view clears a live selection");

    // The authoring keys work on rendered rows like on source; the rows come pre-wrapped.
    app.toggle_select();
    assert!(app.select_anchor.is_some(), "`v` starts a range in the rendered view");
    app.start_comment();
    assert!(app.composing(), "`c` opens the composer in the rendered view");
    app.cancel_comment();
    app.toggle_wrap();
    assert!(app.wrap, "the wrap toggle is inert in the rendered view");
}

#[test]
fn the_rendered_choice_survives_a_refresh_and_a_file_change() {
    let (_repo, mut app) = markdown_app();
    app.toggle_rendered();
    assert!(!app.rendered_active(), "flipped to source");

    app.reload().unwrap();
    assert!(!app.rendered_active(), "a same-file refresh keeps the source choice");

    app.move_cursor(1).unwrap(); // open code.rs
    app.move_cursor(-1).unwrap(); // back to README.md
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert!(!app.rendered_active(), "reopening the file keeps the pane's source choice");
}

#[test]
fn the_pane_choice_holds_across_tabs_and_the_pr_tab() {
    use herdr_reviewr::app::Tab;
    let (repo, mut app) = markdown_app();
    // Give the Changes tab a markdown file of its own.
    repo.write("README.md", "# Title\n\nalpha beta gamma\n\nedited\n");
    app.toggle_rendered();
    assert!(!app.rendered_active(), "All files flipped to source");

    enter_tab(&mut app, Tab::Changes);
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert!(!app.rendered_active(), "the Changes tab follows the pane's choice");
    app.toggle_rendered();
    enter_tab(&mut app, Tab::AllFiles);
    assert!(app.rendered_active(), "and so does All files, both ways");

    app.set_tab(Tab::Pr).unwrap();
    enter_tab(&mut app, Tab::AllFiles);
    assert!(app.rendered_active(), "a PR round-trip keeps it");
}

#[test]
fn the_description_row_pins_first_and_follows_refetches() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Comment, PrSnapshot, PrView};

    let comment = |author: &str, created: &str| Comment {
        author: author.into(),
        created_at: created.into(),
        ..common::comment()
    };
    let snap = |body: &str, comments: Vec<Comment>| {
        PrView::Pr(Box::new(PrSnapshot { body: body.into(), comments, ..common::pr_snapshot() }))
    };

    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();

    // A non-empty description pins one extra row first.
    app.apply_pr(snap("the body", vec![comment("ann", "2026-06-27T10:00:00Z")]));
    assert_eq!(app.pr_row_count(), 2, "description + one comment");
    assert!(app.pr_on_description(), "the cursor starts on the pinned description");
    assert!(app.pr_selected_comment().is_none(), "the description is not a comment");
    app.pr_move(1);
    assert_eq!(app.pr_selected_comment().map(|c| c.author.as_str()), Some("ann"));

    // A refetch keeps the selected comment across the pinned row's offset.
    app.apply_pr(snap(
        "the body",
        vec![comment("bob", "2026-06-27T11:00:00Z"), comment("ann", "2026-06-27T10:00:00Z")],
    ));
    assert_eq!(
        app.pr_selected_comment().map(|c| c.author.as_str()),
        Some("ann"),
        "identity-following accounts for the description row"
    );

    // On the description, a refetch that keeps a description holds the selection.
    app.pr_move(-5);
    assert!(app.pr_on_description());
    app.apply_pr(snap("edited body", vec![comment("ann", "2026-06-27T10:00:00Z")]));
    assert!(app.pr_on_description(), "the description row keeps its identity");

    // An emptied description vanishes like a comment: the row is gone, the cursor clamps.
    app.apply_pr(snap("", vec![comment("ann", "2026-06-27T10:00:00Z")]));
    assert_eq!(app.pr_row_count(), 1, "no description row without a body");
    assert!(!app.pr_on_description());
    assert_eq!(app.pr_selected_comment().map(|c| c.author.as_str()), Some("ann"));

    // A whitespace-only body is no description either.
    app.apply_pr(snap("  \n ", vec![comment("ann", "2026-06-27T10:00:00Z")]));
    assert_eq!(app.pr_row_count(), 1);
}

#[test]
fn m_flips_between_rendered_and_source_at_the_same_block() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::Row;
    let block_of = |app: &App| match app.visible[app.diff_cursor] {
        Row::Rendered { src, src_end, kind: RenderedKind::Block { wrap, .. }, .. } => {
            (src, src_end, wrap)
        }
        _ => panic!("the cursor is on a rendered row"),
    };

    // A block's first line and back to the block holding it.
    let doc = "# Title\n\npara one\n\n## Section two\n\npara two\n";
    let r = Repo::init();
    r.write("doc.md", doc);
    r.commit_all("init");
    r.write("edit.md", "# Doc\n\nalpha\n\nbeta\n\ngamma\n");
    r.commit_all("edit base");
    r.write("edit.md", "# Doc\n\nalpha\n\ngamma\n"); // delete "beta" and its blank line
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|row| row.text() == "para two").unwrap();
    app.toggle_rendered();
    assert!(!app.rendered_active());
    assert_eq!(app.visible[app.diff_cursor].new_no(), Some(7), "source lands on the block's line");
    assert!(app.reveal_diff);

    app.diff_cursor = 4; // "## Section two" (source line 5)
    app.toggle_rendered();
    assert!(app.rendered_active());
    assert_eq!(block_of(&app), (5, 5, Some(0)), "rendered lands on the heading's text row");
    assert_eq!(app.visible[app.diff_cursor].text(), "Section two");

    // A blank source line between blocks paints as the gap above the next block.
    app.toggle_rendered();
    app.diff_cursor = 5; // source line 6
    app.toggle_rendered();
    assert_eq!(app.visible[app.diff_cursor].text(), "para two");

    // Changes: a source cursor on a deletion row takes the nearest current line below it.
    enter_tab(&mut app, Tab::Changes);
    assert_eq!(app.diff_path.as_deref(), Some("edit.md"));
    app.focus = Focus::Diff;
    app.toggle_rendered();
    assert!(!app.rendered_active());
    let del = app.visible.iter().position(|row| row.marker() == '-').expect("a deletion row");
    let below = app.visible[del..].iter().find_map(Row::new_no).unwrap();
    app.diff_cursor = del;
    app.toggle_rendered();
    assert!(app.rendered_active());
    let (src, src_end, _) = block_of(&app);
    assert!((src..=src_end).contains(&below), "the deletion maps to the line below it");
    assert!(app.visible[app.diff_cursor].text().contains("gamma"));

    // And back: the diff cursor lands on that block's first line.
    app.toggle_rendered();
    assert_eq!(app.visible[app.diff_cursor].new_no(), Some(src));
}

#[test]
fn a_degraded_markdown_file_never_renders() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("empty.md", "");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("empty.md"));
    assert!(!app.rendered_active(), "an empty file shows its notice, not a render");
    app.toggle_rendered();
    assert!(!app.rendered_active(), "a file showing a notice or nothing never renders");
}

#[test]
fn a_fold_under_the_source_cursor_flips_to_its_first_hidden_line() {
    use herdr_reviewr::diff::Row;
    let body = (1..=20).fold(String::new(), |mut s, i| {
        use std::fmt::Write as _;
        let _ = write!(s, "para {i}\n\n");
        s
    });
    let r = Repo::init();
    r.write("doc.md", &body);
    r.commit_all("init");
    r.write("doc.md", &format!("{body}tail\n")); // append past the context margin
    let mut app = app_on_rendered(&r);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.focus = Focus::Diff;
    app.toggle_rendered();

    // A leading fold stands in for its first hidden line.
    assert!(app.visible[0].fold_anchor().is_some(), "a leading fold");
    app.diff_cursor = 0;
    app.toggle_rendered();
    assert!(app.rendered_active());
    assert!(matches!(app.visible[app.diff_cursor], Row::Rendered { src: 1, .. }));

    // A folded block lands on its fold; an expanded fold survives the trip.
    app.toggle_rendered();
    assert_eq!(app.diff_cursor, 0, "the block inside the fold lands on the fold");
    expand_fold(&mut app);
    let expanded = app.visible.len();
    assert!(expanded > 2, "the leading fold expanded into rows");
    app.toggle_rendered();
    app.toggle_rendered();
    assert_eq!(app.visible.len(), expanded, "the round-trip kept the fold expanded");
}

#[test]
fn a_deleted_markdown_file_never_renders_in_the_diff() {
    let r = Repo::init();
    r.write("gone.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.remove("gone.md");
    let mut app = app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("gone.md"));
    assert!(!app.rendered_active(), "a deleted file has no current content to render");
    app.toggle_rendered();
    assert!(!app.rendered_active());
}

#[test]
fn toggling_after_the_changeset_empties_is_inert() {
    let r = Repo::init();
    r.write("doc.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.write("doc.md", "# Doc\n\nbody edited\n"); // an uncommitted change puts doc.md in scope
    let mut app = app_on_rendered(&r);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.focus = Focus::Diff;
    assert!(app.rendered_active());

    // Committed, the pane and its render input clear together.
    r.commit_all("apply");
    app.reload().unwrap();
    assert!(app.visible.is_empty(), "the changeset is empty after the commit");
    assert!(!app.rendered_active());
    app.toggle_rendered();
    assert!(!app.rendered_active(), "an empty changeset never renders");
    assert!(app.visible.is_empty());
}

#[test]
fn a_scope_switch_holds_the_rendered_choice() {
    let r = Repo::init();
    r.write("doc.md", "# Doc\n\nv1\n");
    r.commit_all("init"); // on main
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("doc.md", "# Doc\n\nv2\n");
    r.commit_all("feature"); // committed: shows in branch scope
    r.write("doc.md", "# Doc\n\nv3\n"); // uncommitted: shows in uncommitted scope
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, Some("main".to_string()));
    app.reload().unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));

    assert!(!app.rendered_active(), "the diff shows the markdown source by default");
    app.toggle_rendered();
    assert!(app.rendered_active());
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"), "the same file stays open");
    assert!(app.rendered_active(), "the rendered choice holds across a scope switch");
}

#[test]
fn both_file_tabs_show_one_markdown_file_by_the_panes_choice() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("doc.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.write("doc.md", "# Doc\n\nbody edited\n");
    let mut app = app_on_rendered(&r);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));

    // Source in Changes, and so in All files.
    app.toggle_rendered();
    assert!(!app.rendered_active(), "the Changes diff flipped to source");
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    assert!(!app.rendered_active(), "All files shows it as source too");

    // Rendered in All files, and so back in Changes.
    app.toggle_rendered();
    enter_tab(&mut app, Tab::Changes);
    assert!(app.rendered_active(), "Changes follows the flip made in All files");
}

#[test]
fn the_rendered_cursor_survives_polls_resizes_and_toggles() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::Row;
    let id = |app: &App| match app.visible[app.diff_cursor] {
        Row::Rendered { src, kind: RenderedKind::Block { wrap, .. }, .. } => (src, wrap),
        _ => panic!("the cursor is on a rendered row"),
    };
    let long = "word ".repeat(40);
    let doc = |intro: &str| {
        format!(
            "# Top\n\n{intro}\n\n<details>\n<summary>More</summary>\n\nhidden body\n\n\
             </details>\n\n{long}\n\ntail\n"
        )
    };
    let r = Repo::init();
    r.write("doc.md", &doc("intro line"));
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.focus = Focus::Diff;
    app.sync_rendered_width(60);

    // The cursor rests on the long paragraph's (source line 12) second rendered line.
    app.diff_cursor = app
        .visible
        .iter()
        .position(|row| {
            matches!(
                row,
                Row::Rendered { src: 12, kind: RenderedKind::Block { wrap: Some(1), .. }, .. }
            )
        })
        .expect("the paragraph wraps at 60 columns");
    app.diff_scroll = app.diff_cursor - 1;

    // A poll that rewrites the text above the cursor keeps it on the same line.
    r.write("doc.md", &doc("intro line, now rewritten at length to change its own wrapping"));
    app.reload().unwrap();
    assert_eq!(id(&app), (12, Some(1)), "a poll edit above keeps the cursor's line");
    assert!(matches!(
        app.visible[app.diff_scroll],
        Row::Rendered { src: 12, kind: RenderedKind::Block { wrap: Some(0), .. }, .. }
    ));

    // Rewraps keep the line; a wide pane clamps the wrap.
    app.sync_rendered_width(40);
    assert_eq!(id(&app), (12, Some(1)), "a resize keeps the cursor's line");
    app.sync_rendered_width(400);
    assert_eq!(id(&app), (12, Some(0)), "a vanished wrap clamps to its line");

    // A modal freezes the view under it: a resize waits for its close, then lands.
    let frozen = app.visible.clone();
    app.mode = Mode::List;
    app.sync_rendered_width(40);
    assert_eq!(app.visible, frozen, "no rebuild under a modal");
    app.mode = Mode::Normal;
    app.sync_rendered_width(40);
    assert_ne!(app.visible, frozen, "the held resize lands once the modal closes");
    app.sync_rendered_width(400);
    assert_eq!(id(&app), (12, Some(0)));

    // Opening the `<details>` above adds rows; the cursor stays on its line.
    let rows = app.visible.len();
    app.toggle_details("More#0");
    assert!(app.visible.len() > rows, "the opened body adds rows");
    assert!(app.visible.iter().any(|row| row.text().contains("hidden body")));
    assert_eq!(id(&app), (12, Some(0)), "a details toggle keeps the cursor's line");
}

// --- world completions ---------------------------------------------------------

/// A worker completion for the app's current input, tagged `generation`.
fn completion_for(app: &App, generation: u64) -> herdr_reviewr::world::WorldCompletion {
    let input = app.world_input();
    let snapshot = herdr_reviewr::world::build(&input).unwrap();
    herdr_reviewr::world::WorldCompletion {
        generation,
        input,
        reveal: false,
        turn: None,
        snapshot: Some(Ok(snapshot)),
    }
}

#[test]
fn a_result_for_a_view_that_moved_on_is_discarded_whole() {
    let r = edited_repo();
    let mut app = app_on(&r);
    // The build ran for `uncommitted`; the reviewer switched scope before it landed.
    let stale = completion_for(&app, 7);
    app.set_scope(Scope::Branch).unwrap();
    let before = app.entries.clone();
    assert!(
        herdr_reviewr::land_world_completion(&mut app, stale, 7),
        "the live generation clears the in-flight marker even when the view moved on"
    );
    assert_eq!(app.entries, before, "the mismatched snapshot never paints");
    assert!(app.world_request.is_some(), "a fresh refresh is queued for the current view");
}

#[test]
fn a_superseded_completion_syncs_the_baseline_but_paints_nothing() {
    let r = edited_repo();
    let mut app = app_on(&r);
    r.write("d.rs", "d\n");
    let mut stale = completion_for(&app, 3);
    stale.input.turn_baseline = Some("cafe".into());
    stale.turn = Some(herdr_reviewr::world::TurnReport {
        ended: true,
        agents_present: Some(true),
        written: None,
    });
    let before = app.entries.clone();
    assert!(
        !herdr_reviewr::land_world_completion(&mut app, stale, 4),
        "a superseded tag never clears the live in-flight marker"
    );
    assert_eq!(app.entries, before, "a superseded snapshot never paints");
    assert!(app.pr_pending.is_some(), "the turn end still schedules the PR refetch");
    assert_eq!(
        app.agents_present(),
        Some(true),
        "membership syncs from a superseded completion too"
    );
    assert_eq!(
        app.world_input().turn_baseline.as_deref(),
        Some("cafe"),
        "the worker's baseline is authoritative even from a superseded completion"
    );
}

#[test]
fn a_completion_landing_mid_composition_leaves_the_frozen_diff() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    for ch in "half-written".chars() {
        app.input_push(ch);
    }
    let frozen_diff = app.diff.clone();

    // The refresh began before composing did; its result lands mid-composition.
    r.write("a.rs", "alpha\nBETA\ngamma\ndelta\nepsilon\nzeta\n");
    r.write("c.rs", "c\n");
    let early = completion_for(&app, 9);
    assert!(herdr_reviewr::land_world_completion(&mut app, early, 9));

    assert!(app.composing(), "still composing");
    assert_eq!(app.input, "half-written", "the draft is untouched");
    assert_eq!(app.diff, frozen_diff, "the frozen diff holds, however early the refresh began");
    assert!(app.entries.iter().any(|f| f.path == "c.rs"), "the file list still lands");
}

#[test]
fn a_reveal_completion_settles_the_tab_and_rearms_the_cursor_reveal() {
    let r = edited_repo();
    let mut app = app_on(&r);
    r.write("c.rs", "c\n");
    let mut landing = completion_for(&app, 2);
    landing.reveal = true;
    app.reveal_files = false;
    assert!(herdr_reviewr::land_world_completion(&mut app, landing, 2));
    assert!(app.reveal_files, "a switch-originated landing re-reveals the re-anchored cursor");
    assert!(app.entries.iter().any(|f| f.path == "c.rs"), "the landing caught up");
}

#[test]
fn a_landing_world_result_never_flips_the_hidden_navigator() {
    let r = edited_repo();
    let mut app = app_on(&r);
    app.toggle_navigator_hidden();
    r.write("c.rs", "c\n");
    let mut landing = completion_for(&app, 2);
    landing.reveal = true;
    assert!(herdr_reviewr::land_world_completion(&mut app, landing, 2));
    assert!(app.navigator_hidden, "the hidden state is place state; a landing reconciles only");
    assert_eq!(app.focus, Focus::Diff, "the settle keeps focus on the lone read pane");
}

#[test]
fn outside_a_repo_the_build_yields_the_quiet_empty_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(dir.path().to_path_buf(), Scope::Uncommitted, None);
    let snapshot = herdr_reviewr::world::build(&app.world_input()).unwrap();
    assert!(snapshot.entries.is_empty(), "no error, no entries — the empty state stays quiet");
    let build = herdr_reviewr::world::build_changed(&app.world_input()).unwrap();
    assert!(build.changeset.files.is_empty());
}

#[test]
fn a_superseded_reveal_rearms_for_the_next_dispatch() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let mut superseded = completion_for(&app, 3);
    superseded.reveal = true;
    assert!(
        !herdr_reviewr::land_world_completion(&mut app, superseded, 4),
        "the stale tag does not clear the live marker"
    );
    let request = app.world_request.expect("the undelivered reveal re-arms a refresh");
    assert!(request.reveal, "the reveal rides the next dispatch instead of dying");
}

#[test]
fn the_worker_coalesces_queued_jobs_keeping_their_flags() {
    use herdr_reviewr::world::{self, TurnHost, WorldJob};
    use std::sync::mpsc;
    let dir = tempfile::tempdir().unwrap();
    let (job_tx, job_rx) = mpsc::channel();
    let (res_tx, res_rx) = mpsc::channel();
    let input = App::new(dir.path().to_path_buf(), Scope::Uncommitted, None).world_input();
    let mut newer = input.clone();
    newer.scope = Scope::Branch;
    // Both jobs queue before the worker starts, so the coalescing path is deterministic.
    job_tx.send(WorldJob { generation: 1, input, sample_turn: true, reveal: false }).unwrap();
    job_tx
        .send(WorldJob { generation: 2, input: newer, sample_turn: false, reveal: true })
        .unwrap();
    let worker = world::spawn(TurnHost::open(dir.path().to_path_buf()), job_rx, res_tx);
    let completion = res_rx.recv().expect("one coalesced completion");
    assert_eq!(completion.generation, 2, "the latest request wins");
    assert_eq!(completion.input.scope, Scope::Branch, "the newest input is the one built");
    assert!(completion.turn.is_some(), "the superseded job's sample still runs");
    assert!(completion.reveal, "the superseded job's reveal is kept by OR");
    drop(job_tx);
    assert!(res_rx.recv().is_err(), "exactly one completion lands for the coalesced pair");
    worker.join().unwrap();
}

// --- Search overlay --------------------------------------------------

mod search_overlay {
    use super::{common, press};
    use common::{Repo, app_on, app_on_rendered, enter_tab};
    use herdr_reviewr::app::{App, Focus, Mode, SearchPhase, Tab};
    use herdr_reviewr::keymap::{Keymap, default_keymap};
    use herdr_reviewr::land_search_completion;
    use herdr_reviewr::search::{
        CodeHit, FileHit, SearchCompletion, SearchJob, SearchOutcome, SearchResults,
    };
    use ratatui::crossterm::event::KeyCode;

    fn results(files: Vec<FileHit>, code: Vec<CodeHit>) -> SearchResults {
        SearchResults { file_total: files.len(), files, code, code_more: false }
    }

    fn done(generation: u64, results: SearchResults) -> SearchCompletion {
        SearchCompletion { generation, outcome: SearchOutcome::Ready(results) }
    }

    fn file_hit(path: &str) -> FileHit {
        FileHit { path: path.into(), spans: Vec::new() }
    }

    fn code_hit(path: &str, line: u64, text: &str) -> CodeHit {
        CodeHit { path: path.into(), line, text: text.into(), spans: vec![] }
    }

    fn open(app: &mut App, keymap: &Keymap) {
        press(app, keymap, KeyCode::Char('/'));
        assert_eq!(app.mode, Mode::Search, "the search screen opens");
    }

    #[test]
    fn slash_opens_from_any_tab() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);

        // Every tab's footer carries the hint, and `/` opens from each.
        for tab in [Tab::Changes, Tab::Pr, Tab::AllFiles] {
            enter_tab(&mut app, tab);
            let actions: Vec<_> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
            assert!(
                actions.contains(&herdr_reviewr::app::FooterAction::Search),
                "the {tab:?} footer carries the search hint: {actions:?}"
            );
            open(&mut app, &keymap);
            assert!(app.search_dirty, "the open dispatches the empty query");
            press(&mut app, &keymap, KeyCode::Esc);
            assert_eq!(app.tab, tab, "esc returns to the tab it left");
        }
    }

    #[test]
    fn flip_keeps_query_and_lands_pick_on_first_row() {
        use herdr_reviewr::app::SearchMode;
        let repo = Repo::init();
        for f in ["a.rs", "b.rs", "c.rs"] {
            repo.write(f, "one\n");
        }
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);
        for c in "one".chars() {
            press(&mut app, &keymap, KeyCode::Char(c));
        }
        // A flip onto fewer rows resets the pick.
        land_search_completion(
            &mut app,
            done(
                1,
                results(
                    vec![file_hit("a.rs"), file_hit("b.rs"), file_hit("c.rs")],
                    vec![code_hit("a.rs", 1, "one")],
                ),
            ),
            1,
        );
        press(&mut app, &keymap, KeyCode::Down);
        press(&mut app, &keymap, KeyCode::Down);
        assert_eq!(app.search.as_ref().unwrap().pick, 2, "the pick moved off the first row");

        press(&mut app, &keymap, KeyCode::Tab);
        let s = app.search.as_ref().unwrap();
        assert_eq!(s.search_mode, SearchMode::Code);
        assert_eq!(s.query, "one", "the flip keeps the query");
        assert_eq!(s.pick, 0, "the flip lands the pick on the first result row");
        assert!(s.picked().is_some(), "the reset pick points at a real code result");
        assert_eq!(s.picks(), 1, "the held code results paint at once");

        // Move within Code, flip back to Files: the pick resets there too.
        press(&mut app, &keymap, KeyCode::Tab);
        let s = app.search.as_ref().unwrap();
        assert_eq!(s.search_mode, SearchMode::Files);
        assert_eq!(s.pick, 0, "flipping back resets the pick to the first file row");
    }

    /// A poll reconciles the preview, never the results or pick (Continuity).
    #[test]
    fn poll_never_reshapes_results_or_pick() {
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.write("b.rs", "two\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);
        land_search_completion(
            &mut app,
            done(3, results(vec![file_hit("a.rs"), file_hit("b.rs")], Vec::new())),
            3,
        );
        press(&mut app, &keymap, KeyCode::Down);
        let before = app.search.as_ref().unwrap();
        let (results_before, pick_before) = (before.results.clone(), before.pick);

        // A full synchronous reconcile — the poll path.
        app.reload().unwrap();

        let after = app.search.as_ref().unwrap();
        assert_eq!(
            after.results, results_before,
            "a poll leaves the result set and counts untouched"
        );
        assert_eq!(after.pick, pick_before, "a poll never moves the pick");
    }

    #[test]
    fn superseded_result_never_paints() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);

        let stale = done(1, results(vec![file_hit("a.rs")], Vec::new()));
        assert!(!land_search_completion(&mut app, stale, 2), "stale generation");
        let s = app.search.as_ref().unwrap();
        assert!(s.results.files.is_empty(), "a superseded result set never paints");
        assert_eq!(s.phase, SearchPhase::Indexing);

        let live = done(2, results(vec![file_hit("a.rs")], Vec::new()));
        assert!(land_search_completion(&mut app, live, 2));
        assert_eq!(app.search.as_ref().unwrap().results.files.len(), 1);
    }

    #[test]
    fn open_lands_on_clamped_line() {
        let repo = Repo::init();
        repo.write("a.rs", "one\ntwo\nthree\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);

        let hit = code_hit("a.rs", 99, "three");
        land_search_completion(&mut app, done(1, results(Vec::new(), vec![hit])), 1);
        press(&mut app, &keymap, KeyCode::Tab); // code results live in `Code` mode
        press(&mut app, &keymap, KeyCode::Enter);

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.search.is_none());
        assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
        assert_eq!(app.focus, Focus::Diff);
        assert_eq!(app.diff_cursor, app.visible.len() - 1, "line 99 clamps to the last row");
        assert_eq!(app.search_track.as_deref(), Some("a.rs"), "the pick feeds frecency");
    }

    #[test]
    fn a_code_hit_in_markdown_lands_on_its_rendered_block() {
        let repo = Repo::init();
        repo.write("doc.md", "# Title\n\nfirst para\n\nsecond para\nwith the needle\n\ntail\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on_rendered(&repo);
        enter_tab(&mut app, Tab::AllFiles);

        // A content hit on the paragraph's second line lands on that paragraph, rendered.
        open(&mut app, &keymap);
        let hit = code_hit("doc.md", 6, "with the needle");
        land_search_completion(&mut app, done(1, results(Vec::new(), vec![hit])), 1);
        press(&mut app, &keymap, KeyCode::Tab);
        press(&mut app, &keymap, KeyCode::Enter);
        assert!(app.rendered_active(), "the markdown file stays rendered");
        assert_eq!(app.visible[app.diff_cursor].text(), "second para with the needle");

        // A file pick lands at the top, as for any file.
        open(&mut app, &keymap);
        land_search_completion(&mut app, done(2, results(vec![file_hit("doc.md")], Vec::new())), 2);
        press(&mut app, &keymap, KeyCode::Enter);
        assert!(app.rendered_active());
        assert_eq!(app.diff_cursor, 0);
    }

    #[test]
    fn file_pick_moves_selection_and_expands_ancestors() {
        let repo = Repo::init();
        repo.write("src/deep/a.rs", "fn a() {}\n");
        repo.write("top.rs", "fn t() {}\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);

        land_search_completion(
            &mut app,
            done(1, results(vec![file_hit("src/deep/a.rs")], Vec::new())),
            1,
        );
        press(&mut app, &keymap, KeyCode::Enter);

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.diff_path.as_deref(), Some("src/deep/a.rs"));
        let row = &app.file_rows[app.file_cursor];
        let idx = row.file_index().expect("the selection lands on the file's row");
        assert_eq!(app.entries[idx].path, "src/deep/a.rs");
    }

    #[test]
    fn esc_restores_place_untouched() {
        let repo = Repo::init();
        repo.write("a.rs", "one\ntwo\n");
        repo.write("b.rs", "one\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        press(&mut app, &keymap, KeyCode::Down);
        let place = (
            app.tab,
            app.focus,
            app.file_cursor,
            app.file_scroll,
            app.diff_cursor,
            app.diff_scroll,
            app.diff_path.clone(),
        );

        open(&mut app, &keymap);
        for c in "registry".chars() {
            press(&mut app, &keymap, KeyCode::Char(c));
        }
        assert_eq!(app.search.as_ref().unwrap().query, "registry");
        press(&mut app, &keymap, KeyCode::Esc);

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.search.is_none(), "the query drops with the overlay");
        let after = (
            app.tab,
            app.focus,
            app.file_cursor,
            app.file_scroll,
            app.diff_cursor,
            app.diff_scroll,
            app.diff_path.clone(),
        );
        assert_eq!(place, after, "esc leaves the place exactly as it was");
    }

    #[test]
    fn vanished_path_keeps_overlay() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);

        land_search_completion(
            &mut app,
            done(1, results(vec![file_hit("missing.rs")], Vec::new())),
            1,
        );
        press(&mut app, &keymap, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Search, "a vanished path opens nothing, the overlay stays");
    }

    #[test]
    fn config_error_closes_overlay_and_drops_query() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);
        press(&mut app, &keymap, KeyCode::Char('x'));

        app.set_config_error("bad config".into());
        assert!(app.search.is_none(), "the overlay closes when the config view takes over");
        assert_ne!(app.mode, Mode::Search);
    }

    /// After an error, `enter` opens nothing from the hidden stale rows.
    #[test]
    fn error_phase_makes_stale_results_inert() {
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);

        land_search_completion(&mut app, done(1, results(vec![file_hit("a.rs")], Vec::new())), 1);
        app.build_search_preview();
        assert!(app.search.as_ref().unwrap().preview.is_some(), "a preview builds for the pick");
        let error =
            SearchCompletion { generation: 2, outcome: SearchOutcome::Failed("boom".into()) };
        land_search_completion(&mut app, error, 2);
        assert_eq!(app.search.as_ref().unwrap().phase, SearchPhase::Error("boom".into()));
        assert!(!app.search.as_ref().unwrap().results.files.is_empty(), "results held");
        // The stale preview clears, so no unrelated file shows below the red error.
        assert!(app.search.as_ref().unwrap().preview.is_none(), "the error drops the preview");

        press(&mut app, &keymap, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Search, "enter opens nothing off an error frame");
        press(&mut app, &keymap, KeyCode::Down);
        assert_eq!(app.search.as_ref().unwrap().pick, 0, "arrows are inert too");
    }

    /// With nothing pickable the footer offers only the exit.
    #[test]
    fn footer_offers_only_esc_when_nothing_pickable() {
        use herdr_reviewr::app::{Band, FooterAction};
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);

        let flip = FooterAction::FlipSearchMode;
        // Warming: the mode flip and esc only.
        assert_eq!(
            app.footer_bands(),
            vec![(flip, Band::Primary), (FooterAction::CloseSearch, Band::Do)],
            "indexing offers only the flip and esc"
        );
        // Ready but empty: the same.
        land_search_completion(&mut app, done(1, results(Vec::new(), Vec::new())), 1);
        assert_eq!(
            app.footer_bands(),
            vec![(flip, Band::Primary), (FooterAction::CloseSearch, Band::Do)],
            "no matches offers only the flip and esc"
        );
        // Ready with results: the full bar.
        land_search_completion(&mut app, done(2, results(vec![file_hit("a.rs")], Vec::new())), 2);
        let actions: Vec<_> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
        assert_eq!(
            actions,
            vec![
                flip,
                FooterAction::PickResult,
                FooterAction::OpenResult,
                FooterAction::CloseSearch
            ]
        );
    }

    /// A divider drag cancelled by `/` releases without picking.
    #[test]
    fn cancelled_divider_drag_releases_on_mouse_up_in_search() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);

        let area = Rect::new(0, 0, 120, 40);
        let body = herdr_reviewr::ui::body_rect(area, &app);
        let row = body.y + body.height / 2;
        let divider = (body.x..body.x + body.width)
            .find(|&col| herdr_reviewr::ui::hit_divider(area, &app, col, row))
            .unwrap();
        let heights = vec![1usize; app.visible.len()];
        let event = |kind, column| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        herdr_reviewr::handle_mouse(
            &mut app,
            event(MouseEventKind::Down(MouseButton::Left), divider),
            area,
            &heights,
            &keymap,
            &herdr_reviewr::export::Clipboard,
        )
        .unwrap();

        // A landed result makes a stray pick observable.
        open(&mut app, &keymap);
        land_search_completion(&mut app, done(1, results(vec![file_hit("a.rs")], Vec::new())), 1);
        assert!(app.divider_drag_captured(), "the cancelled gesture still owns its events");

        herdr_reviewr::handle_mouse(
            &mut app,
            event(MouseEventKind::Up(MouseButton::Left), divider),
            area,
            &heights,
            &keymap,
            &herdr_reviewr::export::Clipboard,
        )
        .unwrap();
        assert!(!app.divider_drag_captured(), "mouse-up releases the capture");
        assert_eq!(app.mode, Mode::Search, "the release never resolves into a pick");
    }

    /// The query edits like a draft; a paste's newlines become spaces.
    #[test]
    fn query_edits_with_comment_editor_controls() {
        use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
        fn key(app: &mut App, keymap: &Keymap, code: KeyCode, mods: KeyModifiers) {
            let area = ratatui::layout::Rect::new(0, 0, 120, 40);
            herdr_reviewr::handle_key(app, KeyEvent::new(code, mods), area, keymap).unwrap();
        }
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let none = KeyModifiers::NONE;

        for c in "foo bar".chars() {
            key(&mut app, &keymap, KeyCode::Char(c), none);
        }
        key(&mut app, &keymap, KeyCode::Char('w'), ctrl); // delete the word before the caret
        let q = |app: &App| app.search.as_ref().unwrap().query.clone();
        let caret = |app: &App| app.search.as_ref().unwrap().caret;
        assert_eq!(q(&app), "foo ");

        key(&mut app, &keymap, KeyCode::Char('b'), alt); // word left
        assert_eq!(caret(&app), 0);
        key(&mut app, &keymap, KeyCode::Char('x'), none); // insert mid-string, at the caret
        assert_eq!(q(&app), "xfoo ");
        key(&mut app, &keymap, KeyCode::End, none);
        key(&mut app, &keymap, KeyCode::Backspace, none);
        assert_eq!(q(&app), "xfoo");
        key(&mut app, &keymap, KeyCode::Home, none);
        key(&mut app, &keymap, KeyCode::Delete, none);
        assert_eq!(q(&app), "foo");
        key(&mut app, &keymap, KeyCode::Char('k'), ctrl); // kill to end from the start
        assert_eq!(q(&app), "");
        assert!(app.search_dirty, "an edit re-queries");

        app.input_paste("multi\nline");
        assert_eq!(q(&app), "multi line", "a paste's newlines become spaces");
    }

    /// `ctrl+n`/`ctrl+p` move the pick; plain `n`/`p` type.
    #[test]
    fn ctrl_n_p_move_the_pick() {
        use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
        fn key(app: &mut App, keymap: &Keymap, code: KeyCode, mods: KeyModifiers) {
            let area = ratatui::layout::Rect::new(0, 0, 120, 40);
            herdr_reviewr::handle_key(app, KeyEvent::new(code, mods), area, keymap).unwrap();
        }
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);
        land_search_completion(
            &mut app,
            done(
                1,
                results(vec![file_hit("a.rs"), file_hit("b.rs"), file_hit("c.rs")], Vec::new()),
            ),
            1,
        );
        let ctrl = KeyModifiers::CONTROL;
        let pick = |app: &App| app.search.as_ref().unwrap().pick;

        key(&mut app, &keymap, KeyCode::Char('n'), ctrl);
        assert_eq!(pick(&app), 1, "ctrl+n moves the pick down");
        key(&mut app, &keymap, KeyCode::Char('n'), ctrl);
        assert_eq!(pick(&app), 2);
        key(&mut app, &keymap, KeyCode::Char('p'), ctrl);
        assert_eq!(pick(&app), 1, "ctrl+p moves the pick up");
        // Plain n types into the query.
        key(&mut app, &keymap, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(app.search.as_ref().unwrap().query, "n", "plain n still types");
    }

    /// The preview rebuilds on settle for a new pick, carrying its hit, and not otherwise.
    #[test]
    fn preview_builds_on_settle_with_hit() {
        let repo = Repo::init();
        repo.write("a.rs", "one\ntwo\nthree\n");
        repo.write("b.rs", "four\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);
        land_search_completion(
            &mut app,
            done(
                1,
                results(Vec::new(), vec![code_hit("a.rs", 2, "two"), code_hit("b.rs", 1, "four")]),
            ),
            1,
        );
        press(&mut app, &keymap, KeyCode::Tab);
        assert!(app.search.as_ref().unwrap().preview.is_none(), "nothing builds before the settle");

        app.build_search_preview();
        {
            let pv = app.search.as_ref().unwrap().preview.as_ref().unwrap();
            assert_eq!(pv.path, "a.rs");
            assert_eq!(pv.hit.as_ref().unwrap().0, 2, "the code pick carries its hit line");
            assert!(pv.center.get(), "the renderer centers the hit once per build");
        }

        press(&mut app, &keymap, KeyCode::Down);
        assert_eq!(
            app.search.as_ref().unwrap().preview.as_ref().unwrap().path,
            "a.rs",
            "the preview lags the moved pick until it settles",
        );
        app.build_search_preview();
        assert_eq!(
            app.search.as_ref().unwrap().preview.as_ref().unwrap().path,
            "b.rs",
            "the settle rebuilds onto the new pick",
        );

        // Unchanged, no rebuild, so the scroll survives.
        app.scroll_search_preview(1);
        app.build_search_preview();
        assert!(
            !app.search.as_ref().unwrap().preview.as_ref().unwrap().center.get(),
            "an unchanged pick is not rebuilt on settle",
        );
    }

    /// A poll repaints the preview in place; a deleted file previews empty.
    #[test]
    fn poll_repaints_preview_in_place() {
        let repo = Repo::init();
        repo.write("a.rs", "one\ntwo\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);
        land_search_completion(&mut app, done(1, results(vec![file_hit("a.rs")], Vec::new())), 1);
        app.build_search_preview();
        let rows =
            |app: &App| app.search.as_ref().unwrap().preview.as_ref().unwrap().diff.rows.len();
        assert_eq!(rows(&app), 2);
        app.scroll_search_preview(1);

        repo.write("a.rs", "one\ntwo\nthree\n");
        app.refresh_search_preview();
        let s = app.search.as_ref().unwrap();
        assert_eq!(rows(&app), 3, "the poll's reconcile repaints the preview in place");
        let pv = s.preview.as_ref().unwrap();
        assert_eq!(pv.scroll.get(), 1, "the scroll survives the repaint");
        assert!(!pv.center.get(), "a repaint never re-centers");

        std::fs::remove_file(repo.path().join("a.rs")).unwrap();
        app.refresh_search_preview();
        assert_eq!(rows(&app), 0, "a deleted previewed file previews empty");
    }

    /// Opening lands in `All files`; the origin tab keeps its place.
    #[test]
    fn open_from_changes_lands_in_all_files_keeping_origin_place() {
        let repo = Repo::init();
        repo.write("a.rs", "one\ntwo\n");
        repo.write("b.rs", "three\n");
        repo.commit_all("c");
        repo.write("a.rs", "one\nchanged\n");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        assert_eq!(app.tab, Tab::Changes);
        press(&mut app, &keymap, KeyCode::Down);
        let origin_cursor = app.diff_cursor;

        open(&mut app, &keymap);
        land_search_completion(&mut app, done(1, results(vec![file_hit("b.rs")], Vec::new())), 1);
        press(&mut app, &keymap, KeyCode::Enter);
        assert_eq!(app.tab, Tab::AllFiles, "the open lands in All files");
        assert_eq!(app.diff_path.as_deref(), Some("b.rs"));
        assert_eq!(app.focus, Focus::Diff);

        enter_tab(&mut app, Tab::Changes);
        assert_eq!(app.diff_cursor, origin_cursor, "the origin tab keeps its place");
    }

    /// The search divider drags only search's own share.
    #[test]
    fn search_divider_drags_only_the_search_share() {
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let keymap = default_keymap().clone();
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        open(&mut app, &keymap);
        let side = app.navigator_side_pct;
        let stack = app.navigator_stack_pct;
        assert_eq!(app.search_pct, 50, "half the body by default");

        app.start_divider_drag();
        app.drag_search_divider(40, 30);
        assert_eq!(app.search_pct, 75);
        app.finish_divider_drag();
        assert_eq!(app.navigator_side_pct, side, "the review shares are untouched");
        assert_eq!(app.navigator_stack_pct, stack);
    }

    #[test]
    fn opening_search_mid_navigator_drag_does_not_hijack_it() {
        // A held review drag is cancelled on open.
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        app.start_divider_drag();
        assert!(app.divider_drag_active(), "a navigator drag is in flight");
        let before = app.search_pct;
        app.open_search();
        assert!(!app.divider_drag_active(), "opening search cancels the carried drag");
        app.drag_search_divider(40, 30);
        assert_eq!(app.search_pct, before, "the carried gesture never resizes the search split");
    }

    /// Every path under `root`, relative, `.git` included — the worktree-purity probe.
    fn all_paths(root: &std::path::Path) -> Vec<String> {
        fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else if path.extension().is_none_or(|e| e != "lock") {
                    // git's own locks (CI's `maintenance.lock`) aren't reviewr's writes.
                    out.push(path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// The real engine: results arrive, never ignored files or `.git`, and nothing is written.
    #[test]
    fn engine_worker_end_to_end() {
        let repo = Repo::init();
        // The match sits behind a tab indent, so the worker's leading-strip is exercised.
        repo.write("src/alpha.rs", "fn wrap() {\n\t\talpha_marker();\n}\n");
        repo.write(".gitignore", "ignored.txt\n");
        repo.commit_all("c");
        repo.write("ignored.txt", "alpha_marker inside an ignored file\n");
        let cache = tempfile::TempDir::new().unwrap();
        let before = all_paths(repo.path());

        let (job_tx, job_rx) = std::sync::mpsc::channel();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let worker =
            herdr_reviewr::search::spawn(repo.path_buf(), cache.path().into(), job_rx, res_tx);
        job_tx.send(SearchJob::Query { generation: 1, query: "alpha_marker".into() }).unwrap();

        // A warming engine answers `indexing…` first and re-runs by itself.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let results = loop {
            let completion = res_rx
                .recv_timeout(deadline - std::time::Instant::now())
                .expect("the worker answers before the deadline");
            assert_eq!(completion.generation, 1);
            match completion.outcome {
                SearchOutcome::Ready(results) => break results,
                SearchOutcome::Indexing => {}
                SearchOutcome::Failed(e) => panic!("the engine failed: {e}"),
            }
        };

        let paths: Vec<&str> = results
            .files
            .iter()
            .map(|f| f.path.as_str())
            .chain(results.code.iter().map(|c| c.path.as_str()))
            .collect();
        assert!(paths.contains(&"src/alpha.rs"), "the engine finds the file: {paths:?}");
        assert!(
            !paths.iter().any(|p| *p == "ignored.txt" || p.starts_with(".git")),
            "ignored files and .git are not searchable: {paths:?}"
        );
        // The code hit's leading indentation is stripped so the row aligns left
        let code = results.code.iter().find(|c| c.path == "src/alpha.rs");
        if let Some(hit) = code {
            assert!(
                !hit.text.starts_with([' ', '\t']),
                "the worker strips the match line's leading indentation: {:?}",
                hit.text
            );
        }

        job_tx.send(SearchJob::Track { path: "src/alpha.rs".into() }).unwrap();
        drop(job_tx);
        worker.join().unwrap();
        assert_eq!(all_paths(repo.path()), before, "search writes nothing to the worktree");
        assert!(
            cache.path().join("frecency").exists(),
            "the frecency store lives under the cache dir"
        );
    }
}

// --- Agent picker ------------------------------------

fn choice(pane: &str, name: &str) -> AgentChoice {
    AgentChoice { pane_id: pane.into(), name: name.into(), state: "idle".into(), tab: "1".into() }
}

fn three_agents() -> Vec<AgentChoice> {
    vec![choice("w8:p1", "claude"), choice("w8:p2", "release-bot"), choice("w8:p3", "codex")]
}

/// An app with two comments written and the picker open over three agents.
fn app_with_picker(r: &Repo) -> App {
    let mut app = app_on(r);
    comment_on(&mut app, '+', "one");
    comment_on(&mut app, '-', "two");
    app.open_picker(three_agents());
    app
}

#[test]
fn the_highlight_arms_the_last_sent_agent_else_row_one() {
    let r = edited_repo();
    // Through `open_picker`, as the send calls it.
    let armed = |last_sent: Option<&str>| {
        let mut app = app_on(&r);
        app.last_sent_pane = last_sent.map(str::to_string);
        app.open_picker(three_agents());
        app.picker_cursor
    };
    // The last-sent agent wins whenever it is still a candidate.
    assert_eq!(armed(Some("w8:p3")), 2);
    // Nothing sent this session, or a last-sent pane that has since closed: the first row.
    assert_eq!(armed(None), 0);
    assert_eq!(armed(Some("w8:pZ")), 0);
}

#[test]
fn the_picker_moves_by_key_and_a_digit_past_the_last_row_is_inert() {
    let r = edited_repo();
    let mut app = app_with_picker(&r);
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);
    assert_eq!(app.picker_cursor, 0);

    // Through `handle_key`, so the picker's movement bindings are proven live, not just the verb.
    handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')), area, &keymap).unwrap();
    assert_eq!(app.picker_cursor, 1, "`j` moves the highlight down");
    handle_key(&mut app, KeyEvent::from(KeyCode::Char('k')), area, &keymap).unwrap();
    assert_eq!(app.picker_cursor, 0, "`k` moves it back up");
    handle_key(&mut app, KeyEvent::from(KeyCode::Down), area, &keymap).unwrap();
    assert_eq!(app.picker_cursor, 1, "the arrows move it too");

    app.picker_goto(2);
    assert_eq!(app.picker_cursor, 2);
    // A mistyped digit must not arm a neighbour the reviewer would then send to.
    handle_key(&mut app, KeyEvent::from(KeyCode::Char('7')), area, &keymap).unwrap();
    assert_eq!(app.picker_cursor, 2, "a row past the end is inert, not clamped");
}

#[test]
fn the_picker_follows_a_down_rebind_like_the_main_view() {
    let r = edited_repo();
    let mut app = app_with_picker(&r);
    let keymap = Keymap::resolve(&[(Action::Down, vec![Key::plain('x')])]).unwrap();
    let area = Rect::new(0, 0, 80, 24);
    assert_eq!(app.picker_cursor, 0);

    handle_key(&mut app, KeyEvent::from(KeyCode::Char('x')), area, &keymap).unwrap();
    assert_eq!(app.picker_cursor, 1, "the rebound key moves the highlight");
    handle_key(&mut app, KeyEvent::from(KeyCode::Down), area, &keymap).unwrap();
    assert_eq!(app.picker_cursor, 1, "the freed arrow no longer moves it");
}

#[test]
fn cancelling_the_picker_keeps_every_comment() {
    let r = edited_repo();
    let mut app = app_with_picker(&r);
    let keymap = Keymap::default();

    handle_key(&mut app, KeyEvent::from(KeyCode::Esc), Rect::new(0, 0, 80, 24), &keymap).unwrap();
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.store.len(), 2, "cancelling consumes nothing");
    assert!(app.picker_rows.is_empty(), "the frozen rows are dropped with the picker");
}

// `last used` arming is proven end to end in tests/send_flow.rs.

#[test]
fn a_picker_opened_from_the_comments_list_closes_back_onto_it() {
    let r = edited_repo();
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "one");
    comment_on(&mut app, '-', "two");
    app.open_list();
    app.open_picker(three_agents());
    assert_eq!(app.mode, Mode::Picker);

    let keymap = Keymap::default();
    handle_key(&mut app, KeyEvent::from(KeyCode::Esc), Rect::new(0, 0, 80, 24), &keymap).unwrap();
    assert_eq!(app.mode, Mode::List, "cancelling restores the list the reviewer was browsing");
    assert_eq!(app.store.len(), 2, "cancelling consumes nothing");

    // Cancelling the picker restores the find band too.
    app.close_list();
    app.open_find();
    let over_find = app.mode.clone();
    app.open_picker(three_agents());
    handle_key(&mut app, KeyEvent::from(KeyCode::Esc), Rect::new(0, 0, 80, 24), &keymap).unwrap();
    assert_eq!(app.mode, over_find, "cancelling restores the find band");
}

#[test]
fn the_picker_swallows_every_key_it_does_not_bind() {
    let r = edited_repo();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);

    // In the picker `q` doesn't quit and `y` doesn't copy.
    for code in [KeyCode::Char('q'), KeyCode::Char('y'), KeyCode::Char('r'), KeyCode::Char('s')] {
        let mut app = app_with_picker(&r);
        let rows_before = app.picker_rows.clone();
        let status_before = app.status.clone();
        handle_key(&mut app, KeyEvent::from(code), area, &keymap).unwrap();
        assert!(!app.should_quit, "{code:?} quit the app from the picker");
        assert_eq!(app.mode, Mode::Picker, "{code:?} left the picker");
        assert_eq!(app.store.len(), 2, "{code:?} consumed comments from the picker");
        // These change even when the action fails, so they catch a leak anywhere.
        assert_eq!(app.picker_rows, rows_before, "{code:?} rebuilt the frozen rows");
        assert_eq!(app.status, status_before, "{code:?} acted and reported from the picker");
    }
}

#[test]
fn a_chord_never_fires_the_pickers_irreversible_send() {
    let r = edited_repo();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);

    // A chorded `enter`, the editor's newline, must not send.
    for modifiers in [KeyModifiers::ALT, KeyModifiers::SHIFT, KeyModifiers::CONTROL] {
        let mut app = app_with_picker(&r);
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, modifiers), area, &keymap).unwrap();
        assert_eq!(app.mode, Mode::Picker, "{modifiers:?}+enter left the picker");
        assert_eq!(app.store.len(), 2, "{modifiers:?}+enter consumed the review");

        // Nor may a chorded digit move the highlight.
        let mut app = app_with_picker(&r);
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('3'), modifiers), area, &keymap).unwrap();
        assert_eq!(app.picker_cursor, 0, "{modifiers:?}+3 armed a row");
    }

    // `esc` cancels whatever the modifiers.
    let mut app = app_with_picker(&r);
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::SHIFT), area, &keymap).unwrap();
    assert_eq!(app.mode, Mode::Normal, "a modified `esc` still cancels");
    assert_eq!(app.store.len(), 2, "cancelling consumes nothing");
}

#[test]
fn the_picker_owns_its_keys_on_every_tab() {
    use herdr_reviewr::app::Tab;

    let r = edited_repo();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);

    // On `PR`, the picker still outranks the tab's `q` and digits.
    let mut app = app_with_picker(&r);
    app.tab = Tab::Pr;
    handle_key(&mut app, KeyEvent::from(KeyCode::Char('q')), area, &keymap).unwrap();
    assert!(!app.should_quit, "`q` quit the app from a picker on the PR tab");
    assert_eq!(app.mode, Mode::Picker, "`q` left the picker");
    handle_key(&mut app, KeyEvent::from(KeyCode::Char('1')), area, &keymap).unwrap();
    assert_eq!(app.tab, Tab::Pr, "`1` switched tabs behind the picker");
    assert_eq!(app.picker_cursor, 0, "`1` moved the highlight, as the picker's own key");
    handle_key(&mut app, KeyEvent::from(KeyCode::Esc), area, &keymap).unwrap();
    assert_eq!(app.mode, Mode::Normal, "`esc` still cancels from the PR tab");
}

#[test]
fn a_second_open_never_stacks_a_picker_that_one_esc_cannot_leave() {
    let r = edited_repo();
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);

    // A second open is ignored, keeping the frozen rows.
    let mut app = app_with_picker(&r);
    app.picker_goto(2);
    app.open_picker(vec![choice("w8:p9", "other")]);
    assert_eq!(app.picker_rows, three_agents(), "the second open replaced the frozen rows");
    assert_eq!(app.picker_cursor, 2, "the second open moved the highlight");
    handle_key(&mut app, KeyEvent::from(KeyCode::Esc), area, &keymap).unwrap();
    assert_eq!(app.mode, Mode::Normal, "one `esc` leaves the picker");

    // A picker over no rows has nothing to choose and no `enter` that acts, so it never opens.
    let mut app = app_on(&r);
    comment_on(&mut app, '+', "one");
    app.open_picker(Vec::new());
    assert_eq!(app.mode, Mode::Normal, "an empty row set opens no modal");
}

#[test]
fn the_picker_digits_are_literal_whatever_the_tab_keys_are_bound_to() {
    let r = edited_repo();
    let mut app = app_with_picker(&r);
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 80, 24);
    let tab_before = app.tab;

    handle_key(&mut app, KeyEvent::from(KeyCode::Char('2')), area, &keymap).unwrap();
    assert_eq!(app.picker_cursor, 1, "`2` moved the highlight to row 2");
    assert_eq!(app.tab, tab_before, "`2` did not switch tabs from inside the picker");
}

#[test]
fn a_refresh_behind_the_picker_moves_neither_the_rows_nor_the_place() {
    let r = edited_repo();
    let mut app = app_with_picker(&r);
    app.picker_goto(2);
    let rows_before = app.picker_rows.clone();
    let file_before = app.diff_path.clone();
    let cursor_before = app.diff_cursor;
    let frozen_diff = app.diff.clone();

    // Rewrite the open file, since only that would rebuild the frozen diff.
    r.write("a.rs", "alpha\nBETA\ngamma\ndelta\nepsilon\nzeta\n");
    r.write("b.rs", "new\n");
    app.reload().unwrap();

    assert_eq!(app.picker_rows, rows_before, "the frozen rows never reorder or change");
    assert_eq!(app.picker_cursor, 2, "the highlight stays where the reviewer put it");
    assert_eq!(app.diff_path, file_before, "the place behind the picker is frozen");
    assert_eq!(app.diff_cursor, cursor_before);
    assert_eq!(app.diff, frozen_diff, "the open diff is frozen while the picker is up");
    assert!(app.entries.iter().any(|f| f.path == "b.rs"), "the file list still refreshes");
}

#[test]
fn a_config_error_closes_the_picker_and_keeps_the_comments() {
    let r = edited_repo();
    let mut app = app_with_picker(&r);

    app.set_config_error("theme = \"not-a-theme\"".to_string());
    assert_eq!(app.mode, Mode::Normal, "the picker's rows would be stale after recovery");
    assert!(app.picker_rows.is_empty());
    assert_eq!(app.store.len(), 2, "saved comments always survive a config error");

    // Over the find band, recovery never restores a mode without its state.
    let mut over_find = app_on(&r);
    comment_on(&mut over_find, '+', "one");
    over_find.open_find();
    over_find.open_picker(three_agents());
    over_find.set_config_error("theme = \"not-a-theme\"".to_string());
    assert_eq!(over_find.mode, Mode::Normal, "the find band closes with the picker it held");

    // Over the comments list, recovery lands back in the list.
    let mut over_list = app_on(&r);
    comment_on(&mut over_list, '+', "one");
    over_list.open_list();
    over_list.open_picker(three_agents());
    over_list.set_config_error("theme = \"not-a-theme\"".to_string());
    assert_eq!(over_list.mode, Mode::List, "the list outlives the picker it held");
    assert!(over_list.picker_rows.is_empty(), "the frozen rows would be stale after recovery");
    assert_eq!(over_list.store.len(), 1, "saved comments always survive a config error");
}

#[test]
fn the_picker_owns_the_whole_footer_bar() {
    let r = edited_repo();
    let app = app_with_picker(&r);
    let bands: Vec<FooterAction> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
    assert_eq!(
        bands,
        vec![FooterAction::PickAgent, FooterAction::ClosePicker, FooterAction::MovePickerRow]
    );
}

// --- Base picker -----------------------

/// Highlight base row `name`; same-second tips tie, so tests go by name.
fn goto_row(app: &mut App, name: &str) {
    let bp = app.base_picker.as_ref().expect("picker open");
    let i = bp.visible().iter().position(|r| r.name() == name).expect("row listed");
    app.base_picker_goto(i);
}

/// Branch `feature` beside `dev` and default `main`, with one committed edit.
fn based_repo() -> Repo {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["branch", "dev"]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("a.rs", "two\n");
    r.commit_all("feature work");
    r
}

#[test]
fn the_base_picker_opens_on_every_scope_without_a_flag_and_a_pick_switches_to_branch() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.open_base_picker();
    assert_eq!(app.mode, Mode::BasePick, "the picker opens off the branch scope too");
    app.close_base_picker();
    assert_eq!(app.scope, Scope::Uncommitted, "a cancel leaves the scope alone");
    app.open_base_picker();
    goto_row(&mut app, "dev");
    app.base_picker_pick().unwrap();
    assert_eq!(app.scope, Scope::Branch, "a pick switches to the scope it configures");
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("dev")
    );

    app.set_scope(Scope::Uncommitted).unwrap();
    assert!(
        app.footer_bands().iter().any(|&(a, b)| a == FooterAction::BasePick && b == Band::Go),
        "the go band carries the key on every scope"
    );
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    assert_eq!(app.mode, Mode::BasePick);
    let bp = app.base_picker.as_ref().expect("picker state");
    let names: Vec<&str> = bp.rows.iter().map(herdr_reviewr::app::BaseChoice::name).collect();
    assert!(names.contains(&"feature"), "the checked-out branch is a row like any other");
    let feature = bp.rows.iter().find(|r| r.name() == "feature").unwrap();
    assert!(feature.current() && !feature.is_default());
    assert_eq!(bp.rows[0].name(), "main", "the default branch sorts ahead of recency");
    assert!(bp.rows[0].is_default() && !bp.rows[0].current());
    assert!(bp.rows.iter().all(|r| r.tip_secs() > 0), "every branch row carries its tip time");
    assert!(names.contains(&"dev"));
    assert_eq!(bp.rows[bp.cursor].name(), "dev", "the highlight opens on the current base");
    app.close_base_picker();
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.base_picker.is_none());

    // A `--base` flag pins the base for the session, so the picker never opens.
    let mut flagged = App::new(r.path_buf(), Scope::Branch, Some("dev".to_string()));
    flagged.reload().unwrap();
    flagged.open_base_picker();
    assert_eq!(flagged.mode, Mode::Normal, "the flag disables the picker");
}

#[test]
fn typing_filters_and_enter_picks_the_highlight() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("main")
    );
    app.open_base_picker();
    app.input_push('d');
    app.input_push('e');
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.filtered().len(), 1, "the filter matches anywhere in the name");
    app.base_picker_pick().unwrap();
    assert_eq!(app.mode, Mode::Normal, "a pick closes the picker");
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("dev")
    );
    let picked = r.git(&["show", "refs/worktree/reviewr/base-pick"]);
    assert_eq!(picked.trim(), "dev", "the pick persists in the private ref");
    assert!(
        app.entries.iter().any(|e| e.path == "a.rs"),
        "the changeset rebuilds against the picked base before the frame"
    );
}

#[test]
fn a_pick_retags_the_world_input() {
    // A pick bumps the tag, so an earlier build can't land and revert it.
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let stale = app.world_input();
    app.open_base_picker();
    app.input_push('d');
    app.base_picker_pick().unwrap();
    assert_ne!(app.world_input(), stale, "an in-flight build's tag no longer matches");
}

#[test]
fn picking_the_default_deletes_the_pick() {
    let r = based_repo();
    herdr_reviewr::git::write_base_pick(r.path(), "dev").unwrap();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("dev")
    );
    app.open_base_picker();
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.rows[bp.cursor].name(), "dev", "the highlight opens on the current base");
    app.base_picker_goto(0);
    app.base_picker_pick().unwrap();
    assert_eq!(app.scope, Scope::Branch);
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("main")
    );
    assert_eq!(
        herdr_reviewr::git::read_base_pick(r.path()).unwrap(),
        None,
        "the default row is the way back: the ref is gone, so a re-default is followed"
    );
    assert!(
        app.entries.iter().any(|e| e.path == "a.rs"),
        "the changeset rebuilds against the default before the frame"
    );

    // Enter on the default row with no pick writes nothing either.
    let stale = app.world_input();
    app.open_base_picker();
    app.base_picker_pick().unwrap();
    assert_eq!(herdr_reviewr::git::read_base_pick(r.path()).unwrap(), None);
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("main")
    );
    assert_ne!(app.world_input(), stale, "the pick still retags the world input");
}

#[test]
fn a_filter_with_no_match_leaves_enter_inert_and_backspace_recovers() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "zzz".chars() {
        app.input_push(ch);
    }
    assert!(app.base_picker.as_ref().unwrap().filtered().is_empty());
    app.base_picker_pick().unwrap();
    assert_eq!(app.mode, Mode::BasePick, "enter does nothing with no matching branch");
    app.input_backspace();
    app.input_backspace();
    app.input_backspace();
    assert_eq!(app.base_picker.as_ref().unwrap().filtered().len(), 3);
}

#[test]
fn a_one_branch_repo_lists_that_branch() {
    // One local `main`, no remote: still a base, a default, and a row.
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("main")
    );
    app.open_base_picker();
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.rows.len(), 1);
    assert!(bp.rows[0].is_default() && bp.rows[0].current());
    assert_eq!(bp.rows[bp.cursor].name(), "main");
    app.close_base_picker();

    // Even with no default at all, the checked-out branch is a row, never an empty list.
    r.git(&["branch", "-m", "main", "trunk"]);
    app.open_base_picker();
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.rows.len(), 1);
    assert!(bp.rows[0].current() && !bp.rows[0].is_default());
}

#[test]
fn the_pr_target_row_sorts_first_and_says_so() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.pr = herdr_reviewr::forge::PrView::Pr(Box::new(herdr_reviewr::forge::PrSnapshot {
        base_ref: "dev".to_string(),
        ..common::pr_snapshot()
    }));
    app.open_base_picker();
    let bp = app.base_picker.as_ref().unwrap();
    let names: Vec<&str> = bp.rows.iter().map(herdr_reviewr::app::BaseChoice::name).collect();
    assert_eq!(names, ["dev", "main", "feature"], "pr base, then default, then recency");
    let shown: Vec<&str> = bp.visible().iter().map(|c| c.name()).collect();
    assert_eq!(shown, names, "an empty filter shows the rows as opened");
    assert!(bp.rows[0].pr_base() && !bp.rows[0].is_default());
    assert!(bp.rows[1].is_default() && !bp.rows[1].pr_base());
    assert_eq!(bp.rows[bp.cursor].name(), "main", "the highlight still opens on the base");
}

#[test]
fn a_detached_head_opens_the_picker_with_no_current_row() {
    let r = based_repo();
    r.git(&["checkout", "-q", "--detach"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    assert_eq!(app.mode, Mode::BasePick);
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.rows.len(), 3);
    assert!(bp.rows.iter().all(|r| !r.current()), "nothing is checked out");
}

#[test]
fn a_refresh_landing_mid_picker_changes_nothing() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    goto_row(&mut app, "dev");
    app.input_push('d');
    let before = app.base_picker.clone().unwrap();

    // The world moves under the open picker: a new branch, a new commit, a landing.
    r.git(&["branch", "dev-2"]);
    r.write("a.rs", "three\n");
    r.commit_all("more");
    common::land_world(&mut app);
    let after = app.base_picker.as_ref().unwrap();
    let names = |bp: &herdr_reviewr::app::BasePicker| -> Vec<String> {
        bp.rows.iter().map(|c| c.name().to_string()).collect()
    };
    assert_eq!(names(after), names(&before), "rows freeze at open");
    assert_eq!(after.visible()[after.cursor].name(), before.visible()[before.cursor].name());
    assert_eq!(after.query, before.query);
    assert_eq!(app.mode, Mode::BasePick);
}

#[test]
fn the_filter_is_fuzzy_and_ranks_the_closer_name_first() {
    let r = based_repo();
    r.git(&["branch", "feat/main"]);
    r.git(&["branch", "oauth-test"]);
    r.git(&["branch", "auth-fix"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "fmain".chars() {
        app.input_push(ch);
    }
    let bp = app.base_picker.as_ref().unwrap();
    let shown: Vec<&str> = bp.visible().iter().map(|c| c.name()).collect();
    assert_eq!(shown, ["feat/main"], "characters in order, across a slash");

    app.input_kill_to_start();
    for ch in "auth".chars() {
        app.input_push(ch);
    }
    let bp = app.base_picker.as_ref().unwrap();
    let shown: Vec<&str> = bp.visible().iter().map(|c| c.name()).collect();
    assert_eq!(shown, ["auth-fix", "oauth-test"], "a name-start match ranks first");
    assert_eq!(bp.visible()[bp.cursor].name(), "auth-fix", "the highlight rests on the best");
}

#[test]
fn a_typed_revision_is_one_more_row_below_the_matches() {
    let r = based_repo();
    r.git(&["branch", "v1.2-hotfix"]);
    r.git(&["tag", "v1.2", "HEAD~1"]);
    let tagged = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "v1.2".chars() {
        app.input_push(ch);
    }
    // A fuzzy match still probes; the tag's row goes below it.
    assert!(app.base_probe_wait().is_some(), "an inexact query schedules the probe");
    app.run_base_probe();
    let bp = app.base_picker.as_ref().unwrap();
    let shown: Vec<&str> = bp.visible().iter().map(|c| c.name()).collect();
    assert_eq!(shown, ["v1.2-hotfix", "v1.2"]);
    assert_eq!(bp.visible()[bp.cursor].name(), "v1.2-hotfix", "a hit never moves the highlight");
    assert_eq!(bp.visible()[1].oid(), Some(tagged.as_str()));
    app.base_picker_move(1);
    app.base_picker_pick().unwrap();
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("v1.2")
    );

    // A query spelling a listed name exactly runs no probe.
    app.open_base_picker();
    for ch in "dev".chars() {
        app.input_push(ch);
    }
    assert!(app.base_probe_wait().is_none(), "an exact spelling is a row, not a revision");
    app.run_base_probe();
    assert!(matches!(app.base_picker.as_ref().unwrap().probe, herdr_reviewr::app::BaseProbe::Idle));
}

#[test]
fn the_page_keys_move_the_highlight_and_home_end_move_the_caret() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let keymap = Keymap::default();
    app.open_base_picker();
    press(&mut app, &keymap, KeyCode::PageDown);
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.cursor, bp.visible().len() - 1, "a page down clamps to the last row");
    press(&mut app, &keymap, KeyCode::PageUp);
    assert_eq!(app.base_picker.as_ref().unwrap().cursor, 0);
    for ch in "de".chars() {
        app.input_push(ch);
    }
    press(&mut app, &keymap, KeyCode::Home);
    assert_eq!(app.base_picker.as_ref().unwrap().caret, 0, "home is the filter's");
    press(&mut app, &keymap, KeyCode::End);
    assert_eq!(app.base_picker.as_ref().unwrap().caret, 2);
    assert_eq!(app.mode, Mode::BasePick);
}

#[test]
fn the_filter_edits_with_the_comment_editors_controls() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "release dev".chars() {
        app.input_push(ch);
    }

    // The filter edits like every text field.
    app.input_delete_word();
    assert_eq!(app.base_picker.as_ref().unwrap().query, "release ");
    app.input_kill_to_start();
    assert_eq!(app.base_picker.as_ref().unwrap().query, "");
    for ch in "dv".chars() {
        app.input_push(ch);
    }
    app.caret_left();
    app.input_push('e');
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.query, "dev");
    assert_eq!(bp.rows[bp.filtered()[bp.cursor]].name(), "dev", "the narrowed view still picks");

    // A name pasted with its newline still filters.
    app.caret_end();
    app.input_kill_to_start();
    app.input_paste("dev\n");
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.query, "dev", "the trailing newline never lands in the query");
    assert_eq!(bp.rows[bp.filtered()[bp.cursor]].name(), "dev", "the pasted name filters");
}

#[test]
fn the_highlight_follows_its_row_through_a_narrowing_filter() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    goto_row(&mut app, "dev");
    app.input_push('d');
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(
        bp.rows[bp.filtered()[bp.cursor]].name(),
        "dev",
        "the highlight keeps its row when the row survives the filter"
    );

    // A filter that drops the highlighted row rests the highlight on the first match.
    app.input_push('x');
    app.input_backspace();
    app.input_backspace();
    app.input_push('m');
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.cursor, 0);
    assert_eq!(bp.visible()[0].name(), "main", "the first match: {:?}", bp.query);
}

#[test]
fn the_branch_scope_with_no_base_leads_the_footer_with_the_picker() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    r.git(&["branch", "-m", "main", "trunk"]); // no `main`/`master`: no default to fall back on
    r.git(&["branch", "dev"]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    assert!(app.branch_base.winner.is_none(), "no origin/HEAD, no main, no pick: nothing resolves");
    assert!(app.entries.is_empty(), "the empty state is legible, never a guessed base");
    let bands: Vec<(FooterAction, Band)> = app.footer_bands();
    assert_eq!(bands[0], (FooterAction::BasePick, Band::Primary));
    assert_eq!(bands[1], (FooterAction::ScopeOther, Band::Do));
    assert_eq!(bands[2], (FooterAction::Refresh, Band::Do));
}

#[test]
fn the_base_picker_owns_the_whole_footer_bar_and_survives_a_config_error() {
    let r = based_repo();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    let bands: Vec<FooterAction> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
    assert_eq!(
        bands,
        vec![FooterAction::PickBaseRow, FooterAction::ClosePicker, FooterAction::MoveBaseRow]
    );

    // The config error leaves the picker open for recovery to carry.
    app.input_push('d');
    app.set_config_error("theme = \"not-a-theme\"".to_string());
    assert_eq!(app.mode, Mode::BasePick);
    assert_eq!(app.base_picker.as_ref().unwrap().query, "d");
}

#[test]
fn typing_head_tilde_stores_the_spelling() {
    let r = based_repo();
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "HEAD~1".chars() {
        app.input_push(ch);
    }
    assert!(app.base_picker.as_ref().unwrap().visible().is_empty());
    assert!(app.base_probe_wait().is_some(), "typing schedules the empty-list probe");
    app.tick_base_picker_probe();
    assert!(app.base_picker.as_ref().unwrap().visible().is_empty(), "the pause has not elapsed");
    app.run_base_probe();
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.visible().len(), 1);
    assert_eq!(bp.visible()[0].name(), "HEAD~1");
    assert_eq!(bp.visible()[0].oid(), Some(parent.as_str()));
    app.base_picker_pick().unwrap();
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::oid),
        Some(parent.as_str())
    );
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("HEAD~1")
    );
    assert_eq!(herdr_reviewr::git::read_base_pick(r.path()).unwrap().as_deref(), Some("HEAD~1"));

    r.write("a.rs", "three\n");
    r.commit_all("later");
    let moved = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    assert_ne!(moved, parent);
    app.reload().unwrap();
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("HEAD~1")
    );
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::oid),
        Some(moved.as_str()),
        "a later commit still diffs one back"
    );
}

#[test]
fn enter_on_an_empty_list_probes_immediately() {
    let r = based_repo();
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "HEAD~1".chars() {
        app.input_push(ch);
    }
    app.base_picker_pick().unwrap();
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::oid),
        Some(parent.as_str())
    );
    assert_eq!(herdr_reviewr::git::read_base_pick(r.path()).unwrap().as_deref(), Some("HEAD~1"));
}

#[test]
fn a_typed_sha_prefix_stores_the_abbreviated_sha() {
    let r = based_repo();
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    let prefix = short[..4].to_string();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in prefix.chars() {
        app.input_push(ch);
    }
    app.base_picker_pick().unwrap();
    assert_eq!(
        herdr_reviewr::git::read_base_pick(r.path()).unwrap().as_deref(),
        Some(short.as_str())
    );
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some(short.as_str())
    );
    app.open_base_picker();
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.visible()[bp.cursor].name(), short);
}

#[test]
fn a_short_sha_with_a_newline_can_be_pasted_and_picked() {
    let r = based_repo();
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    app.input_paste(&format!("{short}\n"));
    assert_eq!(app.base_picker.as_ref().unwrap().query, short);
    app.base_picker_pick().unwrap();
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::oid),
        Some(parent.as_str())
    );
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some(short.as_str())
    );
    assert_eq!(
        herdr_reviewr::git::read_base_pick(r.path()).unwrap().as_deref(),
        Some(short.as_str())
    );
}

#[test]
fn a_current_named_rev_is_the_highlighted_row_on_reopen() {
    let r = based_repo();
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    herdr_reviewr::git::write_base_pick(r.path(), "HEAD~1").unwrap();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.visible()[bp.cursor].name(), "HEAD~1");
    assert_eq!(bp.visible()[bp.cursor].oid(), Some(parent.as_str()));
}

#[test]
fn picking_the_default_tips_sha_does_not_clear_the_pick() {
    let r = based_repo();
    let main = r.git(&["rev-parse", "main"]).trim().to_string();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    app.input_paste(&main);
    app.base_picker_pick().unwrap();
    assert_eq!(
        herdr_reviewr::git::read_base_pick(r.path()).unwrap().as_deref(),
        Some(main.as_str())
    );
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::oid),
        Some(main.as_str())
    );
}

#[test]
fn checking_out_a_picked_branch_does_not_turn_it_into_a_pin() {
    let r = based_repo();
    herdr_reviewr::git::write_base_pick(r.path(), "dev").unwrap();
    r.git(&["checkout", "-q", "dev"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(
        app.branch_base.winner.as_ref().map(herdr_reviewr::git::ResolvedBase::name),
        Some("dev")
    );
    app.open_base_picker();
    let bp = app.base_picker.as_ref().unwrap();
    assert!(
        bp.rows.iter().all(|row| row.oid().is_none()),
        "a live branch pick must not grow a SHA row"
    );
    assert!(bp.visible()[bp.cursor].oid().is_none(), "the highlight must not be a pin");
    assert_eq!(herdr_reviewr::git::read_base_pick(r.path()).unwrap().as_deref(), Some("dev"));
}

// ---- mouse text selection ----

/// A file with a plain, a tab-indented, and a wide-character line.
fn selection_repo() -> Repo {
    let r = Repo::init();
    r.write("base.rs", "fn main() {}\n");
    r.commit_all("init");
    r.write("m.rs", "alpha beta\n\tif x {\n日本 z\n");
    r
}

const SEL_AREA: Rect = Rect::new(0, 0, 120, 40);

thread_local! {
    /// [`SelClipboard`]'s writes, newest last, per test thread.
    static SEL_COPIES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// A capturing clipboard, so tests never touch the real one.
struct SelClipboard;

impl ExportTarget for SelClipboard {
    fn label(&self) -> &'static str {
        "clipboard"
    }
    fn success_message(&self, count: usize) -> String {
        format!("copied {count}")
    }
    fn failure_message(&self, _error: &anyhow::Error, _copy: &str) -> String {
        "clipboard failed".to_string()
    }
    fn export(&self, text: &str) -> Result<()> {
        SEL_COPIES.with(|c| c.borrow_mut().push(text.to_string()));
        Ok(())
    }
}

/// The last text [`SelClipboard`] captured on this test's thread.
fn last_copy() -> Option<String> {
    SEL_COPIES.with(|c| c.borrow().last().cloned())
}

/// Paint a frame, then dispatch one mouse event, as the event loop does.
fn sel_mouse(app: &mut App, kind: MouseEventKind, col: u16, row: u16) {
    let backend = ratatui::backend::TestBackend::new(SEL_AREA.width, SEL_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| herdr_reviewr::ui::render(f, app)).unwrap();
    let heights = herdr_reviewr::ui::diff_row_heights(app, SEL_AREA);
    let event = MouseEvent { kind, column: col, row, modifiers: KeyModifiers::NONE };
    handle_mouse(app, event, SEL_AREA, &heights, &Keymap::default(), &SelClipboard).unwrap();
}

/// The screen cell of `(row, display column)`, past a 5-cell gutter.
fn sel_cell(app: &App, row: usize, display_col: u16) -> (u16, u16) {
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, app);
    (inner.x + 5 + display_col, inner.y + u16::try_from(row).unwrap())
}

#[test]
fn a_text_drag_maps_tabs_and_wide_chars_and_extracts_source_text() {
    let r = selection_repo();
    let mut app = app_on(&r);
    // From `beta` on row 0 to `z` on row 2, past two wide glyphs.
    let (c0, r0) = sel_cell(&app, 0, 6);
    let (c2, r2) = sel_cell(&app, 2, 5);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c2, r2);
    assert_eq!(herdr_reviewr::drag_text(&app, SEL_AREA).as_deref(), Some("beta\n\tif x {\n日本 z"));
    // A drag onto the tab's expansion cells selects the tab character itself.
    let (c1, r1) = sel_cell(&app, 1, 2);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);
    assert_eq!(herdr_reviewr::drag_text(&app, SEL_AREA).as_deref(), Some("beta\n\t"));
    // A keypress cancels the drag: nothing copies, and the key still acts.
    press(&mut app, &Keymap::default(), KeyCode::Esc);
    assert!(app.text_drag().is_none());
    assert_eq!(app.status, "");
}

#[test]
fn a_release_on_the_mouse_down_cell_is_a_click_and_a_real_drag_copies() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c1, r1) = sel_cell(&app, 1, 3);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c1, r1);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c1, r1);
    assert_eq!(app.diff_cursor, 1, "a same-cell release is the click");
    assert!(app.text_drag().is_none());
    assert_eq!(app.status, "");

    // Drift within one tab's cells is still a click.
    app.diff_cursor = 0;
    let (c1a, _) = sel_cell(&app, 1, 1);
    let (c1b, _) = sel_cell(&app, 1, 2);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c1a, r1);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c1b, r1);
    assert_eq!(app.diff_cursor, 1, "a slip within the tab's cells still clicks");
    assert_eq!(app.status, "", "and copies nothing");

    // TS-NO-REVIEW-STATE: a full drag-release cycle touches no review state.
    let before = app.diff_cursor;
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c2, r2) = sel_cell(&app, 2, 2);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c2, r2);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c2, r2);
    assert_eq!(app.store.len(), 0);
    assert!(app.select_anchor.is_none());
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.diff_cursor, before);
    assert!(app.text_drag().is_none());
    assert_eq!(app.status, "copied 21 chars", "release reports the copy");
    assert_eq!(last_copy().as_deref(), Some("alpha beta\n\tif x {\n日本"));
}

#[test]
fn ts_one_surface_a_drag_clamps_to_its_pane_and_skips_cards() {
    use herdr_reviewr::selection::Surface;
    let r = selection_repo();
    let mut app = app_on(&r);
    // Extend into the navigator pane: the extent clamps into the read pane.
    let (c0, r0) = sel_cell(&app, 0, 0);
    let files = herdr_reviewr::ui::files_inner_rect(SEL_AREA, &app);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), files.x + 2, files.y + 1);
    let drag = app.text_drag().expect("drag still live");
    assert_eq!(drag.surface, Surface::Read);
    assert!(drag.extent.row < app.visible.len());
    press(&mut app, &Keymap::default(), KeyCode::Esc);

    // A code drag across a card's rows copies no card text.
    app.focus = Focus::Diff;
    app.diff_cursor = 1;
    press(&mut app, &Keymap::default(), KeyCode::Char('c'));
    typed(&mut app, "watch this");
    press(&mut app, &Keymap::default(), KeyCode::Enter);
    assert_eq!(app.store.len(), 1);
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    let (c0, r0) = sel_cell(&app, 0, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), inner.x + 5 + 5, inner.y + 5);
    let drag = app.text_drag().expect("drag still live");
    assert_eq!(drag.extent.row, 2, "the extent lands on row 2's code below the card");
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), inner.x + 5 + 5, inner.y + 5);
    assert_eq!(app.status, "copied 23 chars");
    let text = last_copy().unwrap();
    assert_eq!(text, "alpha beta\n\tif x {\n日本 z");
    assert!(!text.contains("watch this"), "spanned cards contribute nothing");

    // A drag that starts on the card selects that card's text, confined to it.
    app.status.clear();
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), inner.x + 4, inner.y + 3);
    assert!(matches!(
        app.text_drag().expect("card drag armed").surface,
        Surface::Card { comment: 0 }
    ));
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), inner.x + 4 + 9, inner.y + 3);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), inner.x + 4 + 9, inner.y + 3);
    assert_eq!(app.status, "copied 10 chars");
    assert_eq!(last_copy().as_deref(), Some("watch this"), "the card's own text copies");

    // A double on the card copies the word under the cell; no composer opens over the card
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), inner.x + 4, inner.y + 3);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), inner.x + 4, inner.y + 3);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), inner.x + 4, inner.y + 3);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), inner.x + 4, inner.y + 3);
    assert!(!app.composing(), "a card double-click never opens the composer");
    assert_eq!(last_copy().as_deref(), Some("watch"), "the card word copies");
}

#[test]
fn the_gutter_click_and_drag_open_the_composer_and_stay_inert_while_composing() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    let gutter_x = inner.x + 1;

    // A gutter click opens the composer on that line, acting as `c` there.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), gutter_x, inner.y);
    assert!(app.gutter_drag());
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), gutter_x, inner.y);
    assert!(app.composing());
    assert_eq!(app.selection_range(), (0, 0));

    // While the comment editor is open, the gutter is inert.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), gutter_x, inner.y + 2);
    assert!(!app.gutter_drag());
    assert!(app.text_drag().is_none());

    // While composing, a double-click copies its word; the composer holds.
    let (c0, r0) = sel_cell(&app, 0, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    assert!(app.composing(), "the open composer survives a double-click");
    assert_eq!(app.selection_range(), (0, 0), "the draft's anchor never moves");
    assert_eq!(last_copy().as_deref(), Some("alpha"), "the word still copies while composing");
    press(&mut app, &Keymap::default(), KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);

    // A gutter drag selects the spanned range and opens the composer on release.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), gutter_x, inner.y);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), gutter_x, inner.y + 2);
    assert!(app.gutter_drag());
    assert_eq!(app.selection_range(), (0, 2));
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), gutter_x, inner.y + 2);
    assert!(app.composing());
    assert_eq!(app.selection_range(), (0, 2));
    typed(&mut app, "range note");
    press(&mut app, &Keymap::default(), KeyCode::Enter);
    assert_eq!(app.store.len(), 1);
}

#[test]
fn a_double_click_copies_the_word_and_settles_its_highlight() {
    let r = selection_repo();
    let mut app = app_on(&r);
    // A double on a word copies exactly that word and leaves it highlighted
    let (c0, r0) = sel_cell(&app, 0, 0); // the `a` of `alpha`
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    assert_eq!(app.status, "", "the first click only moves the cursor");
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    assert!(!app.composing(), "the double copies, never comments");
    assert_eq!(app.status, "copied 5 chars");
    assert_eq!(last_copy().as_deref(), Some("alpha"));
    let settled = app.settled_selection().expect("the copy leaves its highlight");
    assert_eq!((settled.anchor.row, settled.anchor.chr, settled.extent.chr), (0, 0, 4));

    // The settled span paints in the selection fill, distinct from the cursor row's
    let backend = ratatui::backend::TestBackend::new(SEL_AREA.width, SEL_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| herdr_reviewr::ui::render(f, &app)).unwrap();
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    let cell = terminal.backend().buffer().cell((inner.x + 5, inner.y)).unwrap();
    assert_eq!(cell.style().bg, Some(app.palette().fill(herdr_reviewr::roles::Fill::Selection)));

    // Any keypress clears the settled highlight.
    press(&mut app, &Keymap::default(), KeyCode::Char('j'));
    assert!(app.settled_selection().is_none(), "a keypress is the user doing something else");

    // On whitespace the double acts as the click: a tab expansion holds no word.
    let (c1, r1) = sel_cell(&app, 1, 1);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c1, r1);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c1, r1);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c1, r1);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c1, r1);
    assert!(!app.composing());
    assert_eq!(last_copy().as_deref(), Some("alpha"), "nothing new copies on whitespace");
    assert_eq!(app.diff_cursor, 1, "the whitespace double acted as the click");

    // A keypress mid-drag cancels: nothing copies, and the key still moves the cursor.
    app.status.clear();
    let (a0, ar) = sel_cell(&app, 0, 0);
    let (b2, br) = sel_cell(&app, 2, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), a0, ar);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), b2, br);
    press(&mut app, &Keymap::default(), KeyCode::Char('j'));
    assert!(app.text_drag().is_none());
    assert_eq!(app.status, "");
    assert_eq!(app.diff_cursor, 2, "the key still moved the cursor down from row 1");
}

#[test]
fn a_triple_click_copies_the_whole_line_and_settles_its_highlight() {
    let r = selection_repo();
    let mut app = app_on(&r);
    // A triple on a line copies the whole source line and leaves it highlighted
    let (c0, r0) = sel_cell(&app, 0, 0);
    for _ in 0..3 {
        sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
        sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    }
    assert!(!app.composing(), "the triple copies, never comments");
    assert_eq!(app.status, "copied 10 chars");
    assert_eq!(last_copy().as_deref(), Some("alpha beta"));
    let settled = app.settled_selection().expect("the copy leaves its highlight");
    assert_eq!((settled.anchor.row, settled.anchor.chr, settled.extent.chr), (0, 0, 9));

    // A fourth click within the window repeats the triple.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    assert_eq!(app.status, "copied 10 chars");
    assert!(app.settled_selection().is_some(), "the repeat settles the line again");
}

#[test]
fn a_settled_highlight_survives_an_unrelated_refresh_and_clears_when_its_text_changes() {
    let r = selection_repo();
    let mut app = app_on(&r);
    // Settle by drag, which resets the click chain.
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c9, _) = sel_cell(&app, 0, 9);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c9, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c9, r0);
    assert!(app.settled_selection().is_some());

    // An unrelated new file refreshes the world and leaves the spanned text alone.
    r.write("other.rs", "unrelated\n");
    app.reload().unwrap();
    assert!(app.settled_selection().is_some(), "an untouched span survives the refresh");

    // The spanned line's text changes under the poll, same row count: stale never wrong.
    r.write("m.rs", "ALPHA beta\n\tif x {\n日本 z\n");
    app.reload().unwrap();
    assert!(app.settled_selection().is_none(), "changed spanned text blanks the highlight");
}

#[test]
fn a_settled_rendered_highlight_follows_its_text() {
    let r = Repo::init();
    r.write("doc.md", "# Title\n\nplain body words\n");
    r.commit_all("init");
    r.write("doc.md", "# Title\n\nplain body words changed\n");
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active());
    // Double-click the paragraph's first word on rendered row 2.
    let (col, row) = sel_cell(&app, 2, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), col, row);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), col, row);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), col, row);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), col, row);
    assert_eq!(last_copy().as_deref(), Some("plain"));
    assert!(app.settled_selection().is_some());

    // The highlight survives only an unchanged text.
    r.write("doc.md", "# Retitled\n\nplain body words changed\n");
    app.reload().unwrap();
    assert!(app.settled_selection().is_some(), "an untouched rendered line keeps the highlight");
    r.write("doc.md", "# Retitled\n\nrewritten body words here\n");
    app.reload().unwrap();
    assert!(app.settled_selection().is_none(), "a rewritten rendered line blanks the highlight");
}

#[test]
fn a_mouse_down_on_blank_space_arms_no_gesture() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    // The pane's blank space below the last content row starts nothing
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), inner.x + 4, inner.y + 20);
    assert!(app.text_drag().is_none(), "no drag starts on blank space");
    assert!(!app.gesture_active(), "no gesture arms on blank space");
}

#[test]
fn a_drag_release_keeps_the_selection_until_the_next_mouse_down() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c1, r1) = sel_cell(&app, 0, 9);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c1, r1);
    assert_eq!(app.status, "copied 10 chars");
    assert!(app.settled_selection().is_some(), "the release keeps the highlight as feedback");

    // The wheel is reading, not doing something else: the highlight survives it.
    sel_mouse(&mut app, MouseEventKind::ScrollDown, c0, r0);
    assert!(app.settled_selection().is_some(), "scrolling keeps the settled highlight");

    // The next mouse-down clears it before acting.
    let (c2, r2) = sel_cell(&app, 2, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c2, r2);
    assert!(app.settled_selection().is_none(), "a mouse-down is the user doing something else");
}

#[test]
fn the_navigator_click_survives_a_one_cell_slip_and_a_row_drag_copies_paths() {
    let r = selection_repo();
    r.write("sub/two.rs", "two\n");
    let mut app = app_on(&r);
    let files = herdr_reviewr::ui::files_inner_rect(SEL_AREA, &app);
    let rows = app.file_rows.len();
    assert!(rows >= 2, "the tree lists m.rs and sub/two.rs");

    // A release one cell over on the same row still activates the row.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), files.x + 1, files.y);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), files.x + 2, files.y);
    assert_eq!(app.file_cursor, 0, "the slipped click still selects the row");
    assert!(app.text_drag().is_none());

    // A row drag copies full paths; a new start cell avoids the click chain.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), files.x + 4, files.y);
    let last = files.y + u16::try_from(rows - 1).unwrap();
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), files.x + 4, last);
    let text = herdr_reviewr::drag_text(&app, SEL_AREA).unwrap();
    assert!(text.contains("m.rs"), "paths, not display names: {text}");
    assert!(text.contains("sub/two.rs"), "the full path, directories included: {text}");
    press(&mut app, &Keymap::default(), KeyCode::Esc);

    // A navigator double-click copies one row's full path alone.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), files.x + 1, files.y);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), files.x + 1, files.y);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), files.x + 1, files.y);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), files.x + 1, files.y);
    let copied = last_copy().expect("the double copied a path");
    assert!(!copied.contains('\n'), "one row: {copied}");
    assert!(!copied.starts_with('/'), "repo-relative, never absolute: {copied}");
    assert!(app.settled_selection().is_some(), "the navigator copy settles its row highlight");
}

#[test]
fn a_world_result_waits_out_an_active_drag() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let rows_before = app.file_rows.len();
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c1, r1) = sel_cell(&app, 1, 2);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);
    assert!(app.gesture_active());

    // A code drag freezes only the open view.
    r.write("m.rs", "CHANGED first line\n\tif x {\n日本 z\n");
    r.write("n.rs", "new file\n");
    app.reload().unwrap();
    assert_eq!(app.visible[0].text(), "alpha beta", "the drag freezes the open view");
    assert!(app.file_rows.len() > rows_before, "the file list updates beneath the drag");

    // The cancelling keypress reloads the held open view.
    press(&mut app, &Keymap::default(), KeyCode::Esc);
    assert_eq!(app.visible[0].text(), "CHANGED first line");
}

#[test]
fn a_navigator_drag_gates_the_world_drain_and_its_end_lifts_the_gate() {
    let r = selection_repo();
    r.write("sub/two.rs", "two\n");
    let mut app = app_on(&r);
    let files = herdr_reviewr::ui::files_inner_rect(SEL_AREA, &app);

    // A navigator drag holds the world drain.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), files.x + 1, files.y);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), files.x + 1, files.y + 1);
    assert!(app.gesture_active());
    assert!(app.gates_world_drain(), "a navigator drag holds the world drain");
    assert!(!app.gates_pr_drain(), "it holds nothing of the PR pipeline");

    // The reflow cancel lifts the gate; the next loop pass drains the queued completion.
    press(&mut app, &Keymap::default(), KeyCode::Esc);
    assert!(!app.gesture_active());
    assert!(!app.gates_world_drain());

    // A code drag gates no drain.
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c1, r1) = sel_cell(&app, 1, 2);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);
    assert!(!app.gates_world_drain());
    assert!(!app.gates_pr_drain());
    press(&mut app, &Keymap::default(), KeyCode::Esc);
}

#[test]
fn a_release_lost_past_the_border_still_copies() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c1, r1) = sel_cell(&app, 1, 2);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);

    // The drag ends on the pane's edge; its release goes elsewhere.
    let edge = SEL_AREA.x + SEL_AREA.width - 1;
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), edge, r1);
    let event = MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: edge,
        row: r1,
        modifiers: KeyModifiers::NONE,
    };
    assert!(
        herdr_reviewr::pointer_at_pane_edge(event, SEL_AREA),
        "the pane's edge column is the exit signature"
    );

    // The exit deadline copies the selection (TS-NO-SILENT-LOSS).
    herdr_reviewr::complete_gesture(&mut app, SEL_AREA, &SelClipboard);
    assert!(!app.gesture_active());
    assert_eq!(app.status, "copied 18 chars", "the lost release still copies");
    assert_eq!(last_copy().as_deref(), Some("alpha beta\n\tif x {"));
}

#[test]
fn a_still_pointer_inside_the_pane_is_a_held_button_not_an_exit() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let content = herdr_reviewr::ui::read_content_rect(SEL_AREA, &app);
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c1, r1) = sel_cell(&app, 1, 2);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);

    // Stillness inside the pane, border included, means a held button.
    for (col, row) in [(c1, r1), (c0, content.y + content.height)] {
        let event = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(!herdr_reviewr::pointer_at_pane_edge(event, SEL_AREA));
    }
    press(&mut app, &Keymap::default(), KeyCode::Esc);
}

#[test]
fn a_press_that_never_moved_dissolves_on_the_deadline_with_nothing() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    let cursor = app.diff_cursor;

    herdr_reviewr::complete_gesture(&mut app, SEL_AREA, &SelClipboard);
    assert!(!app.gesture_active());
    assert_eq!(app.status, "", "no selection was visible, so nothing copies");
    assert_eq!(app.diff_cursor, cursor, "and no click fires");
}

#[test]
fn a_lost_gutter_drag_dissolves_without_the_composer() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    let gutter_x = inner.x + 1;
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), gutter_x, inner.y);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), gutter_x, inner.y + 2);
    assert_eq!(app.selection_range(), (0, 2));

    // A lost gutter drag dissolves without opening the composer.
    herdr_reviewr::complete_gesture(&mut app, SEL_AREA, &SelClipboard);
    assert!(!app.gesture_active());
    assert!(!app.composing(), "the composer opens only on the gutter's own release");
    assert!(app.select_anchor.is_none(), "the dissolved range clears");
    assert_eq!(app.status, "");
}

#[test]
fn a_press_inside_the_double_click_window_still_drags() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 2); // inside `alpha`
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    app.status.clear();
    // The second press arms a drag; the copy waits for release.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    assert!(app.text_drag().is_some(), "the second press arms the drag");
    assert_eq!(app.status, "", "nothing copies at the down");
    let (c2, r2) = sel_cell(&app, 2, 2);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c2, r2);
    assert_eq!(
        herdr_reviewr::drag_text(&app, SEL_AREA).as_deref(),
        Some("pha beta\n\tif x {\n日本"),
        "the dragged-away press is a plain drag selection"
    );
    press(&mut app, &Keymap::default(), KeyCode::Esc);
}

#[test]
fn pointer_motion_with_no_button_completes_a_drag_whose_release_was_lost() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c1, r1) = sel_cell(&app, 1, 2);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);
    r.write("m.rs", "CHANGED first line\n\tif x {\n日本 z\n");
    app.reload().unwrap();
    assert_eq!(app.visible[0].text(), "alpha beta", "the drag freezes the open view");

    // `Moved` proves a lost release: copy, and the held view catches up.
    sel_mouse(&mut app, MouseEventKind::Moved, c1, r1);
    assert!(!app.gesture_active());
    assert_eq!(app.status, "copied 12 chars", "the proof completes the copy");
    assert_eq!(last_copy().as_deref(), Some("alpha beta\n\t"));
    assert_eq!(app.visible[0].text(), "CHANGED first line");
}

#[test]
fn the_next_mouse_down_completes_the_old_gesture_then_arms_its_own() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 0);
    let (c1, r1) = sel_cell(&app, 1, 2);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);

    // A new down copies the old selection, then arms its own gesture.
    let (c2, r2) = sel_cell(&app, 2, 1);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c2, r2);
    assert_eq!(app.status, "copied 12 chars", "the down completes the old gesture first");
    let drag = app.text_drag().expect("the same down arms its own gesture");
    assert_eq!(drag.anchor.row, 2);
    press(&mut app, &Keymap::default(), KeyCode::Esc);
}

#[test]
fn a_pr_navigator_row_copies_its_full_text_even_when_the_pane_truncates_it() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Comment, PrSnapshot, PrView};
    use herdr_reviewr::selection::{Gesture, Point, Surface, TextDrag};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    let anchor = "a/very/deeply/nested/path/that/no/navigator/pane/could/ever/show/whole.rs:412";
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot {
        comments: vec![Comment { anchor: anchor.into(), ..common::comment() }],
        ..common::pr_snapshot()
    })));

    // The pane elides the anchor to its width; the copy receives it whole
    app.gesture = Gesture::Text {
        drag: TextDrag {
            surface: Surface::PrNav,
            anchor: Point { row: 0, chr: 0 },
            extent: Point { row: 99, chr: 0 },
        },
        count: 1,
    };
    let text = herdr_reviewr::drag_text(&app, SEL_AREA).unwrap();
    assert!(text.contains(anchor), "the full anchor copies: {text}");
    app.gesture = Gesture::None;
}

#[test]
fn a_completed_drag_resets_the_multi_click_chain() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 2); // inside `alpha`
    let (c1, r1) = sel_cell(&app, 1, 3);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c1, r1);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    assert_eq!(app.status, "copied 10 chars");
    app.status.clear();

    // After a drag, the next press is a first click.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0);
    assert!(!app.composing(), "a first click opens no comment box");
    assert_eq!(app.diff_cursor, 0, "the click acts");
}

#[test]
fn a_gutter_gesture_lands_right_with_the_find_band_open() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    let gutter_x = inner.x + 1;
    app.open_find();
    assert_eq!(app.mode, Mode::Find);

    // The band takes the pane's bottom row; the rows above it keep their gutter mapping.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), gutter_x, inner.y);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), gutter_x, inner.y + 1);
    assert!(app.gutter_drag());
    assert_eq!(app.selection_range(), (0, 1));
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), gutter_x, inner.y + 1);
    assert!(app.composing(), "the gutter release opens the composer under the band's file");
    // The composer replaced the find band.
    assert!(app.find.is_none(), "the gutter release closes the find band");
    press(&mut app, &Keymap::default(), KeyCode::Esc);
}

#[test]
fn a_rendered_drag_selects_and_copies_through_the_read_surface() {
    use herdr_reviewr::selection::Surface;
    let r = Repo::init();
    r.write("doc.md", "# Title\n\nplain body words\n");
    r.commit_all("init");
    r.write("doc.md", "# Title\n\nplain **body** words changed\n");
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active());

    // A drag over the rendered heading copies its text.
    let (c0, r0) = sel_cell(&app, 0, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    assert_eq!(app.text_drag().map(|d| d.surface), Some(Surface::Read));
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c0 + 40, r0);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0 + 40, r0);
    assert_eq!(last_copy().as_deref(), Some("Title"), "the rendered heading copies");

    // A drag across rows copies the rendered text, emphasis markers consumed.
    let (c2, r2) = sel_cell(&app, 2, 30);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c2, r2);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c2, r2);
    assert_eq!(last_copy().as_deref(), Some("Title\n\nplain body words changed"));

    // A double copies the word under the cell; the rendered view never takes a comment.
    let (cw, rw) = sel_cell(&app, 2, 7);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), cw, rw);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), cw, rw);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), cw, rw);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), cw, rw);
    assert!(!app.composing(), "the rendered view never takes a comment");
    assert_eq!(last_copy().as_deref(), Some("body"), "the word under the cell copies");
}

#[test]
fn a_pr_navigator_drag_gates_the_pr_drains() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Comment, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot {
        comments: vec![Comment { ..common::comment() }],
        ..common::pr_snapshot()
    })));

    // A `PR` navigator drag holds both PR drains until it ends.
    let files = herdr_reviewr::ui::files_inner_rect(SEL_AREA, &app);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), files.x + 1, files.y);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), files.x + 1, files.y + 1);
    assert!(app.gesture_active());
    assert!(app.gates_pr_drain(), "a PR-navigator drag holds the PR drains");
    assert!(!app.gates_world_drain(), "and nothing of the world drain");
    press(&mut app, &Keymap::default(), KeyCode::Esc);
    assert!(!app.gates_pr_drain());
}

#[test]
fn the_wheel_during_a_drag_scrolls_and_extends_together() {
    use std::fmt::Write as _;
    let r = Repo::init();
    r.write("base.rs", "fn main() {}\n");
    r.commit_all("init");
    let long = (0..60).fold(String::new(), |mut s, i| {
        let _ = writeln!(s, "line number {i}");
        s
    });
    r.write("tall.rs", &long);
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c0, r0 + 2);
    let before = app.text_drag().unwrap().extent.row;

    // The wheel scrolls, then extends against the scrolled rows.
    sel_mouse(&mut app, MouseEventKind::ScrollDown, c0, r0 + 2);
    assert_eq!(app.diff_scroll, 3, "the wheel scrolled the drag's pane");
    let extent = app.text_drag().unwrap().extent.row;
    assert_eq!(extent, before + 3, "the extent follows the pointer onto the scrolled rows");
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c0, r0 + 2);
    assert_eq!(app.status, "copied 71 chars");
}

#[test]
fn the_hover_cell_survives_a_keypress() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let (c0, r0) = sel_cell(&app, 0, 0);
    sel_mouse(&mut app, MouseEventKind::Moved, c0, r0);
    assert_eq!(app.hover, Some((c0, r0)));

    // A keypress doesn't blank the hover.
    press(&mut app, &Keymap::default(), KeyCode::Char('j'));
    assert_eq!(app.hover, Some((c0, r0)));
}

#[test]
fn the_drag_h_scroll_caps_at_the_widest_visible_row() {
    let r = selection_repo();
    let mut app = app_on(&r);
    app.wrap = false;
    let content = herdr_reviewr::ui::read_content_rect(SEL_AREA, &app);
    let widest = herdr_reviewr::ui::widest_visible_row(&app, SEL_AREA);
    assert!(widest > 0);
    let (c0, r0) = sel_cell(&app, 0, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);

    // A held edge drag scrolls up to the widest row, no further.
    for _ in 0..widest {
        sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), content.x + content.width, r0);
    }
    assert_eq!(app.h_scroll, widest - 1, "the cap keeps the widest row's last column");

    // A keyboard scroll past the cap is never pulled back.
    app.h_scroll = widest + 20;
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), content.x + content.width, r0);
    assert_eq!(app.h_scroll, widest + 20);
    press(&mut app, &Keymap::default(), KeyCode::Esc);
}

#[test]
fn the_outermost_row_selects_without_scrolling_and_the_border_scrolls() {
    let r = selection_repo();
    let mut app = app_on(&r);
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    let (c0, r0) = sel_cell(&app, 0, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    // The last inner row selects without scrolling.
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c0, inner.y + inner.height - 1);
    assert_eq!(app.diff_scroll, 0, "the last inner row scrolls nothing");
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c0, inner.y + inner.height);
    assert_eq!(app.diff_scroll, 1, "the border row scrolls");
    press(&mut app, &Keymap::default(), KeyCode::Esc);
}

// --- commit picker --------

/// Four commits each adding a file, an edit to `root.rs`; the shas root first.
fn commits_repo() -> (Repo, Vec<String>) {
    let r = Repo::init();
    r.write("root.rs", "r\n");
    r.commit_all("root");
    r.write("one.rs", "1\n");
    r.commit_all("one");
    r.write("two.rs", "2\n");
    r.commit_all("two");
    r.write("three.rs", "3\n");
    r.commit_all("three");
    r.write("root.rs", "dirty\n");
    let shas: Vec<String> =
        r.git(&["rev-list", "--reverse", "HEAD"]).lines().map(str::to_string).collect();
    (r, shas)
}

fn picker(app: &App) -> &herdr_reviewr::app::CommitPicker {
    app.commit_picker.as_ref().expect("the commit picker is open")
}

fn changed_paths(app: &App) -> Vec<String> {
    app.entries.iter().map(|e| e.path.clone()).collect()
}

#[test]
fn the_commit_picker_opens_on_every_file_tab_and_nowhere_else() {
    let (r, _) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    for scope in [Scope::Uncommitted, Scope::Branch, Scope::LastTurn] {
        app.set_scope(scope).unwrap();
        press(&mut app, &keymap, KeyCode::Char('G'));
        assert_eq!(app.mode, Mode::CommitPick, "opens under {scope:?}");
        assert_eq!(picker(&app).title, "commits · last 50");
        assert_eq!(picker(&app).rows.len(), 4);
        assert_eq!(picker(&app).cursor, 0, "no pick: the highlight opens on the first row");
        assert!(picker(&app).anchor.is_none());
        press(&mut app, &keymap, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.scope, scope, "esc leaves the previous scope active");
    }
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert_eq!(app.mode, Mode::CommitPick, "opens on All files");
    press(&mut app, &keymap, KeyCode::Esc);
    enter_tab(&mut app, herdr_reviewr::app::Tab::Pr);
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert_eq!(app.mode, Mode::Normal, "inert on the PR tab");
    press(&mut app, &keymap, KeyCode::Char('g'));
    assert_eq!(app.mode, Mode::Normal, "scope-commits is inert on the PR tab");
    enter_tab(&mut app, herdr_reviewr::app::Tab::Changes);
    app.set_scope(Scope::Uncommitted).unwrap();
    for (name, open) in [
        ("the comments list", Box::new(|a: &mut App| a.mode = Mode::List) as Box<dyn Fn(&mut App)>),
        ("the base picker", Box::new(|a: &mut App| a.mode = Mode::BasePick)),
        ("search", Box::new(|a: &mut App| a.open_search())),
        ("find", Box::new(|a: &mut App| a.open_find())),
    ] {
        open(&mut app);
        let before = app.mode.clone();
        app.open_commit_picker();
        assert_eq!(app.mode, before, "inert under {name}");
        app.mode = Mode::Normal;
        app.close_search();
        app.close_find();
    }
    assert!(
        app.footer_bands().iter().any(|&(a, b)| a == FooterAction::CommitPick && b == Band::Go),
        "the go band carries the picker key on a file tab"
    );
}

#[test]
fn enter_picks_the_highlight_and_switches_to_the_commits_scope() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('g'));
    assert_eq!(app.mode, Mode::CommitPick, "scope-commits with no pick opens the picker");
    assert_eq!(app.scope, Scope::Uncommitted, "without switching");
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.scope, Scope::Commits);
    assert_eq!(app.commit_pick, Some(herdr_reviewr::model::CommitPick::single(&shas[2])));
    assert_eq!(changed_paths(&app), ["two.rs"], "the commit alone, no worktree edit");
    let status = app.pick_status.as_ref().expect("the verdict lands with the changeset");
    assert_eq!(status.verdict, herdr_reviewr::world::PickVerdict::Live);
    assert_eq!(status.subject, "two");
    assert_eq!(app.changed_count(), 1);

    // Both sides come from the commits, not the worktree.
    app.select_file(0).unwrap();
    assert!(app.diff.rows.iter().any(|row| row.marker() == '+'));
    assert!(!app.diff.rows.iter().any(|row| row.marker() == '-'));

    // `g` with a pick switches straight back to it from another scope.
    app.set_scope(Scope::Uncommitted).unwrap();
    assert_eq!(changed_paths(&app), ["root.rs"]);
    press(&mut app, &keymap, KeyCode::Char('g'));
    assert_eq!(app.mode, Mode::Normal, "a held pick switches without the picker");
    assert_eq!(app.scope, Scope::Commits);
    assert_eq!(changed_paths(&app), ["two.rs"]);

    // The chip's `commits` step does the same.
    app.set_scope(Scope::LastTurn).unwrap();
    app.set_scope(app.scope.cycle()).unwrap();
    assert_eq!(app.scope, Scope::Commits);
}

#[test]
fn v_anchors_a_run_in_either_direction_and_esc_clears_it_first() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    // Anchor on the newest, move down two: the run is three, newest to `one`.
    press(&mut app, &keymap, KeyCode::Char('v'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    assert_eq!(picker(&app).run_len(), 3);
    assert!(picker(&app).in_run(0) && picker(&app).in_run(2) && !picker(&app).in_run(3));
    let bands = app.footer_bands();
    assert_eq!(bands[0].0, FooterAction::PickCommitRun);
    // First `esc` clears the anchor, the picker stays.
    press(&mut app, &keymap, KeyCode::Esc);
    assert_eq!(app.mode, Mode::CommitPick);
    assert!(picker(&app).anchor.is_none());
    assert_eq!(picker(&app).run_len(), 1);
    // Anchor below, move up: the same run the other way round.
    press(&mut app, &keymap, KeyCode::Char('v'));
    press(&mut app, &keymap, KeyCode::Char('k'));
    press(&mut app, &keymap, KeyCode::Char('k'));
    press(&mut app, &keymap, KeyCode::Char('k'));
    assert_eq!(picker(&app).cursor, 0, "clamped at the top");
    assert_eq!(picker(&app).run_len(), 3);
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.scope, Scope::Commits);
    let pick = app.commit_pick.clone().unwrap();
    assert_eq!((pick.oldest.as_str(), pick.newest.as_str()), (shas[1].as_str(), shas[3].as_str()));
    assert_eq!(changed_paths(&app), ["one.rs", "three.rs", "two.rs"]);
    assert_eq!(app.pick_status.as_ref().unwrap().count, 3);

    // Second `esc` with no anchor closes.
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert_eq!(picker(&app).anchor, Some(2), "a run reopens with its anchor on the oldest");
    assert_eq!(picker(&app).cursor, 0, "and the highlight on the newest");
    press(&mut app, &keymap, KeyCode::Esc);
    press(&mut app, &keymap, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.scope, Scope::Commits, "a cancel leaves the pick and the scope alone");
}

#[test]
fn a_single_pick_reopens_without_an_anchor_so_k_enter_steps() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.commit_pick, Some(herdr_reviewr::model::CommitPick::single(&shas[1])));
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert_eq!(picker(&app).cursor, 2, "the highlight opens on the pick");
    assert!(picker(&app).anchor.is_none(), "a run of one reopens with no anchor");
    press(&mut app, &keymap, KeyCode::Char('k'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.commit_pick, Some(herdr_reviewr::model::CommitPick::single(&shas[2])));
    assert_eq!(changed_paths(&app), ["two.rs"]);
}

#[test]
fn every_other_key_is_inert_inside_the_commit_picker() {
    let (r, _) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    for code in [
        KeyCode::Char('q'),
        KeyCode::Char('/'),
        KeyCode::Char('1'),
        KeyCode::Char('2'),
        KeyCode::Char('3'),
        KeyCode::Char('u'),
        KeyCode::Char('b'),
        KeyCode::Char('t'),
        KeyCode::Char('g'),
        KeyCode::Char('G'),
        KeyCode::Char('B'),
        KeyCode::Char('c'),
        KeyCode::Char('y'),
        KeyCode::Char('l'),
        KeyCode::Char('?'),
        KeyCode::Tab,
    ] {
        press(&mut app, &keymap, code);
        assert_eq!(app.mode, Mode::CommitPick, "{code:?} is inert");
        assert_eq!(app.tab, herdr_reviewr::app::Tab::Changes);
        assert_eq!(app.scope, Scope::Uncommitted);
        assert!(!app.should_quit);
        assert!(!app.keys_expanded);
    }
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        Rect::new(0, 0, 120, 40),
        &keymap,
    )
    .unwrap();
    assert_eq!(app.mode, Mode::CommitPick, "ctrl+f is inert");
    // The page keys move the highlight, clamped.
    press(&mut app, &keymap, KeyCode::PageDown);
    assert_eq!(picker(&app).cursor, 3);
    press(&mut app, &keymap, KeyCode::PageUp);
    assert_eq!(picker(&app).cursor, 0);
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        Rect::new(0, 0, 120, 40),
        &keymap,
    )
    .unwrap();
    assert_eq!(picker(&app).cursor, 3, "ctrl+d pages too");
}

#[test]
fn an_off_branch_pick_keeps_painting_as_a_row_above_the_list() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.commit_pick, Some(herdr_reviewr::model::CommitPick::single(&shas[3])));
    // Rewrite the tip under the pick.
    r.git(&["reset", "-q", "--hard", &shas[2]]);
    r.write("three.rs", "rewritten\n");
    r.commit_all("three again");
    common::land_world(&mut app);
    assert_eq!(
        app.pick_status.as_ref().map(|s| &s.verdict),
        Some(&herdr_reviewr::world::PickVerdict::OffBranch)
    );
    assert_eq!(changed_paths(&app), ["three.rs"], "the run still paints");
    assert!(!app.commits_gone());

    press(&mut app, &keymap, KeyCode::Char('G'));
    let cp = picker(&app);
    assert_eq!(cp.pick_row.as_ref(), app.commit_pick.as_ref(), "the pick is a row above the list");
    assert_eq!(cp.cursor, 0);
    assert_eq!(cp.rows.len(), 4);
    assert!(cp.is_pick_row(0));
    // The pick row takes no anchor, and `enter` on it re-picks.
    press(&mut app, &keymap, KeyCode::Char('v'));
    assert!(picker(&app).anchor.is_none());
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Char('v'));
    press(&mut app, &keymap, KeyCode::Char('k'));
    assert_eq!(picker(&app).run_len(), 1, "the pick row sits outside every run");
    assert!(!picker(&app).in_run(0) && !picker(&app).in_run(1), "so no row wears the bar");
    assert_eq!(picker(&app).picked(), app.commit_pick, "and enter on the pick row re-picks it");
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.commit_pick, Some(herdr_reviewr::model::CommitPick::single(&shas[3])));
    assert_eq!(app.scope, Scope::Commits);

    // A base change marks nothing.
    r.set_origin_default("main", &shas[1]);
    common::land_world(&mut app);
    assert_eq!(
        app.pick_status.as_ref().map(|s| &s.verdict),
        Some(&herdr_reviewr::world::PickVerdict::OffBranch)
    );
}

#[test]
fn a_gone_pick_reads_as_gone_and_g_reopens_the_picker() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Enter);
    r.git(&["reset", "-q", "--hard", &shas[2]]);
    r.git(&["reflog", "expire", "--expire=now", "--all"]);
    r.git(&["gc", "-q", "--prune=now"]);
    common::land_world(&mut app);
    assert!(app.commits_gone());
    assert_eq!(
        app.pick_status.as_ref().map(|s| &s.verdict),
        Some(&herdr_reviewr::world::PickVerdict::Gone(shas[3].clone()))
    );
    assert_eq!(app.commits_gone_message(), format!("commit {} is gone", &shas[3][..7]));
    assert!(changed_paths(&app).is_empty(), "the scope is empty");
    let bands = app.footer_bands();
    assert_eq!(bands[0], (FooterAction::CommitPick, Band::Primary), "row 1 leads with the picker");
    assert!(bands.iter().any(|&(a, _)| a == FooterAction::ScopeOther));

    press(&mut app, &keymap, KeyCode::Char('g'));
    assert_eq!(app.mode, Mode::CommitPick, "g over a gone pick opens the picker");
    assert!(picker(&app).pick_row.is_some(), "the gone pick is the row above the list");
    press(&mut app, &keymap, KeyCode::Esc);
    assert_eq!(app.scope, Scope::Commits, "esc leaves the scope where it was");
    // `All files` keeps its content, and its own footer.
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    assert!(!app.entries.is_empty());
    let bands = app.footer_bands();
    assert_ne!(bands[0].0, FooterAction::CommitPick, "the gone row is the Changes tab's");
    assert!(bands.iter().any(|&(a, _)| a == FooterAction::TogglePane));
    // With the pick gone, `g` opens the picker instead.
    enter_tab(&mut app, herdr_reviewr::app::Tab::Changes);
    app.set_scope(Scope::Uncommitted).unwrap();
    press(&mut app, &keymap, KeyCode::Char('g'));
    assert_eq!(app.mode, Mode::CommitPick);
    assert_eq!(app.scope, Scope::Uncommitted, "without switching");
    // The chip's cycle skips the gone pick.
    press(&mut app, &keymap, KeyCode::Esc);
    app.set_scope(Scope::LastTurn).unwrap();
    assert_eq!(app.next_chip_scope(), Scope::Uncommitted);
}

#[test]
fn the_chip_skips_commits_until_a_pick_exists() {
    let (r, _) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.set_scope(Scope::LastTurn).unwrap();
    assert_eq!(app.next_chip_scope(), Scope::Uncommitted, "no pick yet");
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Enter);
    app.set_scope(Scope::LastTurn).unwrap();
    assert_eq!(app.next_chip_scope(), Scope::Commits, "a live pick is a chip stop");
}

#[test]
fn edit_never_lands_a_commit_comment_on_a_worktree_line() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Enter);
    app.select_file(0).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("three.rs"));
    // On a commit's diff, `e` opens the file at its start.
    app.focus = herdr_reviewr::app::Focus::Diff;
    app.start_edit();
    let target = app.editor_request.take().unwrap();
    assert_eq!((target.path.as_str(), target.line), ("three.rs", 1));
    comment_on(&mut app, '+', "commit note");
    assert_eq!(
        app.store.get(0).unwrap().rev,
        herdr_reviewr::model::Rev::Commit(herdr_reviewr::model::CommitPick::single(&shas[3]))
    );
    // Under a worktree scope the commit comment isn't editable.
    app.set_scope(Scope::Uncommitted).unwrap();
    app.select_file(0).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("root.rs"));
    // Opened from the diff pane, the way a reviewer reaches it.
    app.focus = herdr_reviewr::app::Focus::Diff;
    app.open_list();
    assert!(!app.footer_bands().iter().any(|&(a, _)| a == FooterAction::EditComment));
    app.start_edit();
    assert_eq!(app.mode, Mode::List, "nothing opens");
    // On `All files`, `e` opens the worktree line, whatever the scope.
    app.close_list();
    app.set_scope(Scope::Commits).unwrap();
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    app.select_file(file_row(&app, "root.rs")).unwrap();
    app.focus = herdr_reviewr::app::Focus::Diff;
    app.start_edit();
    assert_eq!(app.editor_request.take().unwrap().line, 1, "root.rs is one line");
}

#[test]
fn the_base_picker_opens_on_the_persisted_pick_from_any_scope() {
    let (r, shas) = commits_repo();
    r.set_origin_default("main", &shas[1]);
    r.git(&["branch", "dev", &shas[2]]);
    herdr_reviewr::git::write_base_pick(r.path(), "dev").unwrap();
    let mut app = app_on(&r);
    assert_eq!(app.scope, Scope::Uncommitted, "branch was never visited");
    app.open_base_picker();
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.rows[bp.cursor].name(), "dev", "the highlight opens on the pick");
}

#[test]
fn on_the_base_branch_itself_the_picker_lists_the_last_fifty() {
    let (r, _) = commits_repo();
    r.set_origin_default("main", "HEAD");
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert_eq!(picker(&app).title, "commits · last 50");
    assert_eq!(picker(&app).rows.len(), 4);
    assert!(app.footer_bands().iter().any(|&(a, _)| a == FooterAction::CommitAnchor));
    // The pick row offers no `v`.
    press(&mut app, &keymap, KeyCode::Enter);
    r.git(&["reset", "-q", "--hard", "HEAD~1"]);
    r.git(&["commit", "-q", "--allow-empty", "-m", "other"]);
    common::land_world(&mut app);
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert!(picker(&app).is_pick_row(picker(&app).cursor));
    assert!(!app.footer_bands().iter().any(|&(a, _)| a == FooterAction::CommitAnchor));
}

#[test]
fn the_picker_title_follows_the_range_it_lists() {
    let (r, _) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    // A base with no merge-base (unrelated history) lists the last 50 and says so.
    r.git(&["checkout", "-q", "--orphan", "island"]);
    r.git(&["commit", "-q", "--allow-empty", "-m", "island"]);
    r.git(&["checkout", "-q", "main"]);
    r.set_origin_default("island", "island");
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert_eq!(picker(&app).title, "commits · last 50");
    assert_eq!(picker(&app).rows.len(), 4);
}

#[test]
fn a_stale_build_for_a_replaced_pick_is_discarded() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Enter);
    let stale = completion_for(&app, 3);
    // The reviewer re-picks before the build lands.
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.commit_pick, Some(herdr_reviewr::model::CommitPick::single(&shas[2])));
    assert!(herdr_reviewr::land_world_completion(&mut app, stale, 3));
    assert_eq!(changed_paths(&app), ["two.rs"], "the old pick's build never paints");
    assert_eq!(app.pick_status.as_ref().unwrap().subject, "two");
    assert!(app.world_request.is_some(), "and a fresh build is requested");
}

#[test]
fn a_poll_under_the_open_picker_reconciles_by_sha() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Char('v'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    assert_eq!((picker(&app).cursor, picker(&app).anchor), (2, Some(1)));
    // Two new commits land on top: the same shas keep the highlight and the anchor.
    r.write("four.rs", "4\n");
    r.commit_all("four");
    r.write("five.rs", "5\n");
    r.commit_all("five");
    common::land_world(&mut app);
    assert_eq!(app.mode, Mode::CommitPick);
    assert_eq!(picker(&app).rows.len(), 6);
    assert_eq!((picker(&app).cursor, picker(&app).anchor), (4, Some(3)));
    assert_eq!(picker(&app).list_row(4).unwrap().sha, shas[1]);
    // A poll that leaves `HEAD` where it is re-lists nothing: the rows stay as they are.
    r.write("six.rs", "6\n");
    common::land_world(&mut app);
    assert_eq!((picker(&app).cursor, picker(&app).anchor), (4, Some(3)));
    // A rewritten anchor falls back to its nearest survivor.
    r.git(&["reset", "-q", "--hard", &shas[1]]);
    common::land_world(&mut app);
    assert_eq!(picker(&app).rows.len(), 2);
    assert_eq!((picker(&app).cursor, picker(&app).anchor), (0, Some(0)));
    // Both gone: each clamps to the last row.
    r.git(&["reset", "-q", "--hard", &shas[0]]);
    common::land_world(&mut app);
    assert_eq!(picker(&app).rows.len(), 1);
    assert_eq!((picker(&app).cursor, picker(&app).anchor), (0, Some(0)), "clamped to the last row");
}

#[test]
fn a_resolved_base_lists_the_commits_over_its_merge_base() {
    let (r, shas) = commits_repo();
    r.set_origin_default("main", &shas[1]);
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert_eq!(picker(&app).title, "commits · 2 over main");
    let listed: Vec<&str> = picker(&app).rows.iter().map(|r| r.sha.as_str()).collect();
    assert_eq!(listed, [shas[3].as_str(), shas[2].as_str()], "newest first, the base excluded");
}

#[test]
fn a_poll_moves_the_pick_between_the_pick_row_and_the_list() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Char('v'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Enter);
    let run = herdr_reviewr::model::CommitPick { oldest: shas[1].clone(), newest: shas[2].clone() };
    assert_eq!(app.commit_pick, Some(run.clone()));
    // Reset below the run: the picker opens with the pick row above the one listed commit.
    r.git(&["reset", "-q", "--hard", &shas[0]]);
    common::land_world(&mut app);
    press(&mut app, &keymap, KeyCode::Char('G'));
    assert_eq!(picker(&app).pick_row, Some(run.clone()));
    assert_eq!((picker(&app).cursor, picker(&app).len()), (0, 2));
    // Back, the run is listed whole and spanned again.
    r.git(&["reset", "-q", "--hard", &shas[3]]);
    common::land_world(&mut app);
    assert_eq!(app.mode, Mode::CommitPick);
    assert!(picker(&app).pick_row.is_none());
    assert_eq!(picker(&app).list_row(picker(&app).cursor).unwrap().sha, shas[2]);
    assert_eq!(picker(&app).list_row(picker(&app).anchor.unwrap()).unwrap().sha, shas[1]);
    assert_eq!(picker(&app).run_len(), 2);
    // Away again, the pick row returns.
    r.git(&["reset", "-q", "--hard", &shas[0]]);
    common::land_world(&mut app);
    assert_eq!(picker(&app).pick_row, Some(run));
    assert_eq!(picker(&app).list_row(picker(&app).cursor).unwrap().sha, shas[0]);
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.commit_pick.as_ref().map(|p| p.newest.as_str()), Some(shas[0].as_str()));
}

#[test]
fn a_commit_comment_renders_only_while_the_scope_reads_that_commit() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    // A worktree comment on the uncommitted edit.
    app.select_file(0).unwrap();
    comment_on(&mut app, '+', "worktree note");
    assert_eq!(app.store.get(0).unwrap().rev, herdr_reviewr::model::Rev::Worktree);
    assert_eq!(app.comment_marks().1.len(), 1);

    // A commit comment on `two`.
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Enter);
    app.select_file(0).unwrap();
    comment_on(&mut app, '+', "commit note");
    assert_eq!(
        app.store.get(1).unwrap().rev,
        herdr_reviewr::model::Rev::Commit(herdr_reviewr::model::CommitPick::single(&shas[2]))
    );
    assert_eq!(app.comment_marks().1.len(), 1, "only the commit comment renders here");

    // Another run is another diff: the comment hides until its pick returns.
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Char('k'));
    press(&mut app, &keymap, KeyCode::Char('v'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(
        app.commit_pick,
        Some(herdr_reviewr::model::CommitPick { oldest: shas[1].clone(), newest: shas[3].clone() })
    );
    app.select_file(file_row(&app, "two.rs")).unwrap();
    assert!(app.comment_marks().1.is_empty(), "another run's diff carries no card");
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Esc);
    press(&mut app, &keymap, KeyCode::Char('j'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.commit_pick, Some(herdr_reviewr::model::CommitPick::single(&shas[2])));
    app.select_file(file_row(&app, "two.rs")).unwrap();
    assert_eq!(app.comment_marks().1.len(), 1, "its own pick shows the card again");

    // Back on a worktree scope, the worktree comment renders and the commit one hides.
    app.set_scope(Scope::Uncommitted).unwrap();
    app.select_file(0).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("root.rs"));
    assert_eq!(app.comment_marks().1.len(), 1);
    r.set_origin_default("main", &shas[1]);
    app.set_scope(Scope::Branch).unwrap();
    app.select_file(file_row(&app, "root.rs")).unwrap();
    assert_eq!(app.comment_marks().1.len(), 1, "a worktree comment renders under branch too");

    // The list and the export carry both, unchanged.
    assert_eq!(app.store.len(), 2);
    let all: Vec<&herdr_reviewr::model::Comment> = app.store.iter().collect();
    let text = herdr_reviewr::export::format_all(&all);
    assert!(text.contains("worktree note") && text.contains("commit note"));
}

#[test]
fn all_files_marks_the_run_and_lists_the_worktree() {
    let (r, shas) = commits_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.commit_pick, Some(herdr_reviewr::model::CommitPick::single(&shas[3])));
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    let marked: Vec<&str> =
        app.entries.iter().filter(|e| e.annotation.is_some()).map(|e| e.path.as_str()).collect();
    assert_eq!(marked, ["three.rs"], "only the run's files carry a mark");
    assert!(app.entries.iter().any(|e| e.path == "root.rs"), "the tree lists the worktree");
}

#[test]
fn the_folder_dot_appears_under_a_poll_without_moving_the_cursor() {
    // A poll marks a folder without moving the cursor, even as rows shift (Continuity).
    let r = Repo::init();
    r.write("src/a.rs", "x\n");
    r.write("src/b.rs", "y\n");
    r.write("root.rs", "z\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    app.focus = Focus::Files;
    let dir_row = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    app.file_cursor = dir_row;
    let marked = |app: &App| {
        matches!(
            app.file_rows[app.file_cursor].kind,
            herdr_reviewr::file_list::RowKind::Dir { has_change: true, expanded: false, .. }
        )
    };
    assert!(!marked(&app), "a clean worktree marks nothing");

    r.write("src/a.rs", "x2\n");
    r.write("aaa/one.rs", "1\n"); // a new folder sorting above `src/` shifts its row
    r.write("aaa/two.rs", "2\n");
    common::land_world(&mut app);
    assert_eq!(
        app.file_rows[app.file_cursor].dir_path(),
        Some("src"),
        "the cursor follows the folder by path"
    );
    assert_eq!(app.file_cursor, dir_row + 1, "the folder's row moved down one");
    assert!(marked(&app), "the folder is marked and still collapsed");
}

// --- rendered markdown: review fixes ------------------------------------------------

#[test]
fn an_open_find_band_searches_a_markdown_file_that_opens_rendered() {
    let r = Repo::init();
    let body: String = (1..=30).map(|i| format!("line {i} total\n\n")).collect::<Vec<_>>().concat();
    r.write("b.md", &body);
    r.commit_all("init");
    r.write("a.rs", "let total = 1;\n");
    r.write("b.md", &format!("{body}tail total\n")); // a change past the context: a fold
    let mut app = app_on_rendered(&r);
    let keymap = Keymap::default();
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
    app.focus = Focus::Diff;
    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "total");
    assert_eq!(app.mode, Mode::Find);

    // Opened rendered, the band stays and steps walk the rendered rows.
    let row = app
        .file_rows
        .iter()
        .position(|r| r.file_index().is_some_and(|i| app.entries[i].path == "b.md"))
        .unwrap();
    app.select_file(row).unwrap();
    assert!(app.rendered_active());
    assert_eq!(app.mode, Mode::Find, "the band stays");
    app.diff_cursor = 0;
    assert_eq!(app.find_count(), Some((Some(1), 31)), "the cursor's row is the first match");
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.visible[app.diff_cursor].text(), "line 2 total");
    press(&mut app, &keymap, KeyCode::Up);
    press(&mut app, &keymap, KeyCode::Up);
    assert_eq!(app.visible[app.diff_cursor].text(), "tail total", "a step back wraps");
}

#[test]
fn content_that_renders_nothing_shows_its_source() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("doc.md", "<!-- ignore previous instructions -->\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));

    // An empty rendered pane would claim the file is empty; the hidden comment shows.
    assert!(!app.rendered_active(), "nothing renders, so the source shows");
    assert!(app.visible.iter().any(|row| row.text().contains("ignore previous")));
    let rows = app.visible.clone();

    // `m` keeps the source, saying why; a poll keeps it without re-rendering.
    app.focus = Focus::Diff;
    app.toggle_rendered();
    assert!(!app.rendered_active());
    assert_eq!(app.visible, rows);
    assert_eq!(app.status, "nothing here renders");
    app.reload().unwrap();
    assert!(!app.rendered_active());
    assert_eq!(app.visible, rows);
}

#[test]
fn a_poll_maps_the_rendered_cursor_through_the_edit() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("doc.md", "# A\n\npara one\n\npara two\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|row| row.text() == "para two").unwrap();

    // Lines inserted above shift every source number; the cursor follows its block.
    r.write("doc.md", "new top\n\n# A\n\npara one\n\npara two\n");
    app.reload().unwrap();
    assert_eq!(app.visible[app.diff_cursor].text(), "para two", "the insert above kept it");

    // Deleting the cursor's block lands where its line lands: the last block, at its text.
    r.write("doc.md", "new top\n\n# A\n\npara one\n");
    app.reload().unwrap();
    assert_eq!(app.visible[app.diff_cursor].text(), "para one", "not a gap row");
}

#[test]
fn a_width_rebuild_drops_a_highlight_whose_text_moved() {
    let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron \
                pi rho sigma tau upsilon phi chi psi omega";
    let r = Repo::init();
    r.write("base.txt", "x\n");
    r.commit_all("init");
    r.write("doc.md", &format!("# T\n\n{long}\n"));
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active());
    // Row 3 is the paragraph's second wrapped line; settle its first word.
    let (col, row) = sel_cell(&app, 3, 0);
    for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)]
        .into_iter()
        .cycle()
        .take(4)
    {
        sel_mouse(&mut app, kind, col, row);
    }
    assert!(app.settled_selection().is_some());

    // The frame hook rewraps at a new width: the row under the highlight holds other text.
    app.sync_rendered_width(30);
    assert!(app.settled_selection().is_none(), "a moved text drops its highlight");
}

#[test]
fn a_theme_change_rebuilds_rendered_rows_on_both_tabs() {
    use herdr_reviewr::app::Tab;
    let colors = painted_colors;
    let r = Repo::init();
    r.write("doc.md", "# Heading\n\nbody\n");
    r.commit_all("init");
    r.write("doc.md", "# Heading\n\nbody edited\n");
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active());
    let changes_before = colors(&app);
    enter_tab(&mut app, Tab::AllFiles);
    assert!(app.rendered_active());
    let files_before = colors(&app);

    app.set_cli_theme(Some("dracula".to_string()));
    app.sync_rendered_width(80); // the next frame
    assert_ne!(colors(&app), files_before, "the active tab rebuilt in the new palette");
    app.set_tab(Tab::Changes).unwrap();
    app.sync_rendered_width(80);
    assert_ne!(colors(&app), changes_before, "the stashed tab rebuilt on its first frame");
}

#[test]
fn a_source_comment_on_markdown_survives_the_view_flips() {
    let r = Repo::init();
    r.write("doc.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.write("doc.md", "# Doc\n\nbody edited\n");
    let mut app = app_on_rendered(&r);
    app.toggle_rendered(); // to source
    comment_on(&mut app, '+', "note");
    assert_eq!(app.store.len(), 1);
    let card_text = |app: &App| -> Vec<String> {
        app.card_rows().iter().map(|&(row, _)| app.visible[row].text()).collect()
    };
    assert_eq!(card_text(&app), ["body edited"]);

    app.toggle_rendered();
    assert!(app.rendered_active());
    assert_eq!(card_text(&app), ["body edited"], "the card sits under its block rendered");
    app.toggle_rendered();
    assert_eq!(app.store.len(), 1, "the comment survives the flips");
    assert_eq!(card_text(&app), ["body edited"], "and under its line in source again");
}

#[test]
fn a_transient_render_nothing_poll_returns_to_rendered() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("doc.md", "# A\n\npara one\n\npara two\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|row| row.text() == "para two").unwrap();

    // Mid-edit the file holds only a comment: its source shows, the choice untouched.
    r.write("doc.md", "<!-- wip -->\n");
    app.reload().unwrap();
    assert!(!app.rendered_active(), "nothing renders, so the source shows");
    assert!(app.diff_cursor < app.visible.len());

    // The content comes back: rendered again, the cursor in range on a text row.
    r.write("doc.md", "# A\n\npara one\n\npara two\n");
    app.reload().unwrap();
    assert!(app.rendered_active(), "content that renders shows rendered again");
    assert_eq!(app.visible[app.diff_cursor].text(), "A", "the cursor crosses back by its line");
}

#[test]
fn a_theme_change_and_an_edit_land_in_one_rebuild_keeping_the_cursor() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("doc.md", "# A\n\npara one\n\npara two\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|row| row.text() == "para two").unwrap();
    let before = painted_colors(&app);

    app.set_cli_theme(Some("dracula".to_string()));
    r.write("doc.md", "new top\n\n# A\n\npara one\n\npara two\n");
    app.reload().unwrap();
    assert_eq!(app.visible[app.diff_cursor].text(), "para two", "the cursor kept its block");
    assert_ne!(before, painted_colors(&app), "the rows repainted in the new theme");
}

#[test]
fn a_tab_switch_keeps_a_find_band_the_new_view_can_search() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("README.md", "# Title\n\ntotal\n");
    r.commit_all("init");
    r.write("a.rs", "let total = 1;\n");
    let mut app = app_on_rendered(&r);
    let keymap = Keymap::default();
    enter_tab(&mut app, Tab::AllFiles); // visit once: README.md opens rendered
    assert!(app.rendered_active());
    enter_tab(&mut app, Tab::Changes);
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
    app.focus = Focus::Diff;
    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "total");
    assert_eq!(app.mode, Mode::Find);

    // The stash swap paints All files' rendered README, which find searches too.
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(app.rendered_active());
    assert_eq!(app.mode, Mode::Find, "the band stays where it can search");
    app.find_step(1);
    assert_eq!(app.visible[app.diff_cursor].text(), "total");
}

#[test]
fn a_rebuild_that_moves_the_clicked_text_breaks_the_click_chain() {
    let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron \
                pi rho sigma tau upsilon phi chi psi omega";
    let r = Repo::init();
    r.write("base.txt", "x\n");
    r.commit_all("init");
    r.write("doc.md", &format!("# T\n\n{long}\n"));
    let mut app = app_on_rendered(&r);
    let (col, row) = sel_cell(&app, 3, 0);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), col, row);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), col, row);

    // A rewrap between presses makes the second a first click.
    app.sync_rendered_width(30);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), col, row);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), col, row);
    assert!(app.settled_selection().is_none(), "no word copied: the chain broke");
}

#[test]
fn a_line_prepended_inside_a_paragraph_keeps_the_cursor_row() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("doc.md", "# A\n\nline a\\\nline b\\\nline c\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|row| row.text() == "line c").unwrap();

    r.write("doc.md", "# A\n\nline z\\\nline a\\\nline b\\\nline c\n");
    app.reload().unwrap();
    assert_eq!(app.visible[app.diff_cursor].text(), "line c", "the cursor kept its row");
}

#[test]
fn editing_a_stale_comment_leaves_the_open_markdown_rendered() {
    let r = Repo::init();
    r.write("README.md", "# Title\n");
    r.commit_all("init");
    r.write("README.md", "# Title\n\nmore\n");
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active());
    // A comment on a file that left the changeset.
    app.store.add(herdr_reviewr::model::Comment {
        file: "gone.rs".into(),
        side: Side::New,
        start: 1,
        end: 1,
        lines: "+x".into(),
        text: "note".into(),
        diff_anchored: true,
        rev: herdr_reviewr::model::Rev::Worktree,
        draft: None,
    });
    app.open_list();
    app.start_edit();
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert!(app.rendered_active(), "an unrelated file keeps its rendered view");
}

/// Rendered: heading (1), paragraph (3-4, line 4 rewritten), list item (6).
fn rendered_review_app() -> (Repo, App) {
    let r = Repo::init();
    r.write("doc.md", "# Title\n\nalpha one\nbeta two\n\n- item\n");
    r.commit_all("init");
    r.write("doc.md", "# Title\n\nalpha one\nBETA TWO\n\n- item\n");
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active(), "the markdown file opens rendered");
    app.focus = Focus::Diff;
    (r, app)
}

/// The first rendered row whose text holds `needle`.
fn rendered_row(app: &App, needle: &str) -> usize {
    (0..app.visible.len()).find(|&i| painted(app, i).contains(needle)).expect("a row with the text")
}

/// The read pane's painted foreground colors, cell by cell, as one frame paints them.
fn painted_colors(app: &App) -> Vec<ratatui::style::Color> {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| herdr_reviewr::ui::render(f, app)).unwrap();
    let inner = herdr_reviewr::ui::read_inner_rect(Rect::new(0, 0, 120, 40), app);
    let buf = terminal.backend().buffer();
    (inner.y..inner.y + inner.height)
        .flat_map(|y| (inner.x..inner.x + inner.width).map(move |x| (x, y)))
        .filter(|&(x, y)| !buf[(x, y)].symbol().trim().is_empty())
        .map(|(x, y)| buf[(x, y)].fg)
        .collect()
}

/// Row `i`'s text as painted.
fn painted(app: &App, i: usize) -> String {
    match &app.visible[i] {
        Row::Rendered { kind: RenderedKind::Marker { kind, lines, .. }, .. } => {
            herdr_reviewr::ui::marker_text(*kind, *lines, "m")
        }
        row => row.text(),
    }
}

/// Write `text` in the open composer and save it.
fn write_comment(app: &mut App, text: &str) {
    assert!(app.composing(), "the composer is open");
    typed(app, text);
    app.submit_comment();
}

#[test]
fn a_rendered_comment_equals_the_source_comment() {
    let (_repo, mut app) = rendered_review_app();
    // Rendered: `c` on the modified paragraph.
    app.diff_cursor = rendered_row(&app, "alpha one");
    app.start_comment();
    write_comment(&mut app, "tighten this");
    let rendered = app.store.get(0).unwrap().clone();
    let rendered_export = herdr_reviewr::export::format_all(&[&rendered]);
    assert_eq!((rendered.side, rendered.start, rendered.end), (Side::New, 3, 4));
    assert_eq!(
        rendered.lines, " alpha one\n-beta two\n+BETA TWO",
        "the block's diff rows, its deletion included"
    );

    // Source: the same diff rows by `v` + `c`.
    app.store.take(0);
    app.toggle_rendered();
    assert!(!app.rendered_active());
    app.diff_cursor = app.visible.iter().position(|r| r.new_no() == Some(3)).unwrap();
    app.toggle_select();
    app.diff_cursor = app.visible.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
    write_comment(&mut app, "tighten this");
    let source = app.store.get(0).unwrap().clone();
    assert_eq!(source, rendered, "same side, range, snippet, kind, and rev");
    assert_eq!(herdr_reviewr::export::format_all(&[&source]), rendered_export);

    // Two rendered blocks anchor lines 1..=4, the blank between included.
    app.store.take(0);
    app.toggle_rendered();
    app.diff_cursor = 0;
    app.toggle_select();
    app.diff_cursor = rendered_row(&app, "alpha one");
    app.start_comment();
    write_comment(&mut app, "both");
    let rendered = app.store.take(0).unwrap();
    assert_eq!((rendered.side, rendered.start, rendered.end), (Side::New, 1, 4));
    assert_eq!(rendered.lines, " # Title\n \n alpha one\n-beta two\n+BETA TWO");
    let rendered_export = herdr_reviewr::export::format_all(&[&rendered]);

    app.toggle_rendered();
    app.diff_cursor = app.visible.iter().position(|r| r.new_no() == Some(1)).unwrap();
    app.toggle_select();
    app.diff_cursor = app.visible.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
    write_comment(&mut app, "both");
    let source = app.store.get(0).unwrap();
    assert_eq!(*source, rendered, "a multi-block range equals the contiguous source range");
    assert_eq!(herdr_reviewr::export::format_all(&[source]), rendered_export);
}

/// Comment the first `-` row in source, then return its card's rendered row.
fn old_side_card_row(old: &str, new: &str) -> (Repo, App, usize) {
    let r = Repo::init();
    r.write("doc.md", old);
    r.commit_all("init");
    r.write("doc.md", new);
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    app.toggle_rendered();
    comment_on(&mut app, '-', "removed");
    app.toggle_rendered();
    assert!(app.rendered_active());
    let cards = app.card_rows();
    assert_eq!(cards.len(), 1, "the old-side comment shows rendered");
    let row = cards[0].0;
    (r, app, row)
}

#[test]
fn a_tail_deletion_belongs_to_its_own_block_even_before_a_blank_line() {
    // The paragraph's last line is removed; a blank line follows the removal.
    let (_r, mut app, row) =
        old_side_card_row("para one\nlast line\n\n# Next\n", "para one\n\n# Next\n");
    assert_eq!(row, rendered_row(&app, "para one"), "the card sits under its paragraph");

    // Anchoring the paragraph rendered takes the removed line with it.
    app.store.take(0);
    app.diff_cursor = row;
    app.start_comment();
    write_comment(&mut app, "para");
    let c = app.store.get(0).unwrap();
    assert_eq!(c.lines, " para one\n-last line");
}

#[test]
fn a_whole_paragraph_removed_between_blocks_sits_under_its_removed_marker() {
    let (_r, mut app, row) =
        old_side_card_row("# A\n\npara one\n\ngone para\n\n# C\n", "# A\n\npara one\n\n# C\n");
    let marker = rendered_row(&app, "removed");
    assert_eq!(row, marker, "a between-blocks deletion sits under its removed marker");
    assert!(marker > rendered_row(&app, "para one") && marker < rendered_row(&app, "C"));

    // Neither neighbour claims it; a range spanning both blocks takes it.
    app.store.take(0);
    app.diff_cursor = rendered_row(&app, "C");
    app.start_comment();
    write_comment(&mut app, "next");
    assert!(!app.store.get(0).unwrap().lines.contains("gone para"));
    app.store.take(0);
    let next = rendered_row(&app, "C");
    app.diff_cursor = rendered_row(&app, "para one");
    app.start_comment();
    write_comment(&mut app, "para");
    assert!(!app.store.get(0).unwrap().lines.contains("gone para"));
    app.store.take(0);
    app.toggle_select();
    app.diff_cursor = next;
    app.start_comment();
    write_comment(&mut app, "span");
    assert!(app.store.get(0).unwrap().lines.contains("-gone para"));
}

#[test]
fn an_all_files_rendered_comment_equals_the_source_comment() {
    use herdr_reviewr::app::Tab;
    let (_repo, mut app) = rendered_review_app();
    enter_tab(&mut app, Tab::AllFiles);
    assert!(app.rendered_active());
    app.focus = Focus::Diff;
    app.diff_cursor = rendered_row(&app, "BETA TWO");
    app.start_comment();
    write_comment(&mut app, "content note");
    let rendered = app.store.take(0).unwrap();
    assert!(!rendered.diff_anchored, "the File view anchors content, as on source");
    assert_eq!((rendered.start, rendered.end), (3, 4));

    app.toggle_rendered();
    app.diff_cursor = app.visible.iter().position(|r| r.new_no() == Some(3)).unwrap();
    app.toggle_select();
    app.diff_cursor += 1;
    app.start_comment();
    write_comment(&mut app, "content note");
    assert_eq!(app.store.get(0), Some(&rendered));
}

#[test]
fn the_gutter_click_and_drag_open_the_composer_on_rendered_rows() {
    let (_repo, mut app) = rendered_review_app();
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    let gutter_x = inner.x + 1;
    let para = rendered_row(&app, "alpha one");
    let para_y = inner.y + u16::try_from(para).unwrap();

    // A gutter click on the heading opens the composer there.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), gutter_x, inner.y);
    assert!(app.gutter_drag());
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), gutter_x, inner.y);
    assert!(app.composing());
    assert_eq!(app.selection_range(), (0, 0));
    assert_eq!(app.pending_location().as_deref(), Some("doc.md:1"));

    // While composing, the gutter is inert and the draft's anchor holds.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), gutter_x, para_y);
    assert!(!app.gutter_drag());
    assert_eq!(app.selection_range(), (0, 0), "the draft's anchor never moves");
    press(&mut app, &Keymap::default(), KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);

    // A gutter drag spans heading to paragraph and opens the composer on release.
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), gutter_x, inner.y);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), gutter_x, para_y);
    assert!(app.gutter_drag());
    assert_eq!(app.selection_range(), (0, para));
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), gutter_x, para_y);
    assert!(app.composing());
    write_comment(&mut app, "range note");
    let c = app.store.get(0).unwrap();
    assert_eq!((c.start, c.end), (1, 4));
    assert!(app.rendered_active(), "commenting never leaves the rendered view");
}

#[test]
fn list_jump_edit_and_delete_work_in_the_rendered_view() {
    let (_repo, mut app) = rendered_review_app();
    app.diff_cursor = rendered_row(&app, "alpha one");
    app.start_comment();
    write_comment(&mut app, "first");
    let card = app.card_rows()[0].0;
    assert_eq!(card, rendered_row(&app, "BETA TWO"), "the card sits under the block's last row");

    // Jump: from the top onto the commented block, rendered.
    app.diff_cursor = 0;
    app.jump_comment(1);
    assert!(app.comment_marks().1.contains(&app.diff_cursor), "the jump lands on the comment");
    assert!(app.rendered_active());

    // Edit under the cursor: the box opens in the card's place, still rendered.
    app.start_edit();
    assert!(matches!(app.mode, Mode::Composing { editing: Some(0) }));
    assert!(app.rendered_active(), "editing no longer flips to source");
    assert_eq!(app.diff_cursor, card, "the cursor lands on the card's row");
    app.input_push('!');
    app.submit_comment();
    assert_eq!(app.store.get(0).unwrap().text, "first!");

    // Edit from the list: the same landing, rendered.
    app.diff_cursor = 0;
    app.open_list();
    app.start_edit();
    assert!(matches!(app.mode, Mode::Composing { editing: Some(0) }));
    assert!(app.rendered_active());
    assert_eq!(app.diff_cursor, card);
    app.cancel_comment();
    assert_eq!(app.mode, Mode::List, "the list edit returns to the list");
    app.close_list();

    // Delete under the cursor, rendered.
    app.diff_cursor = rendered_row(&app, "alpha one");
    app.delete_comment();
    assert!(app.store.is_empty());
    assert!(app.rendered_active());
}

#[test]
fn a_poll_or_resize_during_a_rendered_draft_never_moves_its_anchor() {
    let (r, mut app) = rendered_review_app();
    app.sync_rendered_width(60);
    app.diff_cursor = rendered_row(&app, "BETA TWO");
    app.start_comment();
    typed(&mut app, "half");
    let rows = app.visible.clone();
    let range = app.selection_range();
    let location = app.pending_location();

    // The agent prepends lines, and the pane resizes, while the draft is open.
    r.write("doc.md", "intro\n\nmore\n\n# Title\n\nalpha one\nBETA TWO\n\n- item\n");
    app.reload().unwrap();
    app.sync_rendered_width(40);
    assert_eq!(app.visible, rows, "the rendered rows are frozen under the draft");
    assert_eq!(app.selection_range(), range);
    assert_eq!(app.pending_location(), location);

    app.submit_comment();
    let c = app.store.get(0).unwrap();
    assert_eq!((c.start, c.end), (3, 4), "the comment anchors where the draft was written");
}

#[test]
fn comments_survive_view_flips_and_polls_in_both_views() {
    let (r, mut app) = rendered_review_app();
    // Made rendered, shown in source.
    app.diff_cursor = rendered_row(&app, "alpha one");
    app.start_comment();
    write_comment(&mut app, "rendered-made");
    app.toggle_rendered();
    assert!(!app.rendered_active());
    assert_eq!(app.card_rows().len(), 1, "a rendered comment shows in source");

    // In source: one on the deletion, one on uncovered blank line 5.
    comment_on(&mut app, '-', "old-side");
    app.diff_cursor = app.visible.iter().position(|r| r.new_no() == Some(5)).unwrap();
    app.start_comment();
    write_comment(&mut app, "blank");
    assert_eq!(app.store.get(1).unwrap().side, Side::Old);
    assert_eq!(app.card_rows().len(), 3);

    // Rendered, every card shows under its nearest block.
    app.toggle_rendered();
    assert!(app.rendered_active());
    let cards = app.card_rows();
    assert_eq!(cards.len(), 3, "no comment hides rendered: {cards:?}");
    let para_end = rendered_row(&app, "BETA TWO");
    let item = rendered_row(&app, "item");
    assert!(cards.contains(&(para_end, 0)));
    assert!(cards.contains(&(para_end, 1)), "the deletion's card sits under its block");
    assert!(cards.contains(&(item, 2)), "a blank line's card sits under the block below it");

    // A poll that edits elsewhere drops and hides nothing, in either view.
    r.write("doc.md", "# Title\n\nalpha one\nBETA TWO\n\n- item\n- more\n");
    app.reload().unwrap();
    assert_eq!(app.store.len(), 3);
    assert_eq!(app.card_rows().len(), 3, "rendered, after the poll");
    app.toggle_rendered();
    assert_eq!(app.card_rows().len(), 3, "source, after the poll");
}

#[test]
fn a_deletion_at_the_end_of_the_file_renders_under_the_last_block() {
    let r = Repo::init();
    r.write("doc.md", "# Title\n\nbody\n\ngone\n");
    r.commit_all("init");
    r.write("doc.md", "# Title\n\nbody\n");
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    app.toggle_rendered();
    comment_on(&mut app, '-', "why remove");
    assert_eq!(app.store.get(0).unwrap().side, Side::Old);
    app.toggle_rendered();
    assert!(app.rendered_active());
    let last = app.visible.len() - 1;
    assert_eq!(app.card_rows(), vec![(last, 0)], "the last block holds an EOF deletion");
}

/// Open `doc.md` edited from `old` to `new`, rendered, the read pane focused.
fn replaced_line_app(old: &str, new: &str) -> (Repo, App) {
    let r = Repo::init();
    r.write("doc.md", old);
    r.commit_all("init");
    r.write("doc.md", new);
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active());
    app.focus = Focus::Diff;
    (r, app)
}

/// The rendered block source line of row `i`.
fn rendered_src(app: &App, i: usize) -> u32 {
    match &app.visible[i] {
        herdr_reviewr::diff::Row::Rendered { src, .. } => *src,
        row => panic!("not a rendered row: {row:?}"),
    }
}

#[test]
fn a_replaced_line_belongs_to_its_replacement_block() {
    // (old, new, the replacement's rendered text, the block above's text, the snippet)
    let cases = [
        ("- a\n- b\n- c\n", "- a\n- B\n- c\n", "B", "a", "-- b\n+- B"),
        (
            "| h |\n|---|\n| x |\n| y |\n",
            "| h |\n|---|\n| X |\n| y |\n",
            "X",
            "h",
            "-| x |\n+| X |",
        ),
        // The last code line holds the closing fence.
        ("```\nfoo\nbar\n```\n", "```\nfoo\nBAR\n```\n", "BAR", "foo", "-bar\n+BAR\n ```"),
        ("# H\npara one\n", "# H\nPARA one\n", "PARA", "H", "-para one\n+PARA one"),
    ];
    for (old, new, needle, above, snippet) in cases {
        let (_r, mut app) = replaced_line_app(old, new);
        let at = rendered_row(&app, needle);

        // Rendered: the replacement's block takes the removed line, the block above does not.
        app.diff_cursor = at;
        app.start_comment();
        write_comment(&mut app, "note");
        let rendered = app.store.take(0).unwrap();
        assert_eq!(rendered.lines, snippet, "{needle}");
        app.diff_cursor = rendered_row(&app, above);
        app.start_comment();
        write_comment(&mut app, "above");
        let c = app.store.take(0).unwrap();
        assert!(!c.lines.lines().any(|l| l.starts_with('-')), "{above}: {:?}", c.lines);

        // The same rows in source make the same comment.
        app.toggle_rendered();
        let first = app.visible.iter().position(|r| r.marker() == '-').unwrap();
        let last = app.visible.iter().rposition(|r| r.new_no() <= Some(rendered.end)).unwrap();
        app.diff_cursor = first;
        app.toggle_select();
        app.diff_cursor = last;
        app.start_comment();
        write_comment(&mut app, "note");
        let source = app.store.take(0).unwrap();
        assert_eq!(source, rendered, "{needle}: the source comment on the same rows");

        // An old-side comment on the replaced line carries its card under the replacement.
        app.diff_cursor = first;
        app.start_comment();
        write_comment(&mut app, "old");
        assert_eq!(app.store.get(0).unwrap().side, Side::Old);
        app.toggle_rendered();
        let at = rendered_row(&app, needle);
        let card = app.card_rows()[0].0;
        assert_eq!(rendered_src(&app, card), rendered_src(&app, at), "{needle}: card block");
    }
}

#[test]
fn an_old_side_comment_follows_its_line_restored_as_context() {
    let r = Repo::init();
    r.write("doc.md", "# A\n\n# B\n\nbody\n");
    r.commit_all("init");
    r.write("doc.md", "# A\n\nbody\n");
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    app.toggle_rendered();
    comment_on(&mut app, '-', "why drop B");
    assert_eq!(app.store.get(0).unwrap().side, Side::Old);

    // The agent restores `# B` and edits elsewhere.
    r.write("doc.md", "# A\n\n# B\n\nbody edited\n");
    app.reload().unwrap();
    let card = app.card_rows()[0].0;
    assert_eq!(app.visible[card].old_no(), Some(3), "source: the card on the restored line");
    app.toggle_rendered();
    assert!(app.rendered_active());
    let card = app.card_rows()[0].0;
    assert_eq!(rendered_src(&app, card), 3, "rendered: the card on the `# B` block");
}

#[test]
fn a_rendered_range_anchor_survives_a_resize() {
    let (_r, mut app) = {
        let r = Repo::init();
        let long = "word ".repeat(40);
        r.write("doc.md", &format!("# T\n\n{long}\n\n- item\n"));
        let app = app_on_rendered(&r);
        (r, app)
    };
    app.focus = Focus::Diff;
    app.sync_rendered_width(80);
    app.diff_cursor = rendered_row(&app, "item");
    app.toggle_select();
    let before = app.select_anchor.unwrap();

    app.sync_rendered_width(30);
    let anchor = app.select_anchor.unwrap();
    assert_ne!(anchor, before, "the paragraph rewrapped above the anchor");
    assert_eq!(anchor, rendered_row(&app, "item"), "the anchor kept its row by identity");
    app.start_comment();
    assert_eq!(app.pending_location().as_deref(), Some("doc.md:5"));
}

#[test]
fn two_comments_on_one_block_are_each_reachable() {
    let r = Repo::init();
    r.write("doc.md", "# T\n\nl1\nl2\nl3\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    app.focus = Focus::Diff;
    app.toggle_rendered();
    for (line, text) in [(3, "first"), (5, "second")] {
        app.diff_cursor = app.visible.iter().position(|r| r.new_no() == Some(line)).unwrap();
        app.start_comment();
        write_comment(&mut app, text);
    }
    app.toggle_rendered();
    assert!(app.rendered_active());
    let para = rendered_row(&app, "l1");
    assert_eq!(app.card_rows(), vec![(para, 0), (para, 1)], "both cards under the paragraph");

    // `]` steps card by card, and `e` edits the one it reached.
    app.diff_cursor = 0;
    app.jump_comment(1);
    assert_eq!(app.diff_cursor, para);
    app.start_edit();
    assert_eq!(app.mode, Mode::Composing { editing: Some(0) });
    app.cancel_comment();
    app.jump_comment(1);
    assert_eq!(app.diff_cursor, para);
    app.start_edit();
    assert_eq!(app.mode, Mode::Composing { editing: Some(1) }, "the second card is reachable");
    app.cancel_comment();
    app.jump_comment(-1);
    app.start_edit();
    assert_eq!(app.mode, Mode::Composing { editing: Some(0) }, "and back");
    app.cancel_comment();

    // `d` deletes the picked one.
    app.jump_comment(1);
    app.delete_comment();
    assert_eq!(app.store.len(), 1);
    assert_eq!(app.store.get(0).unwrap().text, "first");

    // Any other cursor move drops the pick: the first covering comment answers again.
    app.focus = Focus::Diff;
    app.move_cursor(-1).unwrap();
    app.move_cursor(1).unwrap();
    app.start_edit();
    assert_eq!(app.mode, Mode::Composing { editing: Some(0) });
    app.cancel_comment();
}

#[test]
fn a_card_click_picks_its_comment() {
    let r = Repo::init();
    r.write("doc.md", "# T\n\nl1\nl2\nl3\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
    app.focus = Focus::Diff;
    app.toggle_rendered();
    for (line, text) in [(3, "first"), (5, "second")] {
        app.diff_cursor = app.visible.iter().position(|r| r.new_no() == Some(line)).unwrap();
        app.start_comment();
        write_comment(&mut app, text);
    }
    app.toggle_rendered();
    let screen_y = |app: &App, needle: &str| -> u16 {
        let backend = ratatui::backend::TestBackend::new(SEL_AREA.width, SEL_AREA.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| herdr_reviewr::ui::render(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..SEL_AREA.height)
            .find(|&y| {
                let line: String =
                    (0..SEL_AREA.width).map(|x| buf.cell((x, y)).unwrap().symbol()).collect();
                line.contains(needle)
            })
            .expect("the card paints")
    };
    let inner = herdr_reviewr::ui::read_inner_rect(SEL_AREA, &app);
    for (needle, index) in [("second", 1), ("first", 0)] {
        let y = screen_y(&app, needle);
        sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), inner.x + 4, y);
        sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), inner.x + 4, y);
        app.start_edit();
        assert_eq!(app.mode, Mode::Composing { editing: Some(index) }, "clicked {needle}");
        app.cancel_comment();
    }
}

// --- rendered change marks -----------------------------------------------------

/// `a.md` with every kind of mark, `b.md` with one changed paragraph.
fn marked_repo() -> Repo {
    let r = Repo::init();
    r.write(
        "a.md",
        "# A\n\npara one\n\nkeep one\n\ngone para\n\nkeep two\n\n<!-- x -->\n\n- item\n",
    );
    r.write("b.md", "# B\n\nold words\n");
    r.commit_all("init");
    r.write("a.md", "# A\n\npara ONE\n\nkeep one\n\nkeep two\n\n<!-- y -->\n\n- item\n- added\n");
    r.write("b.md", "# B\n\nnew words\n");
    r
}

/// The change mark the row under the cursor wears.
fn cursor_mark(app: &App) -> Wears {
    wears(&app.visible[app.diff_cursor])
}

/// The change mark a rendered row wears: its block's bar, or the marker it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wears {
    Nothing,
    Bar(Bar),
    Marker(MarkerKind),
}

fn wears(row: &Row) -> Wears {
    match row {
        Row::Rendered { kind: RenderedKind::Block { bar: None, .. }, .. } => Wears::Nothing,
        Row::Rendered { kind: RenderedKind::Block { bar: Some(b), .. }, .. } => Wears::Bar(*b),
        Row::Rendered { kind: RenderedKind::Marker { kind, .. }, .. } => Wears::Marker(*kind),
        row => panic!("not a rendered row: {row:?}"),
    }
}

#[test]
fn hunk_steps_visit_rendered_marks_in_order_and_cross_files() {
    let r = marked_repo();
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    assert_eq!(app.diff_path.as_deref(), Some("a.md"));
    assert!(app.rendered_active());
    app.diff_cursor = 0;

    let mut stops = Vec::new();
    for _ in 0..4 {
        app.next_hunk();
        stops.push((cursor_text(&app), cursor_mark(&app)));
    }
    assert_eq!(
        stops,
        vec![
            ("para ONE".to_string(), Wears::Bar(Bar::Modified)),
            ("− 1 line removed".to_string(), Wears::Marker(MarkerKind::Removed)),
            (
                "⚠ 1 changed line doesn't render · m to see".to_string(),
                Wears::Marker(MarkerKind::Unrendered)
            ),
            ("• added".to_string(), Wears::Bar(Bar::Added)),
        ]
    );
    // Back up the same stops.
    app.prev_hunk();
    assert_eq!(cursor_mark(&app), Wears::Marker(MarkerKind::Unrendered));
    app.prev_hunk();
    assert_eq!(cursor_mark(&app), Wears::Marker(MarkerKind::Removed));
    app.prev_hunk();
    assert_eq!(cursor_text(&app), "para ONE");

    // Past the last mark, arm and cross to the next file's first.
    for _ in 0..3 {
        app.next_hunk();
    }
    app.next_hunk();
    assert_eq!(app.armed_cross(), Some(true));
    app.next_hunk();
    assert_eq!(app.diff_path.as_deref(), Some("b.md"));
    assert!(app.rendered_active());
    assert_eq!(
        (cursor_text(&app), cursor_mark(&app)),
        ("new words".to_string(), Wears::Bar(Bar::Modified))
    );
}

#[test]
fn a_removed_marker_comment_anchors_the_deleted_lines_on_the_old_side() {
    let r = marked_repo();
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    let marker = rendered_row(&app, "removed");
    app.diff_cursor = marker;
    app.start_comment();
    write_comment(&mut app, "why drop it?");
    let c = app.store.get(0).unwrap().clone();
    assert_eq!(c.side, Side::Old);
    assert!(c.lines.contains("-gone para"), "{:?}", c.lines);
    assert!(!c.lines.contains("keep"), "a marker stands for its own lines alone");
    let out = herdr_reviewr::export::format_all(&[&c]);
    assert!(out.contains("(removed)"), "{out}");
    // Its card sits under the marker.
    assert_eq!(app.card_rows(), vec![(marker, 0)]);

    // The agent restores the paragraph: the card follows the restored lines.
    r.write(
        "a.md",
        "# A\n\npara ONE\n\nkeep one\n\ngone para\n\nkeep two\n\n<!-- y -->\n\n- item\n- added\n",
    );
    app.reload().unwrap();
    assert_eq!(app.card_rows(), vec![(rendered_row(&app, "gone para"), 0)]);
}

#[test]
fn an_unrendered_marker_comment_anchors_its_changed_source() {
    let r = marked_repo();
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    let marker = rendered_row(&app, "doesn't render");
    app.diff_cursor = marker;
    app.start_comment();
    write_comment(&mut app, "hidden instruction?");
    let c = app.store.get(0).unwrap().clone();
    assert_eq!((c.side, c.start, c.end), (Side::New, 9, 9));
    assert_eq!(c.lines, "-<!-- x -->\n+<!-- y -->");
    assert_eq!(app.card_rows(), vec![(marker, 0)]);

    // The same lines commented in source make the same comment.
    let rendered = app.store.take(0).unwrap();
    app.toggle_rendered();
    app.diff_cursor = app.visible.iter().position(|r| r.text() == "<!-- x -->").unwrap();
    app.toggle_select();
    app.diff_cursor = app.visible.iter().position(|r| r.text() == "<!-- y -->").unwrap();
    app.start_comment();
    write_comment(&mut app, "hidden instruction?");
    assert_eq!(*app.store.get(0).unwrap(), rendered);
}

#[test]
fn a_changed_details_opens_and_a_reviewer_collapse_holds_across_a_poll() {
    let r = Repo::init();
    let doc = |body: &str| {
        format!(
            "Intro\n\n<details>\n<summary>More</summary>\n\n{body}\n\n</details>\n\n\
             <details>\n<summary>Quiet</summary>\n\nsame\n\n</details>\n"
        )
    };
    r.write("doc.md", &doc("body one"));
    r.commit_all("init");
    r.write("doc.md", &doc("body two"));
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    assert!(app.rendered_active());

    // The changed disclosure opens itself; the quiet one stays shut.
    let body = rendered_row(&app, "body two");
    assert_eq!(wears(&app.visible[body]), Wears::Bar(Bar::Modified));
    assert!(app.visible.iter().any(|r| r.text().contains("▸ Quiet")));
    assert!(!app.visible.iter().any(|r| r.text() == "same"));

    // The reviewer collapses it: the summary carries the aggregate mark.
    app.toggle_details("More#0");
    assert!(!app.visible.iter().any(|r| r.text().contains("body two")));
    let summary = rendered_row(&app, "▸ More");
    assert!(
        matches!(
            app.visible[summary],
            Row::Rendered { kind: RenderedKind::Block { hides: Some(2), .. }, .. }
        ),
        "the summary carries the changed lines its body hides"
    );
    assert!(!app.visible[summary].text().contains("changed"), "copy and find never read the note");
    assert_eq!(wears(&app.visible[summary]), Wears::Bar(Bar::Modified));

    // A poll with a fresh change inside never reopens it against the reviewer's choice.
    r.write("doc.md", &doc("body three"));
    app.reload().unwrap();
    assert!(!app.visible.iter().any(|r| r.text().contains("body three")));
    assert!(rendered_row(&app, "▸ More") < app.visible.len());

    // An old-side comment inside the collapsed body shows its card under the summary.
    app.toggle_rendered();
    comment_on(&mut app, '-', "lost");
    app.toggle_rendered();
    let summary = rendered_row(&app, "▸ More");
    assert_eq!(app.card_rows(), vec![(summary, 0)]);
}

#[test]
fn the_files_tab_shows_no_change_marks() {
    use herdr_reviewr::app::Tab;
    let r = marked_repo();
    let mut app = app_on_rendered(&r);
    assert!(app.visible.iter().any(|r| wears(r) != Wears::Nothing));
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("a.md"));
    assert!(app.rendered_active());
    assert!(app.visible.iter().all(|r| wears(r) == Wears::Nothing));
}

#[test]
fn a_comment_opens_its_disclosure_on_the_reviewers_own_input_never_on_a_later_poll() {
    let r = Repo::init();
    let doc = |tail: &str| {
        format!(
            "Intro\n\n<details>\n<summary>Quiet</summary>\n\nsame body\n\n</details>\n\n{tail}\n"
        )
    };
    r.write("doc.md", &doc("old tail"));
    r.commit_all("init");
    r.write("doc.md", &doc("new tail"));
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    let open = |app: &App| app.visible.iter().any(|r| r.text() == "same body");
    assert!(!open(&app), "an unchanged disclosure starts collapsed");

    // A comment on its body, made in source, holds it open rendered.
    app.toggle_rendered();
    app.diff_cursor = app.visible.iter().position(|r| r.hidden() > 0).expect("the fold");
    expand_fold(&mut app);
    app.diff_cursor = app.visible.iter().position(|r| r.text() == "same body").unwrap();
    app.start_comment();
    write_comment(&mut app, "why?");
    app.toggle_rendered();
    assert!(open(&app), "the commented disclosure opens");

    // Deleting the comment closes it on that keystroke.
    app.diff_cursor = rendered_row(&app, "same body");
    app.delete_comment();
    assert!(app.store.is_empty());
    assert!(!open(&app), "the delete closes it at once");

    // An unchanged poll changes nothing.
    let rows = app.visible.clone();
    app.reload().unwrap();
    assert_eq!(app.visible, rows);
}

#[test]
fn a_run_of_marked_lines_is_one_stop() {
    let r = Repo::init();
    r.write("doc.md", "# Code\n\n```\none\ntwo\nthree\n```\n\ntail\n");
    r.commit_all("init");
    r.write("doc.md", "# Code\n\n```\nONE\nTWO\nTHREE\n```\n\nTAIL\n");
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    app.next_hunk();
    assert!(cursor_text(&app).contains("ONE"), "{}", cursor_text(&app));
    app.next_hunk();
    assert_eq!(cursor_text(&app), "TAIL", "the three code lines were one stop");
    app.prev_hunk();
    assert!(cursor_text(&app).contains("ONE"));
}

#[test]
fn a_block_appended_after_another_leaves_it_unmarked_and_out_of_its_anchor() {
    let r = Repo::init();
    r.write("doc.md", "A\n");
    r.commit_all("init");
    r.write("doc.md", "A\n\nB\n");
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    let a = rendered_row(&app, "A");
    assert_eq!(wears(&app.visible[a]), Wears::Nothing);
    assert_eq!(wears(&app.visible[rendered_row(&app, "B")]), Wears::Bar(Bar::Added));
    app.diff_cursor = a;
    app.start_comment();
    write_comment(&mut app, "keep");
    assert_eq!(app.store.get(0).unwrap().lines, " A", "the inserted blank is B's, not A's");
}

#[test]
fn edit_in_the_rendered_view_opens_at_the_cursor_blocks_first_source_line() {
    let r = marked_repo();
    let mut app = app_on_rendered(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    assert!(app.rendered_active());
    let resolve = |app: &mut App| {
        let t = app.editor_request.take().expect("`e` names the file");
        let path = r.path().join(&t.path);
        let cmd = herdr_reviewr::editor::resolve(None, None, Some("vim"), || None, &path, t.line)
            .unwrap();
        (t.line, cmd.args)
    };

    // A block opens at its first source line, whichever of its rows the cursor is on.
    app.diff_cursor = rendered_row(&app, "para ONE");
    press(&mut app, &keymap, KeyCode::Char('e'));
    let (line, args) = resolve(&mut app);
    assert_eq!(line, 3);
    assert_eq!(args[0], "+3");

    // A marker row opens at the line it sits at: the removed block's place.
    app.diff_cursor = rendered_row(&app, "removed");
    let Row::Rendered { src: at, .. } = app.visible[app.diff_cursor] else {
        panic!("a marker row")
    };
    press(&mut app, &keymap, KeyCode::Char('e'));
    assert_eq!(resolve(&mut app).0, at);
    assert_eq!(at, 6, "the blank line where `gone para` was, between `keep one` and `keep two`");

    // A marker past the file's last line — its last block removed — clamps to that line.
    let tail = Repo::init();
    tail.write("doc.md", "A\n\nB\n");
    tail.commit_all("init");
    tail.write("doc.md", "A\n");
    let mut app = app_on_rendered(&tail);
    app.focus = Focus::Diff;
    app.diff_cursor = rendered_row(&app, "removed");
    press(&mut app, &keymap, KeyCode::Char('e'));
    assert_eq!(app.editor_request.take().unwrap().line, 1);
}

#[test]
fn edit_in_a_rendered_commit_diff_opens_the_file_at_its_start() {
    let r = Repo::init();
    r.write("doc.md", "intro\n");
    r.commit_all("one");
    r.write("doc.md", "intro\n\nmore\n");
    r.commit_all("two");
    let mut app = app_on_rendered(&r);
    let keymap = Keymap::default();
    press(&mut app, &keymap, KeyCode::Char('G'));
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.scope, Scope::Commits);
    app.select_file(file_row(&app, "doc.md")).unwrap();
    assert!(app.rendered_active());
    app.focus = Focus::Diff;
    app.diff_cursor = rendered_row(&app, "more");
    app.start_edit();
    let target = app.editor_request.take().unwrap();
    assert_eq!((target.path.as_str(), target.line), ("doc.md", 1), "the commit's numbers stay put");
}

#[test]
fn the_footer_offers_the_rendered_view_only_where_something_renders() {
    use herdr_reviewr::app::Tab;
    let offers = |app: &App| app.footer_bands().iter().any(|&(a, _)| a == FooterAction::Rendered);
    let r = Repo::init();
    r.write("doc.md", "# A\n\nbody\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    assert!(offers(&app), "a rendered file offers its source");
    app.toggle_rendered();
    assert!(offers(&app), "its source offers the rendered view");

    // Content that renders nothing has no rendered view to offer.
    r.write("doc.md", "<!-- wip -->\n");
    app.reload().unwrap();
    app.toggle_rendered();
    app.toggle_rendered();
    assert!(!app.rendered_active());
    assert!(!offers(&app), "nothing renders, so `m rendered` stays silent");
}

#[test]
fn the_footer_offers_wrap_only_where_it_acts() {
    use herdr_reviewr::app::Tab;
    let offers = |app: &App| app.footer_bands().iter().any(|&(a, _)| a == FooterAction::Wrap);
    let r = Repo::init();
    r.write("doc.md", "# A\n\nbody\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    assert!(app.rendered_active());
    assert!(!offers(&app), "rendered rows come pre-wrapped, so `w` stays silent");
    app.toggle_rendered();
    assert!(offers(&app), "the source wraps");
}

#[test]
fn a_comment_step_lands_on_the_comments_first_row() {
    let (_r, mut app) = rendered_review_app();
    // A comment over the heading and the paragraph below it.
    app.diff_cursor = rendered_row(&app, "Title");
    app.toggle_select();
    app.diff_cursor = rendered_row(&app, "alpha one");
    app.start_comment();
    write_comment(&mut app, "both");
    let title = rendered_row(&app, "Title");
    app.diff_cursor = rendered_row(&app, "item");
    app.jump_comment(-1);
    assert_eq!(app.diff_cursor, title, "the step lands where the comment starts");
    app.start_edit();
    assert_eq!(app.input, "both", "and picks it");
    app.cancel_comment();
    app.jump_comment(1);
    assert_eq!(app.diff_cursor, title, "one comment, one stop");
}

#[test]
fn a_picked_comment_stays_picked_across_a_poll_that_moves_its_row() {
    use herdr_reviewr::app::Tab;
    let words = "word ".repeat(40);
    // (the poll's rewrite, the row the cursor sits on after it)
    let cases = [
        // The line above rewraps into more rows: the rows move, the lines do not.
        (format!("{}\n\npara one\n", "word ".repeat(60)), "para one"),
        // A long line joins the paragraph; the cursor keeps its line.
        (format!("# A\n\n{words}\npara one\n"), "para one"),
    ];
    for (rewrite, cursor_text) in cases {
        let r = Repo::init();
        r.write("doc.md", "# A\n\npara one\n");
        r.commit_all("init");
        let mut app = app_on_rendered(&r);
        enter_tab(&mut app, Tab::AllFiles);
        app.focus = Focus::Diff;
        // Wide enough that `para one` wraps to a row of its own after the rewrite.
        app.sync_rendered_width(100);
        // Two comments on one paragraph; the step picks the second.
        for text in ["first", "second"] {
            app.diff_cursor = rendered_row(&app, "para one");
            app.start_comment();
            write_comment(&mut app, text);
        }
        app.diff_cursor = 0;
        app.jump_comment(1);
        app.jump_comment(1);
        app.start_edit();
        assert_eq!(app.input, "second");
        app.cancel_comment();
        // The reviewer's own move off the comment's rows and back drops the pick.
        let keymap = Keymap::default();
        let back = app.diff_cursor;
        while app.comment_marks().1.contains(&app.diff_cursor) {
            press(&mut app, &keymap, KeyCode::Up);
        }
        while app.diff_cursor < back {
            press(&mut app, &keymap, KeyCode::Down);
        }
        app.start_edit();
        assert_eq!(app.input, "first", "a move of the reviewer's own drops the pick");
        app.cancel_comment();
        app.diff_cursor = 0;
        app.jump_comment(1);
        app.jump_comment(1);

        r.write("doc.md", &rewrite);
        app.reload().unwrap();
        assert_eq!(app.visible[app.diff_cursor].text(), cursor_text, "{rewrite:?}");
        app.start_edit();
        assert_eq!(app.input, "second", "a poll never re-points the pick: {rewrite:?}");
        app.cancel_comment();
        app.delete_comment();
        assert_eq!(app.store.get(0).map(|c| c.text.as_str()), Some("first"), "{rewrite:?}");
    }
}

/// Comments "first" and "second" on the same rows, the cursor at the top.
fn two_comments_app(rendered: bool) -> (Repo, App) {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    let para = "word ".repeat(44);
    r.write("doc.md", &format!("# A\n\n{para}\n"));
    r.write("a.rs", "one\ntwo\nthree\nfour\n");
    r.write("z.rs", "z\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    let path = if rendered { "doc.md" } else { "a.rs" };
    app.select_file(file_row(&app, path)).unwrap();
    app.focus = Focus::Diff;
    app.sync_rendered_width(60);
    for text in ["first", "second"] {
        if rendered {
            let lead = rendered_row(&app, "word");
            assert_eq!(
                app.visible.iter().filter(|r| r.text().starts_with("word")).count(),
                4,
                "the paragraph wraps to four rows"
            );
            app.diff_cursor = lead;
        } else {
            app.diff_cursor = 1;
            app.toggle_select();
            app.diff_cursor = 2;
        }
        app.start_comment();
        write_comment(&mut app, text);
    }
    app.diff_cursor = 0;
    (r, app)
}

/// The comment `e` opens, cancelled again.
fn edit_target_text(app: &mut App, keymap: &Keymap) -> String {
    press(app, keymap, KeyCode::Char('e'));
    let text = app.input.clone();
    press(app, keymap, KeyCode::Esc);
    text
}

#[test]
fn a_pick_is_the_comment_chosen_under_the_cursor() {
    let keymap = Keymap::default();
    for rendered in [true, false] {
        let n = |app: &mut App| {
            press(app, &keymap, KeyCode::Char('n'));
            press(app, &keymap, KeyCode::Char('n'));
        };

        // (a) After `e` moves the cursor, `d` still deletes the pick.
        let (_r, mut app) = two_comments_app(rendered);
        n(&mut app);
        assert_eq!(edit_target_text(&mut app, &keymap), "second", "rendered {rendered}");
        press(&mut app, &keymap, KeyCode::Char('d'));
        let left: Vec<_> = app.store.iter().map(|c| c.text.clone()).collect();
        assert_eq!(left, ["first"], "rendered {rendered}");

        // (b) A flip keeps the pick: the comment still covers the cursor's row.
        if rendered {
            let (_r, mut app) = two_comments_app(rendered);
            n(&mut app);
            press(&mut app, &keymap, KeyCode::Char('m'));
            assert!(!app.rendered_active());
            assert_eq!(edit_target_text(&mut app, &keymap), "second");
        }

        // (c) After an export, a stale pick never reaches the new index 1.
        let (_r, mut app) = two_comments_app(rendered);
        n(&mut app);
        let cursor = app.diff_cursor;
        assert!(app.export(&FakeTarget::ok()));
        for text in ["third", "fourth", "fifth"] {
            app.diff_cursor = cursor;
            app.start_comment();
            write_comment(&mut app, text);
        }
        app.diff_cursor = cursor;
        assert_eq!(edit_target_text(&mut app, &keymap), "fifth", "rendered {rendered}");

        // (e) The list's `e` picks its comment for `d`.
        let (_r, mut app) = two_comments_app(rendered);
        n(&mut app);
        press(&mut app, &keymap, KeyCode::Char('l'));
        assert_eq!(app.mode, Mode::List);
        app.list_cursor = 0;
        press(&mut app, &keymap, KeyCode::Char('e'));
        assert_eq!(app.input, "first");
        press(&mut app, &keymap, KeyCode::Esc);
        press(&mut app, &keymap, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        press(&mut app, &keymap, KeyCode::Char('d'));
        let left: Vec<_> = app.store.iter().map(|c| c.text.clone()).collect();
        assert_eq!(left, ["second"], "rendered {rendered}");

        // (f) A new comment on the same rows picks itself.
        let (_r, mut app) = two_comments_app(rendered);
        n(&mut app);
        press(&mut app, &keymap, KeyCode::Char('c'));
        for ch in "third".chars() {
            press(&mut app, &keymap, KeyCode::Char(ch));
        }
        press(&mut app, &keymap, KeyCode::Enter);
        assert_eq!(app.store.len(), 3);
        assert_eq!(edit_target_text(&mut app, &keymap), "third", "rendered {rendered}");

        // (g) The pick is the tab's place: away to another tab and back keeps it.
        let (_r, mut app) = two_comments_app(rendered);
        n(&mut app);
        press(&mut app, &keymap, KeyCode::Char('1'));
        press(&mut app, &keymap, KeyCode::Char('2'));
        if app.focus != Focus::Diff {
            press(&mut app, &keymap, KeyCode::Tab);
        }
        assert_eq!(edit_target_text(&mut app, &keymap), "second", "rendered {rendered}");

        // (h) Deleting another comment from the list leaves the pick alone.
        let (_r, mut app) = two_comments_app(rendered);
        n(&mut app);
        press(&mut app, &keymap, KeyCode::Char('c'));
        for ch in "third".chars() {
            press(&mut app, &keymap, KeyCode::Char(ch));
        }
        press(&mut app, &keymap, KeyCode::Enter);
        press(&mut app, &keymap, KeyCode::Char('l'));
        app.list_cursor = 0;
        press(&mut app, &keymap, KeyCode::Char('d'));
        press(&mut app, &keymap, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(edit_target_text(&mut app, &keymap), "third", "rendered {rendered}");

        // (d) Away and back, the cursor is off the comment and the pick is gone.
        let (_r, mut app) = two_comments_app(rendered);
        n(&mut app);
        press(&mut app, &keymap, KeyCode::Char('f'));
        press(&mut app, &keymap, KeyCode::Char('F'));
        assert_eq!(app.diff_cursor, 0);
        // Back onto the comments' rows by hand: the normal rule picks the first.
        let row = app.card_rows()[0].0;
        app.diff_cursor = row;
        assert_eq!(edit_target_text(&mut app, &keymap), "first", "rendered {rendered}");
    }
}

#[test]
fn the_rendered_cursor_keeps_its_source_line_and_wrap() {
    use herdr_reviewr::app::Tab;
    let para = "word ".repeat(30);
    let link = "aaa [link text\nmore words](http://example.com/a/b/c) end of\n\nlater\n";
    // (before, cursor row, poll rewrite, cursor row after, width, then width, row after it)
    let cases: Vec<(String, &str, String, &str, usize, usize, &str)> = vec![
        // A block above gives the paragraph a gap row; its wraps hold.
        (format!("{para}\n"), "2nd", format!("# T\n\n{para}\n"), "2nd", 60, 60, "2nd"),
        // And the block above goes again.
        (format!("# T\n\n{para}\n"), "2nd", format!("{para}\n"), "2nd", 60, 60, "2nd"),
        // A wrapped link url holds its row through a poll and a resize.
        (
            link.to_string(),
            "://example.com/a",
            link.replace("later", "later edited"),
            "://example.com/a",
            16,
            17,
            "//example.com/a/b",
        ),
    ];
    for (before, at, rewrite, after, width, then, resized) in cases {
        let r = Repo::init();
        r.write("doc.md", &before);
        r.commit_all("init");
        let mut app = app_on_rendered(&r);
        enter_tab(&mut app, Tab::AllFiles);
        app.focus = Focus::Diff;
        app.sync_rendered_width(width);
        // `2nd` names the paragraph's second row.
        let row = |app: &App, at: &str| {
            if at == "2nd" {
                app.visible
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.text().starts_with("word"))
                    .nth(1)
                    .unwrap()
                    .0
            } else {
                rendered_row(app, at)
            }
        };
        app.diff_cursor = row(&app, at);
        r.write("doc.md", &rewrite);
        app.reload().unwrap();
        assert_eq!(app.diff_cursor, row(&app, after), "{before:?} → {rewrite:?}");
        app.sync_rendered_width(then);
        assert_eq!(app.diff_cursor, row(&app, resized), "{rewrite:?} at {then}");
    }
}

// --- the markdown view preference ------------------------------------------------

/// Changed `a.md`, `b.md`, `c.md` (a comment alone: it renders nothing), and `d.md`.
fn markdown_files_repo() -> Repo {
    let r = Repo::init();
    for f in ["a.md", "b.md", "c.md", "d.md"] {
        r.write(f, "# base\n");
    }
    r.commit_all("init");
    r.write("a.md", "# A\n\nbody a\n");
    r.write("b.md", "# B\n\nbody b\n");
    r.write("c.md", "<!-- only a comment -->\n");
    r.write("d.md", "# D\n\nbody d\n");
    r
}

#[test]
fn markdown_opens_as_source_and_m_flips_every_markdown_file_after_it() {
    use herdr_reviewr::app::Tab;
    let r = markdown_files_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    assert_eq!(app.diff_path.as_deref(), Some("a.md"));
    assert!(!app.rendered_active(), "markdown opens as source by default");

    // `m` flips the pane: the next markdown file opens rendered too.
    press(&mut app, &keymap, KeyCode::Char('m'));
    assert!(app.rendered_active());
    press(&mut app, &keymap, KeyCode::Char('f'));
    assert_eq!(app.diff_path.as_deref(), Some("b.md"));
    assert!(app.rendered_active(), "the choice follows to the next file");

    // A file that renders nothing shows its source and offers no `m`, but the choice holds.
    press(&mut app, &keymap, KeyCode::Char('f'));
    assert_eq!(app.diff_path.as_deref(), Some("c.md"));
    assert!(!app.rendered_active());
    assert!(!app.footer_bands().iter().any(|&(a, _)| a == FooterAction::Rendered));
    press(&mut app, &keymap, KeyCode::Char('f'));
    assert_eq!(app.diff_path.as_deref(), Some("d.md"));
    assert!(app.rendered_active(), "the next renderable file opens rendered");

    // The other tab follows the same choice, both ways.
    press(&mut app, &keymap, KeyCode::Char('2'));
    assert_eq!(app.tab, Tab::AllFiles);
    assert!(app.rendered_active(), "All files opens markdown rendered too");
    app.focus = Focus::Diff;
    press(&mut app, &keymap, KeyCode::Char('m'));
    assert!(!app.rendered_active());
    press(&mut app, &keymap, KeyCode::Char('1'));
    assert!(!app.rendered_active(), "back on Changes, the file shows as source");
}

#[test]
fn markdown_view_rendered_opens_markdown_rendered_and_a_reread_never_flips_it() {
    let r = markdown_files_repo();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "markdown_view = \"rendered\"\n").unwrap();
    let rendered = herdr_reviewr::config::plugin_config_in(dir.path()).unwrap();

    // A fresh pane seeded from the config opens the first markdown file rendered.
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.seed_from_config(&rendered);
    app.reload().unwrap();
    assert!(app.rendered_active(), "markdown_view = rendered opens it rendered");

    // A reread of a config saying otherwise never flips the running pane.
    let source = herdr_reviewr::config::PluginConfig::default();
    app.set_plugin_config(source);
    app.reload().unwrap();
    assert!(app.rendered_active(), "a reread never flips a running pane");
}

#[test]
fn a_file_that_renders_nothing_never_takes_the_panes_choice() {
    let r = markdown_files_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.focus = Focus::Diff;
    let offers_m = |app: &App| app.footer_bands().iter().any(|&(a, _)| a == FooterAction::Rendered);
    press(&mut app, &keymap, KeyCode::Char('f'));
    press(&mut app, &keymap, KeyCode::Char('f'));
    assert_eq!(app.diff_path.as_deref(), Some("c.md"));
    assert!(!app.rendered_active());
    assert!(!offers_m(&app), "content that renders nothing offers no `m`, even under source");
    press(&mut app, &keymap, KeyCode::Char('m'));
    assert!(!app.rendered_active());
    press(&mut app, &keymap, KeyCode::Char('F'));
    press(&mut app, &keymap, KeyCode::Char('F'));
    assert_eq!(app.diff_path.as_deref(), Some("a.md"));
    assert!(!app.rendered_active(), "the `m` on c.md never flipped the pane");
    assert!(offers_m(&app));
}

/// `q` with unsent comments asks first (#119), across the event × state matrix.
#[test]
fn quitting_with_unsent_comments_asks_first() {
    let keymap = Keymap::default();
    let fresh = || {
        let r = edited_repo();
        let mut app = app_on(&r);
        app.focus = Focus::Diff;
        (r, app)
    };
    let commented = || {
        let (r, mut app) = fresh();
        app.diff_cursor = app.visible.iter().position(|r| r.marker() == '+').unwrap();
        app.start_comment();
        write_comment(&mut app, "keep me");
        (r, app)
    };
    let asking = || {
        let (r, mut app) = commented();
        press(&mut app, &keymap, KeyCode::Char('q'));
        (r, app)
    };

    // No comments: `q` quits at once, as it always has.
    let (_r, mut app) = fresh();
    press(&mut app, &keymap, KeyCode::Char('q'));
    assert!(app.should_quit);

    // Unsent comments: `q` asks instead, and the footer is the question.
    let (_r, mut app) = asking();
    assert!(!app.should_quit, "the first `q` never drops comments");
    assert!(app.confirming_quit);
    assert_eq!(
        app.footer_bands(),
        [
            (FooterAction::QuitDiscard, Band::Primary),
            (FooterAction::Cancel, Band::Do),
            (FooterAction::Send, Band::Do),
            (FooterAction::Copy, Band::Do),
        ]
    );

    // A repeated `q` never answers; `Q` quits.
    press(&mut app, &keymap, KeyCode::Char('q'));
    press(&mut app, &keymap, KeyCode::Char('q'));
    assert!(app.confirming_quit && !app.should_quit, "`q` never answers its own question");
    press(&mut app, &keymap, KeyCode::Char('Q'));
    assert!(app.should_quit);

    // `Q` outside the question does nothing.
    let (_r, mut app) = commented();
    press(&mut app, &keymap, KeyCode::Char('Q'));
    assert!(!app.should_quit && !app.confirming_quit);

    // `esc` stays, and so does any other key, which does nothing else.
    for code in [KeyCode::Esc, KeyCode::Char('j'), KeyCode::Char('c')] {
        let (_r, mut app) = asking();
        let cursor = app.diff_cursor;
        press(&mut app, &keymap, code);
        assert!(!app.confirming_quit, "{code:?} answers the question");
        assert!(!app.should_quit, "{code:?}");
        assert!(!app.composing(), "{code:?} only answers");
        assert_eq!(app.diff_cursor, cursor, "{code:?} only answers");
        assert_eq!(app.store.len(), 1, "{code:?}");
        // The answer is final: the next `q` asks again.
        press(&mut app, &keymap, KeyCode::Char('q'));
        assert!(!app.should_quit && app.confirming_quit, "{code:?}");
    }

    // `send` answers too (tests/send_flow.rs); `copy` would write the real clipboard.

    // A click or wheel answers and does nothing else; motion, drag, and release don't answer.
    let (_r, mut app) = asking();
    for kind in [
        MouseEventKind::Moved,
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        mouse(&mut app, &keymap, kind);
        assert!(app.confirming_quit, "{kind:?} leaves the question open");
    }
    for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::ScrollDown] {
        let (_r, mut app) = asking();
        let (cursor, scroll) = (app.diff_cursor, app.diff_scroll);
        mouse(&mut app, &keymap, kind);
        assert!(!app.confirming_quit && !app.should_quit, "{kind:?}");
        assert_eq!((app.diff_cursor, app.diff_scroll), (cursor, scroll), "{kind:?} only answers");
    }

    // A poll is no answer: the question stays open across it.
    let (r, mut app) = asking();
    r.write("a.rs", "alpha\nBETA\ngamma\ndelta\nepsilon\nzeta\n");
    app.reload().unwrap();
    assert!(app.confirming_quit && !app.should_quit);

    // The PR tab asks the same way, and takes the same answers.
    let (_r, mut app) = commented();
    press(&mut app, &keymap, KeyCode::Char('3'));
    press(&mut app, &keymap, KeyCode::Char('q'));
    assert!(app.confirming_quit && !app.should_quit);
    press(&mut app, &keymap, KeyCode::Char('Q'));
    assert!(app.should_quit);

    // Inside the comment list `q` stays inert, and in the composer it is text.
    let (_r, mut app) = commented();
    press(&mut app, &keymap, KeyCode::Char('l'));
    assert_eq!(app.mode, Mode::List);
    press(&mut app, &keymap, KeyCode::Char('q'));
    assert!(!app.confirming_quit && !app.should_quit);
}

// --- Diff sides from git ----------------------------------------------

/// `git diff`'s changed lines for `path`, a kept CR spelled as reviewr's marker.
fn git_changed_lines(r: &Repo, rev: &str, path: &str) -> Vec<String> {
    let out = r.git(&["diff", "--no-color", rev, "--", path]);
    out.split('\n')
        .skip_while(|l| !l.starts_with("@@"))
        .filter(|l| l.starts_with('-') || l.starts_with('+'))
        .map(|l| match l.strip_suffix('\r') {
            Some(body) => format!("{body}{}", herdr_reviewr::ui::CR_MARKER),
            None => l.to_string(),
        })
        .collect()
}

/// The open diff's change rows, marker-prefixed, a painted CR marker spelled after the text.
fn change_rows(app: &App) -> Vec<String> {
    let marker = |r: &Row| if r.cr_marker() { herdr_reviewr::ui::CR_MARKER } else { "" };
    app.diff
        .rows
        .iter()
        .filter(|r| r.marker() != ' ')
        .map(|r| format!("{}{}", r.marker_text(), marker(r)))
        .collect()
}

/// The repository's loose-object count, which any object write would raise.
fn loose_objects(r: &Repo) -> String {
    r.git(&["count-objects"]).split(' ').next().unwrap_or_default().to_string()
}

/// Whether reviewr's change rows say what `git diff` says.
fn rows_agree_with_git(rows: &[String], git: &[String]) -> bool {
    let cr = herdr_reviewr::ui::CR_MARKER;
    rows.len() == git.len()
        && rows.iter().zip(git).all(|(row, git)| row == git || format!("{row}{cr}") == *git)
}

#[test]
fn the_diff_agrees_with_git_diff_under_any_line_ending_rule() {
    // `core.autocrlf=true` per repo, as Git for Windows installs it.
    const BASE: &str = "one\ntwo\nthree\n";
    const CRLF: &str = "one\r\ntwo\r\nthree\r\n";
    const CRLF_EDIT: &str = "one\r\nTWO\r\nthree\r\n";
    const LF_EDIT: &str = "one\nTWO\nthree\n";
    const MIXED_EDIT: &str = "one\r\nTWO\nthree\r\n";
    let edit: &[&str] = &["-two", "+TWO"];
    let cases: &[(&str, &str, &str, &[&str])] = &[
        // (.gitattributes, the committed file, what the agent writes, the changed rows)
        ("", BASE, CRLF, &[]),
        ("", BASE, CRLF_EDIT, edit),
        ("", BASE, LF_EDIT, edit),
        ("", BASE, MIXED_EDIT, edit),
        ("* text eol=crlf\n", BASE, CRLF, &[]),
        ("* text eol=crlf\n", BASE, CRLF_EDIT, edit),
        ("* text eol=crlf\n", BASE, LF_EDIT, edit),
        ("* text eol=crlf\n", BASE, MIXED_EDIT, edit),
        // `-text`: git keeps every CR, so a changed ending is a change, and shows.
        ("* -text\n", BASE, CRLF, &["-one", "-two", "-three", "+one^M", "+two^M", "+three^M"]),
        ("* -text\n", BASE, LF_EDIT, edit),
        ("* -text\n", BASE, MIXED_EDIT, &["-one", "-two", "-three", "+one^M", "+TWO", "+three^M"]),
        // A lone CR in the committed blob (`i/-text`), gone from the rewrite: `auto` converts.
        (
            "",
            "one\rx\ntwo\nthree\n",
            "one x\r\nTWO\r\nthree\r\n",
            &["-one\rx", "-two", "+one x", "+TWO"],
        ),
        // A blob with CRLFs beside a lone CR is binary to git: every line changes.
        (
            "",
            "one\rx\r\ntwo\r\nthree\r\n",
            "one x\r\nTWO\r\nthree\r\n",
            &["-one\rx", "-two", "-three^M", "+one x", "+TWO", "+three"],
        ),
        // The same CR, still there: `auto` sees binary content and converts nothing.
        (
            "",
            "one\rx\ntwo\nthree\n",
            "one\rx\r\nTWO\r\nthree\r\n",
            &["-one\rx", "-two", "-three", "+one\rx^M", "+TWO", "+three^M"],
        ),
        // `ident` collapses the expanded keyword on the way in, so only the edit shows.
        ("* ident\n", "$Id$\none\n", "$Id: 0123 $\r\nONE\r\n", &["-one", "+ONE"]),
    ];
    for &(attributes, base, written, want) in cases {
        let case = format!("{attributes:?} {base:?} writing {written:?}");
        let r = Repo::init();
        r.git(&["config", "core.autocrlf", "true"]);
        r.write(".gitattributes", attributes);
        r.write("a.txt", base);
        r.commit_all("init");
        r.set_origin_default("main", "HEAD");
        let baseline = r.git(&["rev-parse", "HEAD^{tree}"]).trim().to_string();
        let objects = loose_objects(&r);
        r.write("a.txt", written);
        let git = git_changed_lines(&r, "HEAD", "a.txt");
        assert!(want.is_empty() == git.is_empty(), "git agrees: {case}: {git:?}");

        let mut app = app_on(&r);
        for scope in [Scope::Uncommitted, Scope::Branch] {
            app.set_scope(scope).unwrap();
            if want.is_empty() {
                // Unchanged to git: no row anywhere.
                assert!(changed_paths(&app).is_empty(), "{case} under {scope:?}");
            } else {
                app.select_file(0).unwrap();
                let rows = change_rows(&app);
                assert_eq!(rows, want, "{case} under {scope:?}");
                assert!(rows_agree_with_git(&rows, &git), "{case}: {rows:?} vs git {git:?}");
            }
        }

        // Every view reads the file as is, and no export carries a CR.
        enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
        app.select_file(file_row_of(&app, "a.txt").expect("a.txt listed")).unwrap();
        let lines: Vec<String> = app.visible.iter().map(Row::text).collect();
        let raw: Vec<&str> = written.split_inclusive('\n').collect();
        let bodies: Vec<&str> = raw.iter().map(|l| l.trim_end_matches(['\r', '\n'])).collect();
        assert_eq!(lines, bodies, "{case}: the file's lines, no ending CR");
        app.focus = Focus::Diff;
        app.diff_cursor = 1;
        app.start_comment();
        typed(&mut app, "why?");
        app.submit_comment();
        let all: Vec<&herdr_reviewr::model::Comment> = app.store.iter().collect();
        let export = herdr_reviewr::export::format_all(&all);
        assert!(export.contains(&lines[1]) && !export.contains('\r'), "{case}: {export:?}");

        app.open_search();
        let results = herdr_reviewr::search::SearchResults {
            files: vec![herdr_reviewr::search::FileHit { path: "a.txt".into(), spans: vec![] }],
            file_total: 1,
            ..Default::default()
        };
        let done = herdr_reviewr::search::SearchCompletion {
            generation: 1,
            outcome: herdr_reviewr::search::SearchOutcome::Ready(results),
        };
        herdr_reviewr::land_search_completion(&mut app, done, 1);
        app.build_search_preview();
        let preview = app.search.as_ref().and_then(|s| s.preview.as_ref()).expect("a preview");
        let previewed: Vec<String> = preview.diff.rows.iter().map(Row::text).collect();
        assert_eq!(previewed, lines, "{case}: the preview reads what All files reads");
        app.close_search();

        // No writes: reviewr asked git for the diff and stored nothing.
        assert_eq!(loose_objects(&r), objects, "{case}");

        // `last-turn` reads the baseline against its changeset's snapshot.
        enter_tab(&mut app, herdr_reviewr::app::Tab::Changes);
        app.sync_turn_baseline(Some(baseline));
        app.set_scope(Scope::LastTurn).unwrap();
        if want.is_empty() {
            assert!(changed_paths(&app).is_empty(), "{case} under last-turn");
        } else {
            app.select_file(0).unwrap();
            assert_eq!(change_rows(&app), want, "{case} under last-turn");
        }
    }
}

#[test]
fn an_untracked_or_renamed_crlf_file_reads_like_git() {
    // The two paths the line-ending table leaves out: a new file, and a rename rewritten in CRLF.
    let r = Repo::init();
    r.git(&["config", "core.autocrlf", "true"]);
    r.write("old.txt", "one\ntwo\n");
    r.commit_all("init");
    r.git(&["mv", "old.txt", "moved.txt"]);
    r.write("moved.txt", "one\r\ntwo\r\n");
    r.write("new.txt", "a\r\nb\r\n");

    let mut app = app_on(&r);
    app.select_file(file_row_of(&app, "new.txt").expect("new.txt listed")).unwrap();
    assert_eq!(change_rows(&app), ["+a", "+b"], "an added CRLF line is no CR change");
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    app.start_comment();
    typed(&mut app, "why?");
    app.submit_comment();
    let all: Vec<&herdr_reviewr::model::Comment> = app.store.iter().collect();
    assert!(!herdr_reviewr::export::format_all(&all).contains('\r'));
    app.select_file(file_row_of(&app, "moved.txt").expect("moved.txt listed")).unwrap();
    assert!(change_rows(&app).is_empty(), "{:?}", change_rows(&app));
}

#[test]
fn a_last_turn_diff_of_an_untracked_file_shows_the_turns_edit() {
    // A file only in the turn's snapshots is one edited line, never deleted.
    let r = Repo::init();
    r.git(&["config", "core.autocrlf", "true"]);
    r.write("a.txt", "a\n");
    r.commit_all("init");
    r.write("notes.txt", "one\ntwo\n");
    let baseline = herdr_reviewr::git::snapshot_worktree(r.path()).unwrap();
    r.write("notes.txt", "one\r\nTWO\r\n");

    let mut app = app_on(&r);
    app.sync_turn_baseline(Some(baseline));
    app.set_scope(Scope::LastTurn).unwrap();
    assert_eq!(changed_paths(&app), ["notes.txt"]);
    app.select_file(0).unwrap();
    assert_eq!(change_rows(&app), ["-two", "+TWO"]);
}

#[test]
fn a_file_diff_reads_the_trees_its_counts_came_from() {
    // A file opened before the new changeset lands diffs the landed trees, never a mixed pair.
    let r = Repo::init();
    r.write("a.txt", "one\n");
    r.write("b.txt", "one\n");
    r.commit_all("init");
    let first = herdr_reviewr::git::snapshot_worktree(r.path()).unwrap();
    r.write("a.txt", "two\n");
    r.write("b.txt", "two\n");
    let mut app = app_on(&r);
    app.sync_turn_baseline(Some(first));
    app.set_scope(Scope::LastTurn).unwrap();
    assert_eq!(changed_paths(&app), ["a.txt", "b.txt"]);

    r.write("b.txt", "three\n");
    let second = herdr_reviewr::git::snapshot_worktree(r.path()).unwrap();
    app.sync_turn_baseline(Some(second));
    app.select_file(1).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("b.txt"));
    assert_eq!(change_rows(&app), ["-one", "+two"]);
}

#[test]
fn a_cr_marker_is_paint_never_text() {
    // `-text`: git keeps the CR, so the line gained an ending, painted `alpha^M`.
    let r = Repo::init();
    r.write(".gitattributes", "* -text\n");
    r.write("e.txt", "alpha\nbeta\n");
    r.commit_all("init");
    r.write("e.txt", "alpha\r\nbeta\n");
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    assert_eq!(change_rows(&app), ["-alpha", "+alpha^M"]);

    // Find: no line holds the marker.
    open_find(&mut app, &keymap);
    find_type(&mut app, &keymap, "^M");
    assert_eq!(app.find_count(), Some((None, 0)));
    press(&mut app, &keymap, KeyCode::Esc);

    // Copy: a drag over the whole painted line, marker included, copies the line.
    let (c0, r0) = sel_cell(&app, 1, 0);
    let (c1, r1) = sel_cell(&app, 1, 6);
    sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), c0, r0);
    sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), c1, r1);
    sel_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), c1, r1);
    assert_eq!(last_copy().as_deref(), Some("alpha"));

    // Snippet: the comment carries the line as the file has it.
    app.focus = Focus::Diff;
    app.diff_cursor = 1;
    app.start_comment();
    typed(&mut app, "why?");
    app.submit_comment();
    assert_eq!(app.store.iter().next().map(|c| c.lines.as_str()), Some("+alpha"));
}

/// A drag over a find match covers the match's block: its text becomes body text on the
/// selection and reads there in every theme, dark or light.
#[test]
fn a_selected_find_match_reads_on_the_selection() {
    use herdr_reviewr::roles::{Fill, contrast};
    for theme in ["catppuccin", "dracula", "tokyo-night-day", "github-light"] {
        let repo = selection_repo();
        let mut app = app_on(&repo);
        app.set_cli_theme(Some(theme.to_string()));
        app.open_find();
        for ch in "beta".chars() {
            app.input_push(ch);
        }
        let (from_col, from_row) = sel_cell(&app, 0, 4);
        let (to_col, to_row) = sel_cell(&app, 0, 10);
        sel_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), from_col, from_row);
        sel_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), to_col, to_row);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            SEL_AREA.width,
            SEL_AREA.height,
        ))
        .unwrap();
        terminal.draw(|f| herdr_reviewr::ui::render(f, &app)).unwrap();
        let (x, y) = sel_cell(&app, 0, 7);
        let cell = terminal.backend().buffer().cell((x, y)).unwrap().clone();
        let p = app.palette();
        assert_eq!(cell.symbol(), "e", "{theme}: inside `beta`");
        assert_eq!(cell.bg, p.fill(Fill::Selection), "{theme}: the selection covers the match");
        let ratio = contrast(cell.fg, cell.bg);
        assert!(ratio >= 4.5, "{theme}: selected match text {ratio:.2} < 4.5");
    }
}

/// One frame's scroll settle through the frame loop's own step, one display row per logical row.
fn settle_frame(app: &mut App, viewport: usize) {
    let heights = vec![1usize; app.visible.len()];
    app.settle_diff_scroll(&heights, viewport);
}

/// `:N` lands on the new side's line N: a fold opens, a deleted line N is no target, past the end
/// is the last row, and a pick drops.
#[test]
fn goto_line_lands_on_the_new_sides_line_in_the_changes_diff() {
    use herdr_reviewr::diff::Row;
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut base = String::from("total = 0\n");
    for i in 0..10 {
        writeln!(base, "filler{i}").unwrap();
    }
    base.push_str("last = 1\ngone\n");
    r.write("m.rs", &base);
    r.commit_all("init");
    r.write("m.rs", &base.replace("last = 1\ngone\n", "last = total\n"));
    let mut app = app_on(&r);
    let new_no = |app: &App| app.visible[app.diff_cursor].new_no();

    assert_eq!(app.line_count(), 12, "the new side's lines, the folded head among them");

    // Line 1 hides in the leading fold: the jump opens it and lands there.
    assert!(app.visible.iter().any(|row| matches!(row, Row::Fold { .. })));
    let before = app.visible.len();
    app.goto_line(1);
    assert!(app.visible.len() > before, "the fold opened");
    assert_eq!(new_no(&app), Some(1));
    assert!(app.visible[app.diff_cursor].text().contains("total = 0"));
    assert_eq!(app.focus, Focus::Diff);

    // Old line 12 (`last = 1`) was deleted; line 12 means the new side's `last = total`.
    app.goto_line(12);
    assert_eq!(new_no(&app), Some(12));
    assert!(app.visible[app.diff_cursor].text().contains("last = total"));

    // Past the end lands on the last row.
    app.goto_line(999);
    assert_eq!(app.diff_cursor, app.visible.len() - 1);

    // A jump drops a line-range pick; `0` is the first line, as in vim.
    app.diff_cursor = 0;
    app.toggle_select();
    app.goto_line(5);
    assert_eq!(app.select_anchor, None, "a jump is navigation, not a pick extend");
    app.goto_line(0);
    assert_eq!(new_no(&app), Some(1));
}

/// A file with no new-side lines numbers by its old side, its count too.
#[test]
fn goto_line_in_a_deleted_file_lands_on_the_old_line() {
    let r = Repo::init();
    r.write("gone.rs", "one\ntwo\nthree\n");
    r.commit_all("init");
    std::fs::remove_file(r.path_buf().join("gone.rs")).unwrap();
    let mut app = app_on(&r);
    assert_eq!(app.line_count(), 3);
    app.goto_line(2);
    assert_eq!(app.visible[app.diff_cursor].old_no(), Some(2));
    assert!(app.visible[app.diff_cursor].text().contains("two"));
}

/// All files lands on line N, centered when it was off screen, and leaves the scroll when it
/// is already on screen.
#[test]
fn goto_line_centers_an_off_screen_line_and_keeps_an_on_screen_one() {
    use herdr_reviewr::app::Tab;
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut text = String::new();
    for i in 1..=100 {
        writeln!(text, "line {i}").unwrap();
    }
    r.write("long.rs", &text);
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let row = app
        .file_rows
        .iter()
        .position(|f| f.file_index().is_some_and(|i| app.entries[i].path == "long.rs"))
        .unwrap();
    app.select_file(row).unwrap();
    settle_frame(&mut app, 10);
    assert_eq!(app.diff_scroll, 0);

    app.goto_line(50);
    assert!(app.visible[app.diff_cursor].text().contains("line 50"));
    settle_frame(&mut app, 10);
    assert_eq!(app.diff_scroll, 45, "line 50 sits mid-pane: 4 rows above it, 5 below");

    app.goto_line(52);
    settle_frame(&mut app, 10);
    assert_eq!(app.diff_scroll, 45, "an on-screen line leaves the view still");

    // Up the file too: line 20 sits mid-pane above the old view.
    app.goto_line(20);
    settle_frame(&mut app, 10);
    assert_eq!(app.diff_scroll, 15);

    // Heights, not rows: a 3-row wrapped line above the target fills its share of the half.
    let mut heights = vec![1usize; app.visible.len()];
    heights[47] = 3;
    (app.diff_cursor, app.diff_scroll) = (49, 0);
    app.center_diff_cursor(&heights, 10);
    assert_eq!(app.diff_scroll, 47, "line 49's 1 row and line 48's 3 fill the 4 above");

    // A target taller than the pane starts at the top.
    heights[49] = 15;
    (app.diff_cursor, app.diff_scroll) = (49, 0);
    app.center_diff_cursor(&heights, 10);
    assert_eq!(app.diff_scroll, 49);
}

/// Rendered markdown lands on the block holding the line; a line inside a collapsed `<details>`
/// opens it.
#[test]
fn goto_line_in_rendered_markdown_lands_on_its_block_and_opens_details() {
    let r = Repo::init();
    let doc = "# Title\n\nfirst paragraph\nstill first\n\n<details>\n<summary>More</summary>\n\nhidden line\n\n</details>\n\nlast paragraph\n";
    r.write("doc.md", doc);
    r.commit_all("init");
    r.write("doc.md", &doc.replace("last paragraph", "last paragraph, edited"));
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active());
    let text = |app: &App| app.visible[app.diff_cursor].text();

    // Line 4 sits in the first paragraph's block.
    app.goto_line(4);
    assert!(text(&app).contains("first paragraph"), "{}", text(&app));

    // Line 9 hides in the collapsed `<details>`: it opens and the cursor lands on the line.
    assert!(!app.visible.iter().any(|row| row.text().contains("hidden line")), "collapsed");
    app.goto_line(9);
    assert!(text(&app).contains("hidden line"), "{}", text(&app));

    // The blank line 5 between blocks belongs to the next block; past the end, the last block.
    app.goto_line(5);
    assert!(text(&app).contains("More"), "{}", text(&app));
    app.goto_line(999);
    assert!(text(&app).contains("last paragraph"), "{}", text(&app));
    assert_eq!(app.line_count(), 13, "the source's lines");
}

/// Past the end means the file's last line, so a last line inside a collapsed `<details>` opens
/// it like any line that hides there.
#[test]
fn goto_line_past_the_end_opens_a_last_details() {
    let r = Repo::init();
    let doc =
        "# Title\n\nintro\n\n<details>\n<summary>More</summary>\n\nhidden line\n\n</details>\n";
    r.write("doc.md", doc);
    r.commit_all("init");
    r.write("doc.md", &doc.replace("intro", "intro, edited"));
    let mut app = app_on_rendered(&r);
    assert!(app.rendered_active());
    assert!(!app.visible.iter().any(|row| row.text().contains("hidden line")), "collapsed");
    for line in [u32::MAX, 999] {
        app.goto_line(line);
        let text = app.visible[app.diff_cursor].text();
        assert!(text.contains("hidden line"), "{line}: {text}");
    }
}

/// A line inside nested collapsed `<details>` opens every one that hides it; the same jump
/// works rendered in All files.
#[test]
fn goto_line_opens_nested_details_in_all_files() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    let doc = "# Doc\n\n<details>\n<summary>Outer</summary>\n\n<details>\n<summary>Inner</summary>\n\ndeep line\n\n</details>\n\n</details>\n";
    r.write("doc.md", doc);
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let row = app
        .file_rows
        .iter()
        .position(|f| f.file_index().is_some_and(|i| app.entries[i].path == "doc.md"))
        .unwrap();
    app.select_file(row).unwrap();
    assert!(app.rendered_active());
    assert!(!app.visible.iter().any(|row| row.text().contains("deep line")), "collapsed");
    app.goto_line(9);
    assert!(app.visible[app.diff_cursor].text().contains("deep line"));
}

/// A repo with one changed 30-line file, `src/m.rs`: a fold hides its head, a change at the end.
fn line_field_repo() -> Repo {
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut text = String::new();
    for i in 1..=30 {
        writeln!(text, "line {i}").unwrap();
    }
    r.write("src/m.rs", &text);
    r.write("src/n.rs", "one\n");
    r.commit_all("init");
    r.write("src/m.rs", &text.replace("line 30", "line 30 edited"));
    r.write("src/n.rs", "one\ntwo\n");
    r
}

/// The line field's event table: open, type, edit, jump, close, `:0`/`:$`, and what a refresh,
/// a tab switch, and an invalid config do to it.
#[test]
fn the_line_field_follows_its_event_table() {
    use herdr_reviewr::app::Tab;
    let r = line_field_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let place = |app: &App| (app.diff_cursor, app.diff_scroll, app.visible.len());
    let colon = |app: &mut App| press(app, &keymap, KeyCode::Char(':'));

    // Opening drops a pick and focuses the read pane.
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.len() - 1;
    app.toggle_select();
    app.focus = Focus::Files;
    colon(&mut app);
    assert!(app.line_open());
    assert_eq!((app.select_anchor, app.focus), (None, Focus::Diff));

    // Typing moves nothing; letters are inert; `2` is a digit, not the All files tab.
    let before = place(&app);
    find_type(&mut app, &keymap, "2x5");
    press(&mut app, &keymap, KeyCode::Backspace);
    find_type(&mut app, &keymap, "6");
    assert_eq!(app.find.as_ref().unwrap().query, "26");
    assert_eq!(app.tab, Tab::Changes, "a digit never switches tab while the field is open");
    assert_eq!(place(&app), before, "nothing moves before enter");

    // A refresh of the same file, its content changed, keeps the field and its number.
    r.write(
        "src/m.rs",
        &std::fs::read_to_string(r.path_buf().join("src/m.rs"))
            .unwrap()
            .replace("line 2\n", "line 2 edited\n"),
    );
    common::land_world(&mut app);
    assert!(app.line_open());
    assert_eq!(app.find.as_ref().unwrap().query, "26");

    // Enter jumps and closes: line 26 hid in the fold, which opened.
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.visible[app.diff_cursor].new_no(), Some(26));

    // Esc and an empty Enter close with nothing moved.
    for keys in [&[KeyCode::Esc][..], &[KeyCode::Enter]] {
        colon(&mut app);
        let before = place(&app);
        for &k in keys {
            press(&mut app, &keymap, k);
        }
        assert_eq!(app.mode, Mode::Normal, "{keys:?} closes");
        assert_eq!(place(&app), before, "{keys:?} moves nothing");
    }

    // `:0` is the first line and `:$` the last, as in vim; a digit after `$` starts a number.
    let new_no = |app: &App| app.visible[app.diff_cursor].new_no();
    for (typed, line) in [("0", 1), ("$", 30), ("$7", 7)] {
        colon(&mut app);
        find_type(&mut app, &keymap, typed);
        press(&mut app, &keymap, KeyCode::Enter);
        assert_eq!(new_no(&app), Some(line), ":{typed}");
    }

    // A tab switch and an invalid config close it.
    colon(&mut app);
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(!app.line_open(), "a tab switch closes the field");
    enter_tab(&mut app, Tab::Changes);
    colon(&mut app);
    app.set_config_error("bad config".into());
    assert!(!app.line_open(), "an invalid config closes the field");
}

/// `:` works only where a read cursor has a line to land on: the PR tab and an open find or
/// composer take it as text, and the footer offers it only in a file tab with content.
#[test]
fn the_line_field_is_silent_where_it_cannot_work() {
    use herdr_reviewr::app::{FooterAction, Tab};
    let r = line_field_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let offered = |app: &App| app.footer_bands().iter().any(|&(a, _)| a == FooterAction::GotoLine);
    assert!(offered(&app), "a file tab with content offers `:`");

    open_find(&mut app, &keymap);
    press(&mut app, &keymap, KeyCode::Char(':'));
    assert!(!app.line_open());
    assert_eq!(app.find.as_ref().unwrap().query, ":", "find takes `:` as text");
    press(&mut app, &keymap, KeyCode::Esc);

    enter_tab(&mut app, Tab::Pr);
    press(&mut app, &keymap, KeyCode::Char(':'));
    assert!(!app.line_open(), "the PR tab has no line to land on");
    assert!(!offered(&app));
}

/// A paste into the line field is a number or `$`, wrapping punctuation dropped; anything else
/// leaves the field as it was.
#[test]
fn the_line_field_takes_a_pasted_number_only() {
    let r = line_field_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let query = |app: &App| app.find.as_ref().unwrap().query.clone();
    press(&mut app, &keymap, KeyCode::Char(':'));
    for (pasted, want) in [("42", "42"), ("`1337`\n", "1337"), (" 21, ", "21"), ("$", "$")] {
        app.input_paste(pasted);
        assert_eq!(query(&app), want, "{pasted:?}");
        let caret = app.find.as_ref().unwrap().caret;
        assert_eq!(caret, want.chars().count(), "{pasted:?}: the caret ends the number");
    }
    app.input_paste("7");
    for pasted in ["src/m.rs:12", "see line 21", "1,337", "v2", "src/n.rs#L5", "-1", "+3", ""] {
        app.input_paste(pasted);
        assert_eq!(query(&app), "7", "{pasted:?} leaves the field");
    }
}

/// Past the end lands on the new side's last line even when the file's tail was deleted, and a
/// number too long for any file is past the end, not nothing.
#[test]
fn the_line_field_lands_past_the_end_on_the_last_new_line() {
    let r = Repo::init();
    r.write("t.rs", "a\nb\nc\nd\n");
    r.commit_all("init");
    r.write("t.rs", "a\nb\n");
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.goto_line(99);
    assert_eq!(app.visible[app.diff_cursor].new_no(), Some(2), "not the deleted tail");

    app.diff_cursor = 0;
    press(&mut app, &keymap, KeyCode::Char(':'));
    find_type(&mut app, &keymap, "99999999999");
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.visible[app.diff_cursor].new_no(), Some(2));
}

/// The line field holds digits or `$`: an alt chord types nothing, `$` replaces the number
/// wherever the caret is, and the footer offers `enter go` only for a line Enter would jump to.
#[test]
fn the_line_field_holds_digits_and_offers_enter_only_for_a_line() {
    use herdr_reviewr::app::FooterAction;
    let r = line_field_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let go = |app: &App| app.footer_bands().iter().any(|&(a, _)| a == FooterAction::LineGo);
    press(&mut app, &keymap, KeyCode::Char(':'));
    find_type(&mut app, &keymap, "12");
    let alt_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
    handle_key(&mut app, alt_x, Rect::new(0, 0, 120, 40), &keymap).unwrap();
    assert_eq!(app.find.as_ref().unwrap().query, "12", "an alt chord types nothing");
    assert!(go(&app));
    for _ in 0..2 {
        press(&mut app, &keymap, KeyCode::Backspace);
    }
    assert!(!go(&app), "an empty field closes on Enter, so the footer offers only esc");
    find_type(&mut app, &keymap, "$");
    assert!(go(&app), "`$` goes to the last line");
    find_type(&mut app, &keymap, "12");
    press(&mut app, &keymap, KeyCode::Left);
    find_type(&mut app, &keymap, "$");
    assert_eq!(app.find.as_ref().unwrap().query, "$", "`$` stands alone, caret anywhere");
}

/// The field is typed for the file and tab it opened over: opening another file, or any tab,
/// closes it, by key or by click; the PR tab closes find too, which it cannot draw.
#[test]
fn the_line_field_closes_when_its_file_or_tab_changes() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("m.rs", "one\n");
    r.write("n.rs", "one\n");
    r.commit_all("init");
    r.write("m.rs", "one\ntwo\n");
    r.write("n.rs", "one\ntwo\n");
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let row_of = |app: &App, path: &str| {
        app.file_rows
            .iter()
            .position(|f| f.file_index().is_some_and(|i| app.entries[i].path == path))
            .unwrap()
    };

    press(&mut app, &keymap, KeyCode::Char(':'));
    find_type(&mut app, &keymap, "2");
    let other = row_of(&app, "n.rs");
    app.select_file(other).unwrap();
    assert!(!app.line_open(), "another file closes the field");

    // All files with a file already open there: the tab switch closes it too.
    enter_tab(&mut app, Tab::AllFiles);
    let m = row_of(&app, "m.rs");
    app.select_file(m).unwrap();
    enter_tab(&mut app, Tab::Changes);
    press(&mut app, &keymap, KeyCode::Char(':'));
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(!app.line_open(), "a tab switch closes the field");

    // The PR tab draws no band: find closes there too, so no key falls into a hidden field.
    enter_tab(&mut app, Tab::Changes);
    open_find(&mut app, &keymap);
    app.set_tab(Tab::Pr).unwrap();
    assert_eq!(app.mode, Mode::Normal, "the PR tab closes find");
}

/// A general draft, a published thread with a draft reply, and a published comment.
fn drafts_snapshot() -> herdr_reviewr::forge::PrView {
    use herdr_reviewr::forge::{Comment, PrSnapshot, PrView, Reply};
    PrView::Pr(Box::new(PrSnapshot {
        number: 1497,
        comments: vec![
            Comment {
                author: "you".into(),
                body: "Split the commit.".into(),
                created_at: String::new(),
                draft_id: Some(684_068),
                ..common::comment()
            },
            Comment {
                body: "Published root.".into(),
                replies: vec![Reply {
                    author: "you".into(),
                    author_is_bot: false,
                    body: "Agreed.".into(),
                    created_at: String::new(),
                    draft_id: Some(684_070),
                }],
                ..common::comment()
            },
            Comment { body: "Published only.".into(), ..common::comment() },
        ],
        ..common::pr_snapshot()
    }))
}

#[test]
fn a_rework_note_on_a_draft_queues_and_sends_to_the_agent() {
    use herdr_reviewr::app::Tab;
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.set_tab(Tab::Pr).unwrap();
    app.pr_forge = herdr_reviewr::git::Forge::GitLab;
    app.pr = drafts_snapshot();

    let acts: Vec<_> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
    assert!(acts.contains(&FooterAction::Rework), "a draft row offers the rework key");
    press(&mut app, &keymap, KeyCode::Char('c'));
    assert!(matches!(app.mode, Mode::Composing { editing: None }));
    assert_eq!(app.pending_location().as_deref(), Some("MR !1497 draft note 684068"));
    typed(&mut app, "one sentence");
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.store.len(), 1);

    // `c` again edits the queued note instead of adding a second one.
    press(&mut app, &keymap, KeyCode::Char('c'));
    assert!(matches!(app.mode, Mode::Composing { editing: Some(0) }));
    assert_eq!(app.input, "one sentence");
    press(&mut app, &keymap, KeyCode::Esc);

    // The thread row targets its draft reply.
    app.pr_move(1);
    press(&mut app, &keymap, KeyCode::Char('c'));
    typed(&mut app, "add the reason");
    press(&mut app, &keymap, KeyCode::Enter);
    assert_eq!(app.store.len(), 2);

    // A refresh keeps the queued notes.
    app.pr = drafts_snapshot();
    assert_eq!(app.store.len(), 2, "comments survive a PR refresh");

    let target = FakeTarget::ok();
    assert!(app.export(&target));
    let sent = target.last();
    assert!(sent.contains("MR !1497 draft note 684068\n> Split the commit.\n"), "{sent}");
    assert!(sent.contains("Reword this draft note. Do not post it. Suggestion: one sentence"));
    assert!(sent.contains("MR !1497 draft reply 684070\n> Agreed.\n"), "{sent}");
    assert!(app.store.is_empty());
}

#[test]
fn a_published_row_takes_no_rework_note() {
    use herdr_reviewr::app::Tab;
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    app.set_tab(Tab::Pr).unwrap();
    app.pr_forge = herdr_reviewr::git::Forge::GitLab;
    app.pr = drafts_snapshot();
    app.pr_move(2);
    let acts: Vec<_> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
    assert!(!acts.contains(&FooterAction::Rework), "a published row offers no rework key");
    press(&mut app, &keymap, KeyCode::Char('c'));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.status, "only a draft takes a rework note");
    assert!(app.store.is_empty());
}
