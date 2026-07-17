//! End-to-end tests of the review loop: `App` driven against real repos, with a
//! fake export target so consume-on-success is checked without a live agent.

mod common;

use std::cell::RefCell;

use anyhow::{Result, bail};
use common::{Repo, app_on, typed};
use herdr_reviewr::app::{App, Focus, FooterAction, Mode, Tier};
use herdr_reviewr::config::NavigatorPosition;
use herdr_reviewr::export::ExportTarget;
use herdr_reviewr::keymap::{Action, Keymap};
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

/// Settle the diff scroll with one display row per logical row (no wrap), for tests that
/// drive short-line diffs — reveal the cursor, then bound the offset, as the loop does.
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

    // Wheel-scrolling moves the viewport only: the selection and the open diff stay put,
    // so browsing the list never reloads a diff (the performance fix). It may leave the
    // cursor off screen — it is not yanked back.
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
    // 30 logical rows, each 3 display lines tall, in a 20-display-row viewport. A row-count
    // cap would stop the scroll at 30-20=10, hiding the last ~13 rows; the height-aware cap
    // must reach the offset that shows the last row.
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
    let r = Repo::init();
    r.write("src/a.rs", "x\n");
    r.write("src/b.rs", "y\n");
    r.commit_all("init");
    r.write("src/a.rs", "x2\n");
    r.write("src/b.rs", "y2\n");
    let mut app = app_on(&r);
    app.focus = Focus::Files;
    let dir = app.file_rows.iter().position(|row| row.dir_path() == Some("src")).unwrap();
    app.file_cursor = dir;
    app.reveal_files = false;
    app.collapse_dir();
    assert!(app.reveal_files, "collapsing a directory requests a reveal (even at the same index)");
}

/// A changeset for the traversal keys: `a.rs` with two hunks, a changed binary file with none,
/// and `c.rs` with one. Each change is a pure insertion, so a hunk's first changed row carries
/// the inserted text and the assertions below read as the reviewer's own path through the diff.
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
    app.visible[app.diff_cursor].text()
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

    // Arm the crossing at a.rs's last hunk: the footer leads with the offer, the one movement
    // key it ever names, and the comment key stays on the bar because it still works here.
    press(&mut app, &keymap, KeyCode::Char(']'));
    assert_eq!(app.armed_cross(), Some(true));
    let bar = app.footer_actions();
    assert_eq!(bar.first(), Some(&(FooterAction::CrossFile { forward: true }, Tier::Primary)));
    assert!(bar.iter().any(|&(a, t)| a == FooterAction::Comment && t == Tier::Normal));

    // Any other key drops the arm and still does its own work — here `j` moves the cursor.
    let cursor = app.diff_cursor;
    press(&mut app, &keymap, KeyCode::Char('j'));
    assert_eq!(app.armed_cross(), None, "another key disarms");
    assert_eq!(app.diff_cursor, cursor + 1, "and still moves the cursor");
    assert!(!app.footer_actions().iter().any(|(a, _)| matches!(a, FooterAction::CrossFile { .. })));

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

    // Mouse capture reports every pointer move over the pane. A pointer resting on the sidebar
    // is not an input the reviewer made, so it must not drop the crossing they armed.
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

    // Directories sort first, so the tree is [src/, src/b.rs, src/c.rs, a.rs] and the initial
    // cursor lands on the first *file* row.
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

    // Park the list cursor on the directory row above the open file — moving onto a directory
    // keeps the open diff, so the two can diverge.
    app.file_cursor = app.file_rows.iter().position(|row| row.dir_path() == Some("src")).unwrap();

    // Both traversals step from the open file. Stepping from the parked cursor would find
    // `src/a.rs` again — the open file — and wrap the diff back to its first hunk.
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
    app.set_tab(herdr_reviewr::app::Tab::AllFiles).unwrap();
    let (path, cursor) = (app.diff_path.clone(), app.diff_cursor);
    app.next_hunk();
    assert_eq!((app.diff_path.clone(), app.diff_cursor), (path, cursor));

    // The file skips still work there.
    app.next_file();
    assert_ne!(app.diff_path.as_deref(), Some("a.rs"));
}

