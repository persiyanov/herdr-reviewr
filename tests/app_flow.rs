//! End-to-end tests of the review loop: `App` driven against real repos, with a
//! fake export target so consume-on-success is checked without a live agent.

mod common;

use std::cell::RefCell;

use anyhow::{Result, bail};
use common::{Repo, app_on, enter_tab, typed};
use herdr_reviewr::app::{App, Band, Focus, FooterAction, Mode};
use herdr_reviewr::config::NavigatorPosition;
use herdr_reviewr::export::ExportTarget;
use herdr_reviewr::keymap::{Action, Key, Keymap};
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

    // Arm the crossing at a.rs's last hunk: row 1 leads with the offer as its primary, and the
    // comment key stays on the bar (demoted to a `Do` action) because it still works here. The
    // hunk pair drops from the `move` band so the armed key is never listed twice.
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
    enter_tab(&mut app, herdr_reviewr::app::Tab::AllFiles);
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
    // A poll re-derives the footer's content but never moves the toggle (overview.md Continuity).
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
    // `?` is the `keys` binding, but the editor's mode-check runs before the keymap dispatch, so it
    // types a literal `?` and never toggles the expansion (specs/input.md).
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

    // A live selection sits above the open expansion: the first esc clears the selection, the
    // next closes the expansion — one layer per press.
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
    // Hunk stepping is inert outside the Changes diff (`step_hunk` early-returns), so the move band
    // must not advertise `] [ hunk` on All files, where file stepping still works.
    let r = edited_repo();
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let acts: Vec<_> = app.footer_bands().into_iter().map(|(a, _)| a).collect();
    assert!(acts.contains(&FooterAction::MoveFile), "file stepping still works on All files");
    assert!(!acts.contains(&FooterAction::MoveHunk), "but hunk stepping is inert there");
}

