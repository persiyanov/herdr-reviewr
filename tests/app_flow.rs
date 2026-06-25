//! End-to-end tests of the review loop: `App` driven against real repos, with a
//! fake export target so consume-on-success is checked without a live agent.

mod common;

use std::cell::RefCell;

use anyhow::{Result, bail};
use common::Repo;
use herdr_review::app::{App, Focus, Mode};
use herdr_review::export::ExportTarget;
use herdr_review::model::{Scope, Side};

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

fn app_on(r: &Repo) -> App {
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app
}

/// Settle the diff scroll with one display row per logical row (no wrap), for tests that
/// drive short-line diffs — reveal the cursor, then bound the offset, as the loop does.
fn clamp(app: &mut App, viewport: usize) {
    let heights = vec![1usize; app.visible.len()];
    app.reveal_diff_cursor(&heights, viewport);
    app.bound_diff_scroll(viewport);
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
fn the_wheel_scrolls_the_diff_without_moving_its_cursor() {
    let mut app = long_diff_app(40);
    app.focus = Focus::Diff;
    app.diff_cursor = 3;
    app.reveal_diff = false;
    app.wheel_diff(10);
    app.bound_diff_scroll(8);
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

#[test]
fn page_keys_move_the_cursor_in_both_panes() {
    let mut app = long_diff_app(40);
    // File pane: page moves the selection (not just the viewport).
    app.focus = Focus::Files;
    app.file_cursor = 0;
    app.reveal_files = false;
    app.page_files(5);
    assert_eq!(app.file_cursor, 5usize.min(app.file_rows.len() - 1));
    assert!(app.reveal_files);
    // Diff pane: page moves the cursor.
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    app.reveal_diff = false;
    app.page_diff(5);
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

/// The index of the first diff row with the given marker (`'+'`, `'-'`, or `' '`).
fn row_with(app: &App, marker: char) -> usize {
    app.diff.rows.iter().position(|r| r.marker() == marker).expect("a row with that marker")
}

/// The visible file-list row index for `path`.
fn file_row(app: &App, path: &str) -> usize {
    app.file_rows
        .iter()
        .position(|r| r.file_index().is_some_and(|i| app.files[i].path == path))
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
            .any(|r| r.file_index().is_some_and(|i| app.files[i].path == "src/foo.rs")),
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

/// Place the diff cursor on the first row with `marker` and write a comment there.
fn comment_on(app: &mut App, marker: char, text: &str) {
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(app, marker);
    app.start_comment();
    for ch in text.chars() {
        app.input_push(ch);
    }
    app.submit_comment();
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
    app.expand_fold();
    let expanded = app.visible.len();
    assert!(expanded > folded, "expanding reveals the hidden lines");
    assert!(app.diff_cursor < app.visible.len(), "cursor stays in range");
    assert!(!app.on_fold(), "the fold is gone, so `→` now scrolls instead");

    // Expansion is permanent — pressing again on a revealed content line does nothing.
    app.expand_fold();
    assert_eq!(app.visible.len(), expanded, "no collapse-back");
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
    app.expand_fold();
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
    assert_eq!(app.files.len(), 1);

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
    assert!(app.files.iter().any(|f| f.path == "b.rs"), "file list still refreshed");
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
    assert!(app.files.iter().any(|f| f.path == "c.rs"), "file list still refreshes");
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

    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = 30;
    app.start_comment();
    for ch in "one\ntwo\nthree".chars() {
        app.input_push(ch);
    }

    // Mirror the event loop: reserve the box's rows, then clamp. The anchored line must
    // stay within the narrowed viewport so it renders above the box.
    let viewport = 12;
    let effective = viewport - herdr_review::ui::composer_height(&app, 80);
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
fn the_pane_divider_resizes_and_clamps() {
    let r = edited_repo();
    let mut app = app_on(&r);
    let start = app.list_pct;
    app.resize_list(4);
    assert_eq!(app.list_pct, start + 4, "[ / ] step the divider");
    // Clamps: never collapses either pane however far it is pushed.
    for _ in 0..50 {
        app.resize_list(4);
    }
    assert!(app.list_pct <= 60, "the file list never swallows the diff");
    for _ in 0..50 {
        app.resize_list(-4);
    }
    assert!(app.list_pct >= 15, "the diff never swallows the file list");

    // A drag sets the width from the divider's body column.
    app.drag_divider(100, 70); // list spans columns 70..100 → 30%
    assert_eq!(app.list_pct, 30);
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
    let one_word = herdr_review::ui::composer_height(&app, width);
    for ch in "the quick brown fox jumps over the lazy dog again and again".chars() {
        app.input_push(ch);
    }
    let wrapped = herdr_review::ui::composer_height(&app, width);
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
fn editing_from_the_list_navigates_to_the_comments_file() {
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\n");
    r.write("b.rs", "one\ntwo\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nBETA\n");
    r.write("b.rs", "one\nTWO\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();

    // Comment on b.rs, then move the view to a.rs.
    let bi = app.files.iter().position(|f| f.path == "b.rs").unwrap();
    app.select_file(bi).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = row_with(&app, '+');
    app.start_comment();
    for ch in "fix this".chars() {
        app.input_push(ch);
    }
    app.submit_comment();
    let ai = app.files.iter().position(|f| f.path == "a.rs").unwrap();
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

    assert!(app.files.iter().all(|f| f.path != "a.rs"), "file left the changeset");
    assert_eq!(app.store.len(), 1, "the comment still exists");
    assert!(app.stale_files().contains("a.rs"), "and is flagged stale");
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
    assert!(app.files.iter().any(|f| f.path == "dirty.rs"));
    assert!(app.files.iter().all(|f| f.path != "committed.rs"));

    app.set_scope(Scope::Branch).unwrap();
    assert!(app.files.iter().any(|f| f.path == "committed.rs"));
    assert!(app.files.iter().all(|f| f.path != "dirty.rs"));
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
fn the_app_reads_branch_scoped_diffs_not_working_tree() {
    let r = Repo::init();
    r.write("shared.rs", "base\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("on_branch.rs", "committed on the feature branch\n");
    r.commit_all("feature work");

    let mut app = App::new(r.path_buf(), Scope::Branch, Some("main".to_string()));
    app.reload().unwrap();

    let idx = app.files.iter().position(|f| f.path == "on_branch.rs").expect("branch file listed");
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

    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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

    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    assert_ne!(app.current_file().map(|f| f.path.as_str()), Some("a.rs"));
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
    assert_ne!(app.current_file().map(|f| f.path.as_str()), Some("a.rs"));

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
    assert!(app.files.is_empty());
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