#[test]
fn a_file_skip_out_of_a_preview_opens_the_next_file_in_source() {
    let r = Repo::init();
    r.write("a.md", "# title\n");
    r.write("b.rs", "x\n");
    r.commit_all("init");
    r.write("a.md", "# title\n\nbody\n");
    r.write("b.rs", "x2\n");
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    assert_eq!(app.diff_path.as_deref(), Some("a.md"));
    app.toggle_preview();
    assert!(app.preview_active(), "the markdown preview is open");

    // The preview has no cursor, so a hunk step has no target there.
    app.next_hunk();
    assert_eq!(app.diff_path.as_deref(), Some("a.md"));
    assert!(app.preview_active());

    app.next_file();
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"));
    assert!(!app.preview_active(), "the opened file starts in source");
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

    // A poll reloads the same unchanged content. It must request no reveal, so the next
    // frame leaves both wheel scrolls where they are (the regression snapped them to the top).
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

    // Edit the comment from the list: the diff must switch to foo and land on its line,
    // even though foo has no visible row (the A2 bug opened the box over root.rs).
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
        !app.footer_actions().iter().any(|&(a, _)| a == FooterAction::NavigatorPosition),
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
    app.footer_actions().first().expect("a footer action").0
}

#[test]
fn the_footer_offers_the_action_for_what_the_cursor_is_on() {
    let mut app = composing_app(); // diff focus, on a changed line, composer open
    app.cancel_comment(); // back to Normal, still on the changed line
    assert_eq!(primary(&app), FooterAction::Comment, "a diff line offers comment");

    app.toggle_select();
    assert_eq!(primary(&app), FooterAction::Comment, "a live selection still leads with comment");
    assert!(
        app.footer_actions().iter().any(|&(a, _)| a == FooterAction::ClearSelection),
        "and offers to clear the selection"
    );
    app.toggle_select();

    comment_on(&mut app, '+', "note");
    // The cursor now sits on the line it just commented.
    assert_eq!(primary(&app), FooterAction::EditComment, "a commented line offers edit");
    assert!(
        app.footer_actions().iter().any(|&(a, _)| a == FooterAction::Send),
        "a written comment surfaces send wherever the cursor is"
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
    let has_scope = |a: &App| a.footer_actions().iter().any(|&(x, _)| x == FooterAction::Scope);
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
        !app.footer_actions().iter().any(|&(a, _)| a == FooterAction::OpenPr),
        "no resolved PR → no open action"
    );

    // A resolved PR with zero comments still offers `o open` — `o` opens the PR URL, not a comment.
    app.pr = PrView::Pr(Box::new(PrSnapshot { number: 7, ..common::pr_snapshot() }));
    assert!(app.pr_selected_comment().is_none(), "zero comments → nothing selected");
    assert_eq!(
        app.footer_actions().first().map(|&(a, _)| a),
        Some(FooterAction::OpenPr),
        "a resolved PR offers open even with no comments"
    );
}

#[test]
fn the_footer_offers_send_only_once_a_comment_exists() {
    let mut app = composing_app();
    app.cancel_comment();
    assert!(
        !app.footer_actions().iter().any(|&(a, _)| a == FooterAction::Send),
        "no comments yet → no send action"
    );
    comment_on(&mut app, '+', "note");
    assert!(
        app.footer_actions().iter().any(|&(a, _)| a == FooterAction::Send),
        "a comment written → send appears"
    );
}

/// A repo whose `big.rs` has 40 lines with one change in the middle, so the head and
/// tail unchanged runs fold.
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
    assert!(app.commented_lines().iter().any(|&i| app.visible[i].text().contains("LINE 20")));
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

    // Mirror the event loop: reserve the box's rows, then clamp. The anchored line must
    // stay within the narrowed viewport so it renders above the box.
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

#[test]
fn arrows_collapse_and_expand_a_folder() {
    let r = Repo::init();
    r.write("src/a.rs", "x\n");
    r.write("src/b.rs", "y\n");
    r.commit_all("init");
    r.write("src/a.rs", "x2\n");
    r.write("src/b.rs", "y2\n");
    let mut app = app_on(&r);
    app.focus = Focus::Files;

    let dir_row = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    app.file_cursor = dir_row;
    assert!(app.on_folder(), "the cursor is on the folder");
    let expanded = app.file_rows.len();

    app.collapse_dir(); // ←
    assert!(app.file_rows.len() < expanded, "collapsing hides the children");
    assert!(app.on_folder(), "the cursor stays on the folder row");

    app.expand_dir(); // →
    assert_eq!(app.file_rows.len(), expanded, "expanding shows them again");
}

