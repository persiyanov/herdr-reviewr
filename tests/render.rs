//! Render tests: drive `ui::render` through ratatui's `TestBackend` and assert on
//! the painted buffer, so the layout and component wiring are checked for real.

mod common;

use common::Repo;
use herdr_review::app::{App, Focus};
use herdr_review::model::Scope;
use herdr_review::ui::{self, HeaderHit};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn dump(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

fn render(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    dump(terminal.backend().buffer())
}

/// Render and return the buffer, for cell-style assertions.
fn render_buffer(app: &App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// Catppuccin surface2 — the shared selection/cursor fill.
const SELECTION_BG: ratatui::style::Color = ratatui::style::Color::Rgb(0x58, 0x5b, 0x70);
/// Catppuccin peach — the comment-editor caret block.
const PEACH: ratatui::style::Color = ratatui::style::Color::Rgb(0xfa, 0xb3, 0x87);

/// Open the comment composer on the first changed line of `edited_app`.
fn composing(app: &mut App) {
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
}

#[test]
fn the_empty_comment_box_shows_a_placeholder() {
    let mut app = edited_app();
    composing(&mut app);
    assert!(render(&app).contains("Leave a comment…"), "an empty box shows the placeholder");
}

#[test]
fn the_caret_block_sits_on_the_character_at_the_caret() {
    let mut app = edited_app();
    composing(&mut app);
    app.input_push('a');
    app.input_push('b');
    app.caret_left(); // caret between 'a' and 'b' → block over 'b'
    let buf = render_buffer(&app);
    let mut found = false;
    for y in 0..40 {
        for x in 0..140 {
            if buf.cell((x, y)).is_some_and(|c| c.bg == PEACH && c.symbol() == "b") {
                found = true;
            }
        }
    }
    assert!(found, "the caret block highlights the character at the caret");
}

#[test]
fn caret_vertical_moves_between_wrapped_rows() {
    // "abcdef" hard-wraps at width 3 to "abc"/"def"; caret 4 (def col 1) up → 1; 1 down → 4.
    assert_eq!(ui::caret_vertical("abcdef", 4, 3, false), 1);
    assert_eq!(ui::caret_vertical("abcdef", 1, 3, true), 4);
}

#[test]
fn the_fold_hint_names_the_arrow_key() {
    use std::fmt::Write as _;
    let r = Repo::init();
    let mut body = String::new();
    for i in 0..30 {
        let _ = writeln!(body, "line {i}");
    }
    r.write("f.rs", &body);
    r.commit_all("init");
    r.write("f.rs", &body.replace("line 15", "LINE 15")); // one change, long runs fold
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|row| row.hidden() > 0).expect("a fold row");

    let out = render(&app);
    assert!(out.contains("→ expand"), "the fold hint names the `→` key");
    assert!(!out.contains("⏎ expand"), "no stale enter hint remains");
}

fn edited_app() -> App {
    let r = Repo::init();
    r.write("hello.rs", "alpha\nbeta\n");
    r.commit_all("init");
    r.write("hello.rs", "alpha\nBETA\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    // The repo is only needed through reload(); rendering reads cached state, so
    // `r` can drop here and clean up its tempdir.
    app
}

#[test]
fn the_file_list_renders_as_a_directory_tree() {
    let r = Repo::init();
    r.write("src/app.rs", "x\n");
    r.write("src/ui.rs", "y\n");
    r.write("Cargo.toml", "[package]\n");
    r.commit_all("init");
    r.write("src/app.rs", "x2\n");
    r.write("src/ui.rs", "y2\n");
    r.write("Cargo.toml", "[package]\nname='z'\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();

    // Scan only the file-list pane (the right third) so the diff header — which does show
    // the open file's full path — doesn't confuse the assertions.
    let files_pane: String = render(&app)
        .lines()
        .map(|l| l.chars().skip(l.chars().count() * 70 / 100).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(files_pane.contains("src/"), "the directory groups its files: {files_pane:?}");
    assert!(files_pane.contains("app.rs") && files_pane.contains("ui.rs"), "files by basename");
    assert!(!files_pane.contains("src/app.rs"), "a grouped file is not shown by full path");
    assert!(files_pane.contains("Cargo.toml"), "the top-level file shows too");
}

#[test]
fn a_saved_comment_renders_inline_as_a_card() {
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nBETA\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();

    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|row| row.marker() == '+').unwrap();
    app.start_comment();
    for ch in "memoize this".chars() {
        app.input_push(ch);
    }
    app.submit_comment(); // box closes, comment saved

    let out = render(&app);
    assert!(out.contains("memoize this"), "the saved comment stays visible inline: {out:?}");
    assert!(out.contains("comment ·"), "the inline card is titled with the location");
}

#[test]
fn a_renamed_file_shows_old_arrow_new_in_the_header() {
    let r = Repo::init();
    r.write("old_name.rs", "stable contents that survive the move\nplus a second line\n");
    r.commit_all("init");
    r.git(&["mv", "old_name.rs", "new_name.rs"]);
    r.write("new_name.rs", "stable contents that survive the move\nplus an edited line\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();

    let out = render(&app);
    assert!(out.contains("old_name.rs → new_name.rs"), "header shows the rename: {out:?}");
}

#[test]
fn tabs_expand_to_spaces_in_the_diff() {
    let r = Repo::init();
    r.write("t.rs", "x\n");
    r.commit_all("init");
    r.write("t.rs", "x\n\tindented\n"); // a tab-indented added line
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    let out = render(&app);
    let line = out.lines().find(|l| l.contains("indented")).expect("the added line renders");
    // The literal tab is gone; the word is preceded by spaces (4-col tab stop).
    assert!(!line.contains('\t'), "no literal tab in the rendered line");
    assert!(line.contains("    indented") || line.contains("   indented"), "tab became spaces");
}

#[test]
fn a_long_line_wraps_across_display_rows() {
    let long: String = std::iter::repeat_n("abcd", 60).collect(); // 240 cols, wider than the pane
    let r = Repo::init();
    r.write("w.rs", "x\n");
    r.commit_all("init");
    r.write("w.rs", &format!("x\n{long}\n"));
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap(); // wrap defaults on

    // The whole long line is visible (no truncation): every chunk renders.
    let shown: String = render(&app).chars().filter(|c| *c == 'a').collect();
    assert!(shown.len() >= 60, "all of the wrapped line is shown, not truncated");
    // The logical row reports a display height > 1 (it wraps).
    let heights = ui::diff_row_heights(&app, AREA);
    let wrapped = app.visible.iter().position(|r| r.text().starts_with("abcd")).unwrap();
    assert!(heights[wrapped] > 1, "the long line spans multiple display rows");
}

#[test]
fn wrapping_breaks_at_word_boundaries() {
    // Words sized so the line must wrap, but no word is wider than the pane: every break
    // should land on a space, so no word is split across two display rows.
    let words = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima \
                 mike november oscar papa quebec romeo sierra tango";
    let r = Repo::init();
    r.write("w.rs", "x\n");
    r.commit_all("init");
    r.write("w.rs", &format!("x\n{words}\n"));
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap(); // wrap defaults on

    let heights = ui::diff_row_heights(&app, AREA);
    let wrapped = app.visible.iter().position(|r| r.text().starts_with("alpha")).unwrap();
    assert!(heights[wrapped] > 1, "the line wraps across rows");

    // Every word survives intact on some rendered line (none straddles a wrap break).
    let out = render(&app);
    for word in words.split(' ') {
        assert!(out.lines().any(|l| l.contains(word)), "word {word:?} is not split across rows");
    }
}

#[test]
fn wide_glyphs_wrap_by_column_width_not_char_count() {
    // 50 wide CJK glyphs span 100 columns; 50 ASCII chars span 50. Width-aware wrapping
    // must give the CJK line more display rows — a char-counting wrap would tie them.
    let cjk: String = std::iter::repeat_n('あ', 50).collect();
    let ascii: String = std::iter::repeat_n('a', 50).collect();
    let r = Repo::init();
    r.write("w.rs", "x\n");
    r.commit_all("init");
    r.write("w.rs", &format!("x\n{ascii}\n{cjk}\n"));
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap(); // wrap defaults on

    let heights = ui::diff_row_heights(&app, AREA);
    let ascii_h = heights[app.visible.iter().position(|r| r.text().starts_with('a')).unwrap()];
    let cjk_h = heights[app.visible.iter().position(|r| r.text().starts_with('あ')).unwrap()];
    assert!(cjk_h > ascii_h, "wide glyphs wrap by columns: cjk {cjk_h} > ascii {ascii_h}");
}

#[test]
fn horizontal_scroll_shifts_the_diff_left() {
    let r = Repo::init();
    r.write("w.rs", "x\n");
    r.commit_all("init");
    r.write("w.rs", "x\nAAAABBBBCCCCDDDD_marker\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.wrap = false; // horizontal scroll applies only with wrap off
    app.reload().unwrap();
    assert!(render(&app).contains("AAAABBBB"), "the line head shows before scrolling");

    app.scroll_h(8); // drop the first 8 code columns
    let out = render(&app);
    assert!(!out.contains("AAAABBBB"), "the scrolled-off head is gone");
    assert!(out.contains("CCCCDDDD_marker"), "the later columns are now visible");
}

#[test]
fn a_changed_word_gets_the_emphasis_background() {
    const EMPH_INS_BG: ratatui::style::Color = ratatui::style::Color::Rgb(0x30, 0x55, 0x3f);
    let r = Repo::init();
    r.write("e.rs", "let x = foo(a);\n");
    r.commit_all("init");
    r.write("e.rs", "let x = bar(a, b);\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.focus = Focus::Files; // no diff cursor, so the emphasis bg shows
    let buf = render_buffer(&app);

    // Somewhere in the diff pane a cell carries the brighter insertion-emphasis bg,
    // and it sits under a changed character (a `b` from `bar`), not the shared prefix.
    let mut found = false;
    for y in 0..40 {
        for x in 0..95 {
            if let Some(c) = buf.cell((x, y))
                && c.bg == EMPH_INS_BG
                && c.symbol() == "b"
            {
                found = true;
            }
        }
    }
    assert!(found, "a changed word carries the emphasis background");
}

#[test]
fn the_selected_file_row_fills_with_the_shared_selection_color() {
    let app = edited_app(); // one file, file_cursor = 0, Files focused
    let buf = render_buffer(&app);
    // Files pane: right 32% of 140 cols; its border is at y=1, first content row at y=2.
    let files_x0 = 140 - 140 * 32 / 100 + 1;
    let selected =
        (files_x0..139).filter(|&x| buf.cell((x, 2)).is_some_and(|c| c.bg == SELECTION_BG)).count();
    assert!(selected > 10, "the selected file row fills wide with surface2: {selected} cells");
}

#[test]
fn shows_tab_bar_file_list_and_diff() {
    let app = edited_app();
    let out = render(&app);
    assert!(out.contains("Changes"), "tab bar names the view");
    assert!(out.contains("uncommitted"), "current scope shown");
    assert!(out.contains("hello.rs"), "file appears in the list");
    assert!(out.contains("BETA"), "diff content is rendered");
    assert!(out.contains("file(s)"), "status bar shown");
}

#[test]
fn the_footer_hints_wrap_instead_of_truncating() {
    let app = edited_app();
    // At a narrow width the Normal-mode hint line is far longer than the pane, so without
    // wrapping the last hint ("quit") would be cut off the right edge.
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|f| ui::render(f, &app)).unwrap();
    let out = dump(terminal.backend().buffer());
    assert!(out.contains("quit"), "the wrapped footer keeps its last hint:\n{out}");
}

#[test]
fn empty_repo_shows_empty_states() {
    let r = Repo::init();
    r.write("seed.rs", "x\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();

    let out = render(&app);
    assert!(out.contains("no changes"), "empty file list state");
}

#[test]
fn composing_renders_the_inline_multiline_box() {
    let mut app = edited_app();
    app.focus = Focus::Diff;
    app.diff_cursor = app.diff.rows.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
    for ch in "line one".chars() {
        app.input_push(ch);
    }
    app.input_push('\n');
    for ch in "line two".chars() {
        app.input_push(ch);
    }

    let out = render(&app);
    assert!(out.contains("comment ·"), "box titled with the location");
    assert!(out.contains("line one"), "first input line shown");
    assert!(out.contains("line two"), "second input line shown — the box is multi-line");
}

#[test]
fn the_box_grows_with_multiline_input_and_keeps_the_anchor_visible() {
    let r = Repo::init();
    r.write("mid.rs", "a\nb\nc\nd\ne\n");
    r.commit_all("init");
    r.write("mid.rs", "a\nB\nc\nd\ne\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor =
        app.diff.rows.iter().position(|r| r.marker() == '+' && r.text().contains('B')).unwrap();
    app.start_comment();
    for ch in "one\ntwo\nthree".chars() {
        app.input_push(ch);
    }

    let out = render(&app);
    assert!(out.contains("one") && out.contains("two") && out.contains("three"), "all box lines");
    let lines: Vec<&str> = out.lines().collect();
    // The inserted line is the only one carrying an uppercase `B` (no `+` glyph now).
    let anchor = lines.iter().position(|l| l.contains('B')).expect("anchor line visible");
    let box_row = lines.iter().position(|l| l.contains("comment ·")).expect("box");
    assert!(anchor < box_row, "the commented line stays above the box as it grows");
}

#[test]
fn the_box_is_inserted_under_the_selected_line() {
    let r = Repo::init();
    r.write("mid.rs", "alpha\nbeta\ngamma\n");
    r.commit_all("init");
    r.write("mid.rs", "alpha\nBETA\ngamma\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = app.diff.rows.iter().position(|r| r.text().contains("BETA")).unwrap();
    app.start_comment();
    for ch in "note".chars() {
        app.input_push(ch);
    }

    let out = render(&app);
    let lines: Vec<&str> = out.lines().collect();
    let box_row = lines.iter().position(|l| l.contains("comment ·")).expect("box rendered");
    let below_row = lines.iter().position(|l| l.contains("gamma")).expect("context below shown");
    assert!(below_row > box_row, "the diff line below the selection is pushed under the box");
}

const AREA: Rect = Rect { x: 0, y: 0, width: 140, height: 40 };

#[test]
fn header_clicks_map_to_scope_and_send() {
    let app = edited_app(); // scope uncommitted, no comments
    // Scan the header row instead of hardcoding columns, so the test survives changes
    // to the label/button text.
    let scope: Vec<u16> = (0..AREA.width)
        .filter(|&c| ui::hit_header(AREA, &app, c, 0) == Some(HeaderHit::Scope))
        .collect();
    let send: Vec<u16> = (0..AREA.width)
        .filter(|&c| ui::hit_header(AREA, &app, c, 0) == Some(HeaderHit::Send))
        .collect();

    assert!(!scope.is_empty(), "scope chip is clickable");
    assert!(!send.is_empty(), "send button is clickable");
    assert!(scope.iter().max() < send.iter().min(), "scope is left of the button, no overlap");
    assert!(*send.iter().max().unwrap() < AREA.width);

    let gap = scope.iter().max().unwrap() + 1;
    assert_eq!(ui::hit_header(AREA, &app, gap, 0), None, "the space between controls is inert");
    assert_eq!(ui::hit_header(AREA, &app, scope[0], 5), None, "only row 0 is the header");
}

#[test]
fn file_and_diff_clicks_map_to_row_indices() {
    let app = edited_app();
    // Right pane: the first file row maps to index 0; clicking past the list misses.
    assert_eq!(ui::hit_file(AREA, app.list_pct, 120, 2, app.file_rows.len(), 0), Some(0));
    assert_eq!(ui::hit_file(AREA, app.list_pct, 120, 9, app.file_rows.len(), 0), None);
    // With the list scrolled down, the top visible row maps to that scrolled-to index.
    assert_eq!(ui::hit_file(AREA, app.list_pct, 120, 2, 50, 7), Some(7));
    assert_eq!(ui::hit_file(AREA, app.list_pct, 120, 3, 50, 7), Some(8));
    // The wheel routes by pointer: a column in the right pane is "in" the file list,
    // one in the left (diff) pane is not.
    assert!(ui::in_files_pane(AREA, app.list_pct, 120, 3));
    assert!(!ui::in_files_pane(AREA, app.list_pct, 10, 3));
    // Left pane: diff rows map top-down to diff-line indices.
    assert!(app.visible.len() > 1);
    let heights = ui::diff_row_heights(&app, AREA);
    assert_eq!(ui::hit_diff(AREA, app.list_pct, 10, 2, &heights, 0), Some(0));
    assert_eq!(ui::hit_diff(AREA, app.list_pct, 10, 3, &heights, 0), Some(1));
    // With a nonzero scroll and wrapped (multi-row) lines, the click must skip the
    // scrolled-off rows and account for each visible row's display height. Rows are
    // 2 tall each; diff_scroll=1 puts row index 1 at the top of the pane (inner.y == 2).
    let tall = [2usize, 2, 2, 2];
    assert_eq!(ui::hit_diff(AREA, app.list_pct, 10, 2, &tall, 1), Some(1)); // top visible row
    assert_eq!(ui::hit_diff(AREA, app.list_pct, 10, 3, &tall, 1), Some(1)); // its second display row
    assert_eq!(ui::hit_diff(AREA, app.list_pct, 10, 4, &tall, 1), Some(2)); // next logical row
}

#[test]
fn a_binary_file_shows_the_no_line_comments_message() {
    let r = Repo::init();
    r.write("logo.bin", "\0\0\0\0seed\0\0");
    r.commit_all("init");
    r.write("logo.bin", "\0\0\0\0changed\0\0\0");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    let idx = app.files.iter().position(|f| f.path == "logo.bin").expect("binary file listed");
    app.select_file(idx).unwrap();

    let out = render(&app);
    assert!(out.contains("binary — no line comments"), "binary diff message shown:\n{out}");
}

#[test]
fn the_comments_list_flags_a_stale_comment() {
    let r = Repo::init();
    r.write("a.rs", "alpha\nbeta\n");
    r.commit_all("init");
    r.write("a.rs", "alpha\nBETA\n");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = app.diff.rows.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
    for ch in "look here".chars() {
        app.input_push(ch);
    }
    app.submit_comment();

    // a.rs reverts to its committed state → leaves the changeset → the comment is stale.
    r.write("a.rs", "alpha\nbeta\n");
    app.reload().unwrap();
    app.open_list();

    let out = render(&app);
    assert!(out.contains("(stale)"), "stale comment flagged in the list:\n{out}");
}

#[test]
fn open_list_renders_the_comments_overlay() {
    let mut app = edited_app();
    app.focus = Focus::Diff;
    app.diff_cursor = app.diff.rows.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
    for ch in "overlay note".chars() {
        app.input_push(ch);
    }
    app.submit_comment();
    app.open_list();

    let out = render(&app);
    assert!(out.contains("Comments ("), "overlay titled with a count");
    assert!(out.contains("overlay note"), "comment text listed");
}

#[test]
fn last_turn_without_a_baseline_renders_the_waiting_state() {
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.reload().unwrap();
    let out = render(&app);
    assert!(out.contains("[last turn]"), "the scope chip reads last turn");
    assert!(out.contains("waiting for the agent's next turn"), "the cold-start empty state shows");
}