#[test]
fn the_go_band_never_repeats_the_empty_scope_primary() {
    // An empty changeset leads row 1 with `scope`; the `go` band must not list it a second time.
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("c"); // nothing uncommitted → the empty state
    let app = app_on(&r);
    let bands = app.footer_bands();
    assert_eq!(
        bands.first().map(|&(a, _)| a),
        Some(FooterAction::Scope),
        "scope leads the empty state"
    );
    let scopes = bands.iter().filter(|&&(a, _)| a == FooterAction::Scope).count();
    assert_eq!(scopes, 1, "scope is not repeated in the go band");
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

/// Drive one status sample on the worker-owned turn host and mirror its baseline into the
/// app, exactly as a world completion landing would (specs/herdr-host.md).
fn observe_turn(app: &mut App, host: &mut herdr_reviewr::world::TurnHost, status: Option<Status>) {
    host.observe(status);
    app.sync_turn_baseline(host.baseline().map(str::to_string));
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
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    let mut host = herdr_reviewr::world::TurnHost::open(r.path_buf());
    observe_turn(&mut app, &mut host, Some(Status::Idle));
    observe_turn(&mut app, &mut host, Some(Status::Working)); // turn start: candidate = "one"
    r.write("a.rs", "one\ntwo\n");
    observe_turn(&mut app, &mut host, Some(Status::Working)); // first change promotes the baseline
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
    let mut host = herdr_reviewr::world::TurnHost::open(r.path_buf());
    // Turn A edits a file.
    observe_turn(&mut app, &mut host, Some(Status::Idle));
    observe_turn(&mut app, &mut host, Some(Status::Working));
    r.write("a.rs", "one\ntwo\n");
    observe_turn(&mut app, &mut host, Some(Status::Working));
    // Turn B is a question — no file change.
    observe_turn(&mut app, &mut host, Some(Status::Idle));
    observe_turn(&mut app, &mut host, Some(Status::Working));
    observe_turn(&mut app, &mut host, Some(Status::Idle));
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
    let mut host = herdr_reviewr::world::TurnHost::open(r.path_buf());
    observe_turn(&mut app, &mut host, Some(Status::Idle));
    observe_turn(&mut app, &mut host, Some(Status::Working)); // turn start: candidate = "one"
    r.write("a.rs", "one\nbefore\n"); // edit before the prompt
    observe_turn(&mut app, &mut host, Some(Status::Blocked)); // permission prompt promotes baseline = "one"
    observe_turn(&mut app, &mut host, Some(Status::Working)); // resume — must NOT re-baseline
    r.write("a.rs", "one\nbefore\nafter\n"); // edit after the prompt
    observe_turn(&mut app, &mut host, Some(Status::Working));
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
        let mut host = herdr_reviewr::world::TurnHost::open(r.path_buf());
        observe_turn(&mut app, &mut host, Some(Status::Idle));
        observe_turn(&mut app, &mut host, Some(Status::Working));
        r.write("a.rs", "one\ntwo\n");
        observe_turn(&mut app, &mut host, Some(Status::Working)); // promotes and persists the ref
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
    let mut host = herdr_reviewr::world::TurnHost::open(r.path_buf());
    observe_turn(&mut app, &mut host, None); // no herdr / no resolvable agent
    r.write("a.rs", "one\ntwo\n");
    observe_turn(&mut app, &mut host, None);
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

    // A first visit has no stash to paint, so it loads before its frame
    // (policies/ux-responsiveness.md): no pending request survives the switch.
    app.set_tab(Tab::AllFiles).unwrap();
    assert!(app.world_request.is_none(), "a first visit loads synchronously");
    assert!(app.entries.iter().any(|e| e.path == "a.rs"), "the first frame is populated");

    // A return visit paints the stashed frame as it was and requests its refresh
    // (specs/tui.md, specs/overview.md Continuity).
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
    enter_tab(&mut app, Tab::AllFiles);
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

    // Branch is a superset: the changed set rebuilds before the frame, the tree's
    // annotations land with the queued refresh (specs/tui.md).
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
fn an_oversize_file_in_all_files_degrades_to_a_notice() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::diff::{FileState, View};
    let r = Repo::init();
    r.write("small.rs", "fn main() {}\n");
    r.write("big.bin", &"x\n".repeat(1_100_000)); // ~2.2 MB, over the 2 MB budget
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
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
    assert!(!app.commented_lines().is_empty(), "renders in its own diff view");

    // In All files, open a.rs as content: the diff-anchored comment must not bleed in.
    enter_tab(&mut app, Tab::AllFiles);
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
fn fixed_keys_survive_rebinding() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let keymap = Keymap::resolve(&[
        (Action::Down, vec![Key::plain('x')]),
        (Action::Up, vec![Key::plain('z')]),
    ])
    .unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = 0;

    press(&mut app, &keymap, KeyCode::Down);
    assert!(app.diff_cursor > 0, "the down arrow still moves the cursor");

    press(&mut app, &keymap, KeyCode::Tab);
    assert_eq!(app.focus, Focus::Files, "tab still switches focus");
}

// ---- in-file find (specs/find-in-file.md) -----------------------------------------------

/// A repo whose only change is a new `m.rs`: it renders as all-insertion rows, so the diff has
/// no context and no folds — one row per line, matching is straightforward.
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
    // An empty file has no content rows, so `find_available` is false (specs/find-in-file.md).
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
fn find_is_inert_in_the_markdown_preview() {
    let r = Repo::init();
    r.write("base.txt", "x\n");
    r.commit_all("init");
    r.write("doc.md", "# Title\n\nthe word total appears here\n");
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    app.focus = Focus::Diff;
    press(&mut app, &keymap, KeyCode::Char('m')); // open the markdown preview
    assert!(app.preview_active(), "the preview is open");
    open_find(&mut app, &keymap);
    assert_ne!(app.mode, Mode::Find, "find is inert in the markdown preview");
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

    assert_eq!(app.mode, Mode::Find, "a same-file poll keeps the band open (specs O6)");
    assert_eq!(app.find_count().unwrap().1, before + 1, "the count re-derives from new content");
}

#[test]
fn find_opens_on_a_rebound_alt_chord_through_the_dispatcher() {
    let r = find_repo();
    let mut app = app_on(&r);
    let keymap =
        Keymap::resolve(&[(Action::Find, vec![Key { ctrl: false, alt: true, ch: 'x' }])]).unwrap();
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
    enter_tab(&mut app, Tab::AllFiles);
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
    enter_tab(&mut app, Tab::AllFiles);
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

    enter_tab(&mut app, Tab::Changes);
    assert!(!app.preview_active(), "the Changes tab holds its own choice, not All files'");
    enter_tab(&mut app, Tab::AllFiles);
    assert!(app.preview_active(), "the tab restores its preview choice");

    app.set_tab(Tab::Pr).unwrap();
    enter_tab(&mut app, Tab::AllFiles);
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
    enter_tab(&mut app, Tab::AllFiles);
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
    enter_tab(&mut app, Tab::AllFiles);
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
    enter_tab(&mut app, Tab::AllFiles);
    assert_eq!(app.diff_path.as_deref(), Some("doc.md"));
    assert!(!app.preview_active(), "All files holds its own choice, still source");

    // Toggling All files on and returning to Changes finds its preview intact.
    app.toggle_preview();
    assert!(app.preview_active(), "All files previews");
    enter_tab(&mut app, Tab::Changes);
    assert!(app.preview_active(), "Changes kept its own preview choice");
}

// --- world completions ---------------------------------------------------------

/// A completion as the worker would send it: built now, for the app's current input,
/// tagged `generation`.
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
    stale.turn = Some(herdr_reviewr::world::TurnReport { ended: true });
    let before = app.entries.clone();
    assert!(
        !herdr_reviewr::land_world_completion(&mut app, stale, 4),
        "a superseded tag never clears the live in-flight marker"
    );
    assert_eq!(app.entries, before, "a superseded snapshot never paints");
    assert!(app.pr_pending, "the turn end still schedules the PR refetch");
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
fn outside_a_repo_the_build_yields_the_quiet_empty_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(dir.path().to_path_buf(), Scope::Uncommitted, None);
    let snapshot = herdr_reviewr::world::build(&app.world_input()).unwrap();
    assert!(snapshot.entries.is_empty(), "no error, no entries — the empty state stays quiet");
    assert!(herdr_reviewr::world::build_changed(&app.world_input()).unwrap().is_empty());
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

// --- Search overlay (specs/search.md) --------------------------------------------------

mod search_overlay {
    use super::{common, press};
    use common::{Repo, app_on, enter_tab};
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

        // Every tab's footer carries the hint, and `/` opens from each (specs/search.md).
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
        // Files has three rows, Code just one: flipping onto the shorter set must reset
        // the pick, or a stale index would point past the code results.
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

    /// A world poll reconciles the preview but never reshapes the results or the pick —
    /// only an edit re-queries (specs/search.md, overview.md Continuity O6).
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

    /// An error completion holds the previous results but paints only its message, so
    /// `enter` must open nothing off the invisible stale rows (specs/search.md).
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

    /// With nothing pickable the footer offers only the exit, so it never lists a key
    /// that would not work (specs/input.md, specs/search.md).
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

    /// A divider gesture cancelled by `/` still owns its mouse-up: the release frees the
    /// capture and never resolves into a pick (specs/input.md, specs/search.md).
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
        )
        .unwrap();

        // `/` cancels the gesture and opens the overlay; land a result so a stray pick
        // would be observable.
        open(&mut app, &keymap);
        land_search_completion(&mut app, done(1, results(vec![file_hit("a.rs")], Vec::new())), 1);
        assert!(app.divider_drag_captured(), "the cancelled gesture still owns its events");

        herdr_reviewr::handle_mouse(
            &mut app,
            event(MouseEventKind::Up(MouseButton::Left), divider),
            area,
            &heights,
            &keymap,
        )
        .unwrap();
        assert!(!app.divider_drag_captured(), "mouse-up releases the capture");
        assert_eq!(app.mode, Mode::Search, "the release never resolves into a pick");
    }

    /// The query edits with the comment editor's caret controls (specs/search.md):
    /// word jumps, kills, Home/End, and mid-string inserts, with a paste's newlines
    /// flattened to spaces.
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

    /// `ctrl+n` / `ctrl+p` move the pick, like `↓`/`↑` (specs/search.md Keys). Plain
    /// `n`/`p` still type into the query.
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

    /// The preview follows the pick on settle: a retarget doesn't rebuild until the settle
    /// call, which lands on the new pick and carries a code pick's hit; an unchanged pick is
    /// not rebuilt (specs/search.md Preview).
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

        // Idempotent: a settle with the pick unchanged does not rebuild — a rebuild would
        // re-center, so the cleared center flag must survive.
        app.scroll_search_preview(1);
        app.build_search_preview();
        assert!(
            !app.search.as_ref().unwrap().preview.as_ref().unwrap().center.get(),
            "an unchanged pick is not rebuilt on settle",
        );
    }

    /// A landed poll repaints the previewed file in place — same scroll, fresh content —
    /// and a deleted previewed file previews empty (specs/search.md Preview).
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

    /// Opening a result is a deliberate leave: it lands in `All files` whatever tab the
    /// search left, and the origin tab keeps its place (specs/search.md Opening).
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

    /// The search divider drags search's own share; the review layout's shares stay
    /// untouched (specs/search.md).
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
        // A divider drag held from the review view is cancelled on open, so its remaining
        // drag events are consumed, not turned into a search-split resize (specs/input.md).
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
                } else {
                    out.push(path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// The real engine, end to end: spawn the worker, run a query, and check the contract —
    /// results arrive, ignored files and `.git` never appear, and the worktree gains no
    /// file (specs/overview.md O1).
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
        // (specs/search.md).
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