#[test]
fn divider_drag_math_and_keyboard_clamps_follow_all_four_positions() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let area = Rect::new(0, 0, 100, 102); // a 100-cell split axis in either direction
    let body = herdr_reviewr::ui::body_rect(area);
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
        )
        .unwrap();
        handle_mouse(
            &mut app,
            event(MouseEventKind::Drag(MouseButton::Left), target_column, target_row),
            area,
            &heights,
            &keymap,
        )
        .unwrap();
        handle_mouse(
            &mut app,
            event(MouseEventKind::Up(MouseButton::Left), target_column, target_row),
            area,
            &heights,
            &keymap,
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
        !app.footer_actions().iter().any(|&(a, _)| a == FooterAction::NavigatorPosition),
        "the comments list owns its footer"
    );
    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.navigator_position, NavigatorPosition::Right, "the action is inert in list");
    app.mode = Mode::Normal;
    app.set_tab(herdr_reviewr::app::Tab::Pr).unwrap();
    press(&mut app, &keymap, KeyCode::Char('p'));
    assert_eq!(app.navigator_position, NavigatorPosition::Bottom, "the action works on PR");
    assert!(
        app.footer_actions().iter().any(|&(a, _)| a == FooterAction::NavigatorPosition),
        "the PR footer exposes the position action"
    );
}

#[test]
fn divider_drag_cancels_until_mouse_up() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 120, 40);
    let body = herdr_reviewr::ui::body_rect(area);
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
    )
    .unwrap();
    handle_mouse(
        &mut app,
        event(MouseEventKind::Drag(MouseButton::Left), 70, row),
        area,
        &heights,
        &keymap,
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

    // Editing the comment from the list pulls the view back to its file and lands the
    // cursor on a real diff line there (so the inline box opens over the comment).
    app.open_list();
    app.start_edit();
    assert!(app.composing());
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"), "edit switched to the comment's file");
    let dl = &app.diff.rows[app.diff_cursor];
    assert!(dl.new_no().is_some() || dl.old_no().is_some(), "cursor sits on a real diff line");
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

    // A tab switch mid-comment must be a no-op, so the panes never swap out from under the
    // open composer (the compose-freeze invariant), matching set_scope.
    app.set_tab(Tab::AllFiles).unwrap();
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

    // The diff the App loaded for branch scope is base...HEAD, so it shows the
    // committed branch content — which the uncommitted (working-tree) scope cannot.
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

    // The frozen diff — and its title — stay on the file being commented, even though
    // the file cursor now points elsewhere. (No title/body mismatch.)
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
    assert!(app.commented_lines().contains(&app.diff_cursor), "cursor landed on a comment");
}

// --- last-turn scope -----------------------------------------------------------

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
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.apply_agent_status(Some(Status::Idle));
    app.apply_agent_status(Some(Status::Working)); // turn start: candidate = "one"
    r.write("a.rs", "one\ntwo\n");
    app.apply_agent_status(Some(Status::Working)); // first change promotes the baseline
    app.reload().unwrap();
    assert!(!app.awaiting_turn(), "the baseline is now set");
    assert!(app.entries.iter().any(|f| f.path == "a.rs"), "the turn's edit shows");
}

#[test]
fn a_question_only_turn_keeps_the_previous_turns_diff() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    // Turn A edits a file.
    app.apply_agent_status(Some(Status::Idle));
    app.apply_agent_status(Some(Status::Working));
    r.write("a.rs", "one\ntwo\n");
    app.apply_agent_status(Some(Status::Working));
    // Turn B is a question — no file change.
    app.apply_agent_status(Some(Status::Idle));
    app.apply_agent_status(Some(Status::Working));
    app.apply_agent_status(Some(Status::Idle));
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
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.apply_agent_status(Some(Status::Idle));
    app.apply_agent_status(Some(Status::Working)); // turn start: candidate = "one"
    r.write("a.rs", "one\nbefore\n"); // edit before the prompt
    app.apply_agent_status(Some(Status::Blocked)); // permission prompt promotes baseline = "one"
    app.apply_agent_status(Some(Status::Working)); // resume — must NOT re-baseline
    r.write("a.rs", "one\nbefore\nafter\n"); // edit after the prompt
    app.apply_agent_status(Some(Status::Working));
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
        let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
        app.apply_agent_status(Some(Status::Idle));
        app.apply_agent_status(Some(Status::Working));
        r.write("a.rs", "one\ntwo\n");
        app.apply_agent_status(Some(Status::Working)); // promotes and persists the ref
    }
    // A fresh App — a sidebar restart — resumes the persisted baseline.
    let mut restarted = App::new(r.path_buf(), Scope::LastTurn, None);
    restarted.reload().unwrap();
    assert!(!restarted.awaiting_turn(), "baseline resumed from the private ref");
    assert!(restarted.entries.iter().any(|f| f.path == "a.rs"), "the turn's edit still shows");
}

#[test]
fn no_agent_status_pauses_tracking() {
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.apply_agent_status(None); // no herdr / no resolvable agent
    r.write("a.rs", "one\ntwo\n");
    app.apply_agent_status(None);
    app.reload().unwrap();
    assert!(app.awaiting_turn(), "without a status signal the baseline never forms");
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

    // All files lists the whole worktree and opens its first file (README, the top-level one),
    // so src/ stays collapsed by default.
    app.set_tab(Tab::AllFiles).unwrap();
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
    app.set_tab(Tab::AllFiles).unwrap();
    let readme_row = file_row_of(&app, "README.md").expect("README.md at the top level");
    app.select_file(readme_row).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert_eq!(app.diff.view, View::File);

    // Back to Changes: its own selection and diff are restored, not All files'.
    app.set_tab(Tab::Changes).unwrap();
    assert_eq!(app.tab, Tab::Changes);
    assert_eq!(app.entries.len(), 1, "Changes still lists only the changed file");
    assert_eq!(app.diff_path, changes_open);
    assert_eq!(app.diff.view, View::Diff);

    // Forward again: All files restored README.md, not the Changes selection.
    app.set_tab(Tab::AllFiles).unwrap();
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
    };
    app.store.add(comment.clone());

    app.set_tab(Tab::AllFiles).unwrap();
    assert!(app.entries.len() >= 2, "All files lists the whole worktree");
    assert_eq!(app.changed_count(), 1, "the count is the changeset, not the worktree total");
    assert!(
        app.is_stale(&comment),
        "a diff comment keys on the changeset even while All files lists b.rs"
    );
}

/// The annotation on the `All files` row for `path`: `Some(Some(_))` annotated, `Some(None)`
/// listed-but-unchanged, `None` not visible.
#[allow(clippy::option_option)] // outer = row found, inner = its annotation
fn annotation_of(app: &App, path: &str) -> Option<Option<herdr_reviewr::file_list::Annotation>> {
    use herdr_reviewr::file_list::RowKind;
    app.file_rows.iter().find_map(|row| match &row.kind {
        RowKind::File { index, annotation } if app.entries[*index].path == path => {
            Some(annotation.clone())
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
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(
        matches!(annotation_of(&app, "a.rs"), Some(Some(a)) if a.change == ChangeKind::Modified),
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
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("b.rs", "TWO\n");
    r.commit_all("committed change to b"); // committed on the branch
    r.write("a.rs", "ONE\n"); // one uncommitted change
    let mut app = app_on(&r);
    app.set_tab(Tab::AllFiles).unwrap();
    app.focus = Focus::Files;
    app.move_cursor(1).unwrap();
    let cursor = app.file_cursor;
    assert_eq!(app.changed_count(), 1, "uncommitted marks only the dirty file");
    assert!(
        matches!(annotation_of(&app, "a.rs"), Some(Some(_))),
        "a.rs is marked under uncommitted"
    );
    assert_eq!(annotation_of(&app, "b.rs"), Some(None), "b.rs is unmarked under uncommitted");

    // Branch is a superset: it adds the committed b.rs and keeps a.rs — re-marked in place.
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(app.file_cursor, cursor, "the cursor holds across a scope re-mark");
    assert_eq!(app.changed_count(), 2, "branch marks both the committed and the dirty file");
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
    app.set_tab(Tab::AllFiles).unwrap();
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
    app.set_tab(Tab::AllFiles).unwrap();
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

    app.set_tab(Tab::AllFiles).unwrap();
    let row = file_row_of(&app, "a.rs").unwrap();
    app.select_file(row).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"), "viewing a.rs in All files");

    // Back to Changes: nothing carries over, so its own (empty) state stands.
    app.set_tab(Tab::Changes).unwrap();
    assert!(app.diff_path.is_none(), "the All files selection does not carry into Changes");
}

#[test]
fn a_file_view_comment_exports_as_path_line_with_a_context_snippet() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\ngamma\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::AllFiles).unwrap();
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
fn an_oversize_file_in_all_files_degrades_to_a_notice() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::{FileState, View};
    let r = Repo::init();
    r.write("small.rs", "fn main() {}\n");
    r.write("big.bin", &"x\n".repeat(1_100_000)); // ~2.2 MB, over the 2 MB budget
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::AllFiles).unwrap();
    let row = file_row_of(&app, "big.bin").expect("big.bin listed");
    app.select_file(row).unwrap();
    assert_eq!(app.diff.state, FileState::TooLarge, "an over-budget file is not read whole");
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
    app.set_tab(Tab::AllFiles).unwrap();
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
    assert!(!app.commented_lines().is_empty(), "renders in its own diff view");

    // In All files, open a.rs as content: the diff-anchored comment must not bleed in.
    app.set_tab(Tab::AllFiles).unwrap();
    let row = file_row_of(&app, "a.rs").expect("a.rs listed");
    app.select_file(row).unwrap();
    assert_eq!(app.diff.view, View::File);
    assert!(
        app.commented_lines().is_empty(),
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
    app.set_tab(Tab::AllFiles).unwrap();
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
    r.git(&["checkout", "-b", "feature"]);
    r.write("a.rs", &body.replace("line 5", "LINE 5"));
    r.commit_all("feature edit"); // a.rs differs from base → changed in branch scope
    r.write("a.rs", &body.replace("line 5", "LINE 5").replace("line 30", "LINE 30")); // uncommitted

    let mut app = app_on(&r); // Uncommitted scope; a.rs open in Changes
    app.focus = Focus::Diff;
    app.diff_cursor = 2;
    app.diff_scroll = 1;

    // Change scope while on All files, then return to Changes.
    app.set_tab(Tab::AllFiles).unwrap();
    app.set_scope(Scope::Branch).unwrap();
    app.set_tab(Tab::Changes).unwrap();

    assert!(app.entries.iter().any(|e| e.path == "a.rs"), "a.rs is in the branch changeset");
    assert_eq!(app.diff_scroll, 0, "an explicit scope switch snaps the Changes diff to the top");
    assert_eq!(app.diff_cursor, 0);
}

/// A PR-tab detour must not corrupt the two-tab diff stash: each file tab restores its own
/// open file when returned to, even after passing through the read-only `PR` tab.
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
    app.set_tab(Tab::AllFiles).unwrap();
    app.select_file(file_row(&app, "b.rs")).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"));

    // Detour through the PR tab; the file tabs stay frozen.
    app.set_tab(Tab::Pr).unwrap();
    assert_eq!(app.tab, Tab::Pr);

    // Returning to All files restores b.rs (active file tab unchanged → no swap).
    app.set_tab(Tab::AllFiles).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("b.rs"), "All files restored after the PR detour");

    // Returning to Changes swaps its state back — a.rs, never All files' b.rs.
    app.set_tab(Tab::Changes).unwrap();
    assert_eq!(app.tab, Tab::Changes);
    assert_eq!(app.diff_path.as_deref(), Some("a.rs"), "Changes restored without bleeding b.rs");
}

/// The PR navigator cursor walks comments only (checks are a status display), the read pane
/// tracks the selected comment, and `pr_move` clamps at both ends.
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
fn same_input_failure_preserves_any_visible_pr_snapshot_and_remedy() {
    use herdr_reviewr::forge::PrView;

    let repo = Repo::init();
    let mut app = app_on(&repo);
    let no_pr = PrView::NoPr;
    app.apply_pr(no_pr.clone());

    app.apply_pr(PrView::NotAuthed("github.example.com".to_string()));

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
    handle_mouse(app, event, area, &heights, keymap).unwrap();
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

    // `]` and `[` no longer resize. The divider follows the key: `<` moves it left, widening
    // the file list on the right, and `>` moves it back.
    let start = app.navigator_side_pct;
    press(&mut app, &keymap, KeyCode::Char('<'));
    assert!(app.navigator_side_pct > start, "`<` grows the navigator");
    press(&mut app, &keymap, KeyCode::Char('>'));
    assert_eq!(app.navigator_side_pct, start, "`>` shrinks it again");

    // Every traversal action is rebindable, like the rest of the keymap.
    let rebound = Keymap::resolve(&[(Action::NextFile, vec!['ㅁ'])]).unwrap();
    press(&mut app, &rebound, KeyCode::Char('ㅁ'));
    assert_eq!(app.diff_path.as_deref(), Some("bin.dat"));
    press(&mut app, &rebound, KeyCode::Char('f'));
    assert_eq!(app.diff_path.as_deref(), Some("bin.dat"), "the replaced default is inert");
}

#[test]
fn rebound_keys_dispatch_and_replaced_defaults_go_inert() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::resolve(&[(Action::Comment, vec!['ㅊ'])]).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');

    press(&mut app, &keymap, KeyCode::Char('c'));
    assert!(!app.composing(), "`c` was replaced and is inert");

    press(&mut app, &keymap, KeyCode::Char('ㅊ'));
    assert!(app.composing(), "the bound key opens the composer");
}

#[test]
fn fixed_keys_survive_rebinding() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::resolve(&[(Action::Down, vec!['x']), (Action::Up, vec!['z'])]).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = 0;

    press(&mut app, &keymap, KeyCode::Down);
    assert!(app.diff_cursor > 0, "the down arrow still moves the cursor");

    press(&mut app, &keymap, KeyCode::Tab);
    assert_eq!(app.focus, Focus::Files, "tab still switches focus");
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
    let keymap = Keymap::resolve(&[(Action::Delete, vec!['x'])]).unwrap();

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

    app.apply_pr(PrView::NotAuthed("github.example.com".to_string()));

    assert!(
        app.pr_notice().is_some_and(|notice| notice.ends_with("then press R.")),
        "the remedy follows the active refresh binding: {:?}",
        app.pr_notice()
    );
}

/// A repo with one markdown file and one code file, opened on the `All files` tab.
/// The `Repo` rides along: dropping it deletes the tempdir under the app.
fn markdown_app() -> (Repo, App) {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("README.md", "# Title\n\nalpha beta gamma\n");
    r.write("code.rs", "fn main() {}\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::AllFiles).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("README.md"), "first file opens");
    (r, app)
}

#[test]
fn the_markdown_preview_toggles_on_a_markdown_file_in_either_tab() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("README.md", "# Title\n");
    r.commit_all("init");
    r.write("README.md", "# Title\nmore\n");
    let mut app = app_on(&r);

    // `Changes` previews the diff's markdown file, and toggles back to the diff.
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    app.toggle_preview();
    assert!(app.preview_active(), "a markdown file previews on the Changes tab");
    app.toggle_preview();
    assert!(!app.preview_active(), "the toggle returns to the diff");

    // `All files` previews the same file the same way.
    app.set_tab(Tab::AllFiles).unwrap();
    app.toggle_preview();
    assert!(app.preview_active(), "a markdown file previews in All files");
    app.toggle_preview();
    assert!(!app.preview_active(), "the toggle returns to source");
}

#[test]
fn a_non_markdown_file_never_previews() {
    let (_repo, mut app) = markdown_app();
    app.move_cursor(1).unwrap(); // the file list is focused; move opens code.rs
    assert_eq!(app.diff_path.as_deref(), Some("code.rs"));
    app.toggle_preview();
    assert!(!app.preview_active(), "the toggle is inert on a non-markdown file");
}

#[test]
fn the_preview_is_read_only_and_scrolls_without_touching_the_source() {
    let (_repo, mut app) = markdown_app();
    app.focus = Focus::Diff;
    app.diff_cursor = 2;
    app.toggle_select();
    assert!(app.select_anchor.is_some());

    app.toggle_preview();
    assert!(app.preview_active());
    assert!(app.select_anchor.is_none(), "entering the preview clears a live selection");

    // Vertical movement scrolls the preview; the source cursor waits untouched.
    app.move_cursor(3).unwrap();
    assert_eq!(app.preview_scroll, 3);
    assert_eq!(app.diff_cursor, 2, "the source cursor is untouched");

    // Authoring and source-view keys are inert.
    app.toggle_select();
    assert!(app.select_anchor.is_none(), "no selection in the preview");
    app.start_comment();
    assert!(!app.composing(), "no commenting in the preview");
    app.toggle_wrap();
    assert!(app.wrap, "the wrap toggle is inert in the preview");

    // Over-scroll stops with the last line at the pane's bottom edge, so scrolling
    // back responds at once and content that fits the pane does not scroll.
    app.note_preview_max_scroll(4);
    app.preview_scroll_by(100);
    assert_eq!(app.preview_scroll, 4, "scroll stops at the bottom edge");
    app.preview_scroll_by(-1);
    assert_eq!(app.preview_scroll, 3, "no dead zone above the clamp");
    app.note_preview_max_scroll(0);
    app.preview_scroll_by(1);
    assert_eq!(app.preview_scroll, 0, "content that fits the pane does not scroll");

    // Returning to source restores the cursor and view state.
    app.toggle_preview();
    assert_eq!(app.diff_cursor, 2, "source restores its cursor");
}

#[test]
fn the_preview_choice_survives_a_refresh_and_dies_with_a_file_change() {
    let (_repo, mut app) = markdown_app();
    app.toggle_preview();
    app.preview_scroll_by(2);

    app.reload().unwrap();
    assert!(app.preview_active(), "a same-file refresh keeps the preview");
    assert_eq!(app.preview_scroll, 2, "the preview scroll survives the refresh");

    app.move_cursor(1).unwrap(); // open code.rs
    assert!(!app.preview_active(), "another file opens in source");
    app.move_cursor(-1).unwrap(); // back to README.md
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
    assert!(!app.preview_active(), "reopening a file starts in source");
}

#[test]
fn a_tab_switch_restores_the_preview_choice() {
    use herdr_reviewr::app::Tab;
    let (_repo, mut app) = markdown_app();
    app.toggle_preview();
    assert!(app.preview_active());

    app.set_tab(Tab::Changes).unwrap();
    assert!(!app.preview_active(), "the Changes tab holds its own choice, not All files'");
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(app.preview_active(), "the tab restores its preview choice");

    app.set_tab(Tab::Pr).unwrap();
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(app.preview_active(), "a PR round-trip also restores it");
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
fn the_toggle_carries_the_reading_position_block_aligned() {
    use herdr_reviewr::app::Tab;
    let doc = "# Title\n\npara one\n\n## Section two\n\npara two\n";
    let r = Repo::init();
    r.write("doc.md", doc);
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::AllFiles).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.note_diff_width(80);
    app.focus = Focus::Diff;

    // Entering opens at the block holding the cursor's line ("para two", source line 7).
    // The expectation derives from the render contract, not a hardcoded layout index.
    let theme = herdr_reviewr::theme::resolve(Some("catppuccin"));
    let rendered = herdr_reviewr::markdown::render(
        doc,
        80,
        &herdr_reviewr::highlight::Highlighter::new(theme.syntax),
        &theme.palette,
    );
    let block_start =
        rendered.meta.iter().position(|m| m.source_line == 7).expect("the block renders");
    app.diff_cursor = 6;
    app.toggle_preview();
    assert!(app.preview_active());
    assert_eq!(app.preview_scroll, block_start, "the preview opens at the cursor's block");

    // A scrolled return maps the top visible block back to a source cursor.
    app.preview_scroll_by(-5);
    app.toggle_preview();
    assert!(!app.preview_active());
    assert_eq!(app.diff_cursor, 0, "the top block (source line 1) becomes the cursor");

    // An unscrolled round-trip restores the exact position, even off a block start.
    app.diff_cursor = 1; // the blank line under the title
    app.toggle_preview();
    app.toggle_preview();
    assert_eq!(app.diff_cursor, 1, "no scroll input → exact restore");

    // The predicate is the gesture, not the offset: scroll away and back still maps.
    app.diff_cursor = 1;
    app.toggle_preview();
    app.preview_scroll_by(1);
    app.preview_scroll_by(-1);
    app.toggle_preview();
    assert_eq!(app.diff_cursor, 0, "a scroll gesture disables the exact restore");
}

#[test]
fn a_degraded_markdown_file_never_previews() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("empty.md", "");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::AllFiles).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("empty.md"));
    app.toggle_preview();
    assert!(!app.preview_active(), "a file showing a notice or nothing never previews");
}

#[test]
fn the_diff_view_previews_and_returns_to_the_exact_position() {
    let r = Repo::init();
    r.write("doc.md", "# Doc\n\nalpha\nbeta\ngamma\n");
    r.commit_all("init");
    r.write("doc.md", "# Doc\n\nalpha\ngamma\n"); // delete "beta"
    let mut app = app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.note_diff_width(80);
    app.focus = Focus::Diff;

    // The cursor on the deletion row ("beta", no new side) aligns entry by the nearest
    // row above with a current-content line ("alpha", new-side line 3).
    let del = app
        .visible
        .iter()
        .position(|r| r.new_no().is_none() && r.old_no().is_some())
        .expect("a deletion row");
    let theme = herdr_reviewr::theme::resolve(Some("catppuccin"));
    let rendered = herdr_reviewr::markdown::render(
        "# Doc\n\nalpha\ngamma\n",
        80,
        &herdr_reviewr::highlight::Highlighter::new(theme.syntax),
        &theme.palette,
    );
    let block = rendered.meta.iter().position(|m| m.source_line == 3).expect("the block renders");
    app.diff_cursor = del;
    app.diff_scroll = 1; // a non-top scroll that a return must not disturb
    app.toggle_preview();
    assert!(app.preview_active(), "a markdown file previews from the Changes diff");
    assert_eq!(app.preview_scroll, block, "entry aligns to the nearest current-content block");

    // Scrolling the preview and returning leaves the diff cursor and scroll exactly where
    // they were — the Diff view treats the preview as a peek (specs/diff-view.md).
    app.preview_scroll_by(1);
    app.toggle_preview();
    assert!(!app.preview_active(), "the toggle returns to the diff");
    assert_eq!(app.diff_cursor, del, "the diff cursor is untouched by a preview scroll");
    assert_eq!(app.diff_scroll, 1, "the diff scroll is untouched by a preview scroll");
}

#[test]
fn diff_preview_entry_falls_back_to_the_top_with_no_current_line_above() {
    let body = "same\n".repeat(20);
    let r = Repo::init();
    r.write("doc.md", &body);
    r.commit_all("init");
    r.write("doc.md", &format!("{body}tail\n")); // append past the context margin
    let mut app = app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.note_diff_width(80);
    app.focus = Focus::Diff;

    // The first visible row is a leading fold, with no current-content line at or above
    // the cursor, so entry opens the preview at its top (specs/diff-view.md).
    assert!(app.visible[0].new_no().is_none(), "a leading fold has no current-content line");
    app.diff_cursor = 0;
    app.toggle_preview();
    assert!(app.preview_active());
    assert_eq!(app.preview_scroll, 0, "entry with nothing above opens at the top");

    // Expanding the fold, then a preview round-trip, leaves the fold expanded — a return
    // in the Diff view never disturbs the folds (specs/diff-view.md).
    app.toggle_preview();
    expand_fold(&mut app);
    let expanded = app.visible.len();
    assert!(expanded > 1, "the leading fold expanded into rows");
    app.toggle_preview();
    app.toggle_preview();
    assert_eq!(app.visible.len(), expanded, "the return kept the fold expanded");
}

#[test]
fn a_deleted_markdown_file_never_previews_in_the_diff() {
    let r = Repo::init();
    r.write("gone.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.remove("gone.md");
    let mut app = app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("gone.md"));
    app.toggle_preview();
    assert!(!app.preview_active(), "a deleted file has no current content to preview");
}

#[test]
fn toggling_preview_after_the_changeset_empties_is_inert() {
    let r = Repo::init();
    r.write("doc.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.write("doc.md", "# Doc\n\nbody edited\n"); // an uncommitted change puts doc.md in scope
    let mut app = app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.note_diff_width(80);
    app.focus = Focus::Diff;

    // Committing the change empties the uncommitted changeset. The poll clears `visible`
    // without routing through `set_diff`, so the file's `preview_text` is left stale.
    r.commit_all("apply");
    app.reload().unwrap();
    assert!(app.visible.is_empty(), "the changeset is empty after the commit");

    // The stale render input must not make the empty pane previewable — the toggle is
    // inert and never indexes the empty row list.
    app.toggle_preview();
    assert!(!app.preview_active(), "an empty changeset never previews");
}

#[test]
fn a_scope_switch_holds_the_diff_preview() {
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

    app.toggle_preview();
    assert!(app.preview_active(), "the diff previews the markdown file");
    app.set_scope(Scope::Branch).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"), "the same file stays open");
    assert!(app.preview_active(), "the preview holds across a scope switch");
}

#[test]
fn each_file_tab_holds_its_own_diff_preview_choice() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("doc.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.write("doc.md", "# Doc\n\nbody edited\n");
    let mut app = app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));

    // Preview in Changes.
    app.toggle_preview();
    assert!(app.preview_active(), "the Changes diff previews");

    // All files opens the same file with its own choice, still source.
    app.set_tab(Tab::AllFiles).unwrap();
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    assert!(!app.preview_active(), "All files holds its own choice, still source");

    // Toggling All files on and returning to Changes finds its preview intact.
    app.toggle_preview();
    assert!(app.preview_active(), "All files previews");
    app.set_tab(Tab::Changes).unwrap();
    assert!(app.preview_active(), "Changes kept its own preview choice");
}
