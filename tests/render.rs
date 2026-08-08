//! Render tests: drive `ui::render` through ratatui's `TestBackend` and assert on
//! the painted buffer, so the layout and component wiring are checked for real.

mod common;

use common::{Repo, app_on, enter_tab};
use herdr_reviewr::app::{App, Focus, Tab};
use herdr_reviewr::config::NavigatorPosition;
use herdr_reviewr::herdr::AgentChoice;
use herdr_reviewr::keymap::Keymap;
use herdr_reviewr::model::Scope;
use herdr_reviewr::ui::{self, HeaderHit};
use herdr_reviewr::{handle_key, handle_mouse};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
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
    dump(&render_size(app, 140, 40))
}

/// Render and return the buffer, for cell-style assertions.
fn render_buffer(app: &App) -> Buffer {
    render_size(app, 140, 40)
}

/// Render at a specific width (height fixed), for footer fit-to-width assertions.
fn render_at(app: &App, width: u16) -> String {
    dump(&render_size(app, width, 12))
}

fn render_size(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// Catppuccin surface2 — the shared selection/cursor fill.
const SELECTION_BG: ratatui::style::Color = ratatui::style::Color::Rgb(0x58, 0x5b, 0x70);
/// Catppuccin peach — the comment-editor caret block.
const PEACH: ratatui::style::Color = ratatui::style::Color::Rgb(0xfa, 0xb3, 0x87);

/// The right `100-pct`% of every frame row, for pane-scoped assertions — one home for
/// the column math, so the two panes' cut points can't drift apart silently.
fn right_column(out: &str, pct: usize) -> String {
    out.lines()
        .map(|l| l.chars().skip(l.chars().count() * pct / 100).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first painted link region anywhere on the test frame, scanned over its grid.
fn first_painted_link(app: &App) -> Option<std::sync::Arc<str>> {
    (0..40u16)
        .flat_map(|y| (0..140u16).map(move |x| (x, y)))
        .find_map(|(x, y)| app.painted_link_at(x, y))
}

/// Open the comment composer on the first changed line of `edited_app`.
fn composing(app: &mut App) {
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
}

#[test]
fn invalid_config_replaces_the_entire_pane_with_its_error() {
    let mut app = edited_app();
    app.set_config_error(
        "config /tmp/reviewr/config.toml: invalid value for `theme`; expected a built-in theme name"
            .to_string(),
    );

    let out = render(&app);

    assert!(out.contains("config /tmp/reviewr/config.toml"));
    assert!(out.contains("expected a built-in theme name"));
    assert!(out.contains("The config reloads automatically."));
    assert!(!out.contains("Changes"), "normal reviewr chrome must be hidden");
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
    // Composer wrapping preserves repeated spaces so every caret index remains addressable.
    assert_eq!(ui::caret_vertical("ab  cd", 4, 2, false), 2);
    assert_eq!(ui::caret_vertical("ab  cd", 2, 2, true), 4);
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
    let mut app = app_on(&r);
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
    // The repo is only needed through reload(); rendering reads cached state, so
    // `r` can drop here and clean up its tempdir.
    app_on(&r)
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
    let app = app_on(&r);

    // Scan only the default-right navigator so the diff header — which does show
    // the open file's full path — doesn't confuse the assertions.
    let files_pane = right_column(&render(&app), 70);
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
    let mut app = app_on(&r);

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
    let app = app_on(&r);

    let out = render(&app);
    assert!(out.contains("old_name.rs → new_name.rs"), "header shows the rename: {out:?}");
}

#[test]
fn tabs_expand_to_spaces_in_the_diff() {
    let r = Repo::init();
    r.write("t.rs", "x\n");
    r.commit_all("init");
    r.write("t.rs", "x\n\tindented\n"); // a tab-indented added line
    let app = app_on(&r);
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
    let app = app_on(&r); // wrap defaults on

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
    let app = app_on(&r); // wrap defaults on

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
    let app = app_on(&r); // wrap defaults on

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
    let mut app = app_on(&r);
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

/// Catppuccin surface1 — the cursor fill of the pane that does not hold focus.
const UNFOCUSED_CURSOR_BG: ratatui::style::Color = ratatui::style::Color::Rgb(0x45, 0x47, 0x5a);

#[test]
fn the_diff_cursor_row_is_marked_from_either_pane() {
    // The diff pane's cursor row fills like the file list's: brightest when the pane holds
    // focus, a step softer when it does not. A hunk step driven from the file list moves this
    // cursor, so it has to be visible from there.
    let mut app = edited_app();
    app.focus = Focus::Diff;
    app.next_hunk();
    let cursor_y = |app: &App| 2 + app.diff_cursor as u16; // border at y=1, first row at y=2
    let fill = |app: &App, bg| {
        let buf = render_buffer(app);
        let y = cursor_y(app);
        (1..40u16).filter(|&x| buf.cell((x, y)).is_some_and(|c| c.bg == bg)).count()
    };

    assert!(fill(&app, SELECTION_BG) > 10, "the focused diff fills its cursor row with surface2");

    app.focus = Focus::Files;
    assert!(
        fill(&app, UNFOCUSED_CURSOR_BG) > 10,
        "and still marks it, a step softer, while the file list holds focus"
    );
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
fn a_hidden_navigator_gives_the_read_pane_the_whole_body() {
    let mut app = edited_app();
    app.focus = Focus::Diff;
    app.next_hunk();
    let cursor_y = 2 + app.diff_cursor as u16;
    let fill = |app: &App| {
        let buf = render_buffer(app);
        (1..139u16)
            .filter(|&x| buf.cell((x, cursor_y)).is_some_and(|c| c.bg == SELECTION_BG))
            .count()
    };
    let visible_fill = fill(&app);
    let out = render(&app);
    assert!(!out.contains("z hide"), "visible and collapsed, the hide key waits under `?`");

    app.toggle_navigator_hidden();
    let hidden_fill = fill(&app);
    assert!(
        hidden_fill > visible_fill && hidden_fill > 120,
        "the cursor row fills the whole body with surface2: {hidden_fill} vs {visible_fill}"
    );
    let out = render(&app);
    assert!(out.contains("z show"), "the collapsed footer names the way back");

    app.toggle_keys();
    let out = render(&app);
    assert!(out.contains("z show"), "row 1 keeps the way back in the expansion");
    assert!(!out.contains("p position"), "`p position` drops while hidden");

    app.toggle_navigator_hidden();
    let out = render(&app);
    assert!(out.contains("z hide"), "visible, the `go` band lists the hide key");
    assert!(out.contains("p position"), "`p position` returns with the navigator");
}

#[test]
fn shows_tab_bar_file_list_and_diff() {
    let app = edited_app();
    let out = render(&app);
    assert!(out.contains("Changes"), "tab bar names the view");
    assert!(out.contains("uncommitted"), "current scope shown");
    assert!(out.contains("hello.rs"), "file appears in the list");
    assert!(out.contains("BETA"), "diff content is rendered");
    assert!(out.contains("changed"), "the header shows the changed count");
}

#[test]
fn the_header_totals_the_scope_and_hides_them_at_zero() {
    let r = Repo::init();
    r.write("edited.rs", "old\n");
    r.commit_all("init");
    r.write("edited.rs", "new\n");
    r.write("untracked.rs", "one\ntwo\n");
    let app = app_on(&r);

    // 64 columns is the exact fit (the tab strip ends in the two-column reserved
    // indicator cell). The totals' `−` is multi-byte, so this breaks if the header
    // measures bytes instead of display width.
    let header = render_at(&app, 64).lines().next().unwrap().to_string();
    assert!(header.contains("2 changed  +3 −1"), "count, then the totals:\n{header}");

    let clean = Repo::init();
    clean.write("clean.rs", "same\n");
    clean.commit_all("init");
    let app = app_on(&clean);
    let header = render_at(&app, 80).lines().next().unwrap().to_string();
    assert!(header.contains("0 changed"), "the bare count remains:\n{header}");
    assert!(!header.contains('+'), "an empty changeset shows no totals:\n{header}");
}

/// The last non-blank rendered row — the footer band.
fn footer_line(out: &str) -> String {
    out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default().to_string()
}

/// Focus the diff on its first changed line.
fn on_changed_line(app: &mut App) {
    app.focus = Focus::Diff;
    app.diff_cursor = app.visible.iter().position(|r| r.marker() == '+').unwrap();
}

#[test]
fn the_footer_offers_the_armed_crossing_in_both_directions() {
    // Two files, one hunk each, so a hunk step from either end has only a crossing left to offer.
    let r = Repo::init();
    r.write("a.rs", "one\ntwo\n");
    r.write("z.rs", "one\ntwo\n");
    r.commit_all("init");
    r.write("a.rs", "one\nEDIT A\n");
    r.write("z.rs", "one\nEDIT Z\n");
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    app.next_hunk(); // onto a.rs's only hunk
    app.next_hunk(); // nothing below it: arms the crossing forward
    let footer = footer_line(&render(&app));
    assert!(footer.contains("] next file"), "the armed crossing leads the bar:\n{footer}");
    assert!(footer.contains("c comment"), "and the line's own action stays:\n{footer}");

    app.next_hunk(); // takes it
    app.prev_hunk(); // nothing above z.rs's hunk: arms the crossing back
    let footer = footer_line(&render(&app));
    assert!(footer.contains("[ prev file"), "armed backward, the bar names `[`:\n{footer}");
}

#[test]
fn the_footer_shows_the_action_for_the_context() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    let footer = footer_line(&render(&app));
    assert!(footer.contains("c comment"), "a diff line offers comment:\n{footer}");
    assert!(footer.contains("v select"), "and selecting a range:\n{footer}");
    assert!(!footer.contains("changed"), "the changed count is not in the footer:\n{footer}");
}

#[test]
fn the_footer_trims_trailing_actions_to_fit_keeping_the_primary_and_the_more_hint() {
    let mut app = edited_app();
    on_changed_line(&mut app); // diff focus, content line → c comment · v select … ?
    // Wide: every cursor action fits, and the `?` closes the row.
    let wide = footer_line(&render_at(&app, 120));
    assert!(
        wide.contains("c comment") && wide.contains("v select") && wide.trim_end().ends_with('?'),
        "wide footer shows all actions and the `?`:\n{wide}"
    );
    // Narrow: the primary survives, the trailing action drops, and the `?` stays at the right.
    let narrow = footer_line(&render_at(&app, 18));
    assert!(narrow.contains("c comment"), "the primary action is never dropped:\n{narrow}");
    assert!(narrow.trim_end().ends_with('?'), "the `?` never drops:\n{narrow}");
    assert!(!narrow.contains("v select"), "the trailing action is trimmed off row 1:\n{narrow}");
    // Too narrow for the primary and the `?` together: the primary sheds its label to its key, and
    // the `?` still survives at the right.
    let tiny = footer_line(&render_at(&app, 11));
    assert!(tiny.contains(" c "), "the primary keeps its key:\n{tiny}");
    assert!(!tiny.contains("comment"), "the primary sheds its label:\n{tiny}");
    assert!(tiny.trim_end().ends_with('?'), "the `?` still survives:\n{tiny}");
}

#[test]
fn a_narrow_row_keeps_send_and_the_more_hint_by_shedding_the_primary_label() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.start_comment();
    for ch in "n".chars() {
        app.input_push(ch);
    }
    app.submit_comment(); // a written comment adds `s send 1` to row 1
    // A pane too narrow for the full primary alongside `send` and `?` keeps all three by shedding
    // the primary's label — `send` and the `?` must never clip off the right edge.
    let narrow = footer_line(&render_at(&app, 16));
    assert!(narrow.contains("s send 1"), "send never drops:\n{narrow}");
    assert!(narrow.trim_end().ends_with('?'), "the `?` never drops:\n{narrow}");
    assert!(narrow.chars().count() <= 16, "the row never overflows its width:\n{narrow}");
}

#[test]
fn a_status_too_long_to_paint_never_costs_the_row_the_actions_that_fit() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.start_comment();
    app.input_push('n');
    app.submit_comment(); // a written comment adds `s send 1` to row 1

    // A herdr failure is the longest line the status ever carries. Where the row has no room to
    // paint any of it, the status must cost nothing: the row falls back to exactly what it shows
    // with no status at all. Reserving room for a message that then drops would spend the width
    // twice and paint neither (`specs/input.md`).
    for w in 14..=140u16 {
        app.status = "z".repeat(60);
        let with = footer_line(&render_at(&app, w));
        app.status = String::new();
        let without = footer_line(&render_at(&app, w));
        if !with.contains('z') {
            assert_eq!(with, without, "width {w} paid for a status it never painted");
        }
    }
}

#[test]
fn the_footer_shows_the_sends_outcome_at_a_pane_width_by_yielding_the_cursor_actions() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.start_comment();
    app.input_push('n');
    app.submit_comment(); // a written comment adds `s send 1` to row 1

    // The status is the only answer `s` gives, and a reviewr pane is around 40 columns wide, so
    // the cursor's actions yield to it: the `?` panel repeats every action and nothing repeats the
    // status (`specs/input.md`).
    app.status = "no agent here — copy to the clipboard instead".to_string();
    let narrow = footer_line(&render_at(&app, 40));
    assert!(narrow.contains("no agent here"), "the refusal shows at 40 columns:\n{narrow}");
    assert!(narrow.contains("s send 1"), "send never drops:\n{narrow}");
    assert!(narrow.trim_end().ends_with('?'), "the `?` never drops:\n{narrow}");
    assert!(!narrow.contains("d delete"), "the cursor's actions yield to the status:\n{narrow}");

    // With room for both, nothing yields.
    let wide = footer_line(&render_at(&app, 120));
    assert!(
        wide.contains("no agent here — copy to the clipboard instead"),
        "a wide row shows the whole refusal:\n{wide}"
    );
    assert!(wide.contains("d delete"), "and keeps the cursor's actions:\n{wide}");

    // Below a legible width the status drops rather than paint a lone `·` promising a message.
    let tiny = footer_line(&render_at(&app, 20));
    assert!(!tiny.contains("agent"), "no room for a legible message, so none is painted:\n{tiny}");
    assert!(tiny.contains("s send 1"), "send still never drops:\n{tiny}");

    // A truncated status never pushes the `?` off the right edge, at any width that fits row 1's
    // own fixed parts. Below 14 columns the shed primary and `send` overflow it on their own, with
    // no status in play at all.
    for w in 14..=140u16 {
        let row = footer_line(&render_at(&app, w));
        assert!(row.trim_end().ends_with('?'), "the `?` left the row at width {w}:\n{row}");
    }

    // `s` is also the comments list's primary, so a refusal has to reach the reviewer there too.
    // The list has no `?`, so its trailing `…` is the only promise the trimmed actions exist, and
    // the status leaves room for it (`specs/input.md`).
    app.open_list();
    let listed = footer_line(&render_at(&app, 40));
    assert!(listed.contains("no agent here"), "the refusal shows in the list at 40:\n{listed}");
    assert!(listed.contains("s send 1"), "send never drops in the list either:\n{listed}");
    assert!(listed.trim_end().ends_with('…'), "the trimmed actions keep their `…`:\n{listed}");
}

#[test]
fn the_expansion_aligns_row_one_into_the_labeled_grid() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.toggle_keys();
    let out = render(&app);
    // Search only the footer rows, so a stray `move`/`go` in the diff or file list can't stand in.
    let footer_start = ui::body_rect(Rect::new(0, 0, 140, 40), &app);
    let footer_start = (footer_start.y + footer_start.height) as usize;
    let line_of = |lbl: &str| {
        out.lines()
            .skip(footer_start)
            .find(|l| l.trim_start().starts_with(lbl))
            .unwrap_or("")
            .to_string()
    };
    let (do_line, go_line, move_line) = (line_of("do"), line_of("go"), line_of("move"));

    // Row 1 is now the `do` band: the primary, and the `?` still at the right.
    assert!(
        do_line.contains("c comment") && do_line.trim_end().ends_with('?'),
        "row 1 is the `do` line with the primary and `?`:\n{do_line}"
    );
    assert!(go_line.contains("scope"), "the go band lists the always-there keys:\n{go_line}");
    assert!(
        move_line.contains("hunk") && move_line.contains("file"),
        "the move band names the hunk and file steps:\n{move_line}"
    );
    // The three labels share one gutter column, and their content aligns in the next.
    let at = |l: &str, s: &str| l.find(s).expect("token present");
    assert_eq!(at(&do_line, "do"), at(&go_line, "go"), "labels share a gutter column");
    assert_eq!(at(&go_line, "go"), at(&move_line, "move"), "labels share a gutter column");
    assert_eq!(
        at(&do_line, "c comment"),
        at(&go_line, "u/b/t"),
        "the primary aligns under the same column as the band keys"
    );
    assert_eq!(at(&go_line, "u/b/t"), at(&move_line, "j k"), "band keys align in one column");
}

#[test]
fn the_collapsed_footer_stays_a_flush_action_bar() {
    let mut app = edited_app();
    on_changed_line(&mut app); // collapsed: no expansion
    let footer = footer_line(&render(&app));
    assert!(
        footer.trim_start().starts_with("c comment"),
        "no `do` gutter when collapsed:\n{footer}"
    );
    assert!(!footer.contains(" do "), "the `do` label appears only when expanded:\n{footer}");
}

#[test]
fn the_expanded_row_one_never_drops_send_or_the_more_hint_on_a_narrow_pane() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.start_comment();
    for ch in "n".chars() {
        app.input_push(ch);
    }
    app.submit_comment(); // a written comment puts `s send 1` on row 1
    app.toggle_keys(); // expanded — the fixed `do` gutter cannot shed
    for w in [14u16, 16, 18, 20, 22, 30] {
        let out = dump(&render_size(&app, w, 40));
        let row1 = out.lines().find(|l| l.contains("send")).expect("row 1 carries send");
        let row1 = row1.trim_end();
        assert!(row1.contains("s send 1"), "send survives at w={w}: [{row1}]");
        assert!(row1.ends_with('?'), "the `?` survives at w={w}: [{row1}]");
        assert!(row1.chars().count() <= w as usize, "row 1 never overflows at w={w}: [{row1}]");
    }
}

#[test]
fn the_expansion_caps_so_the_body_keeps_its_rows() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.toggle_keys();
    // On a short pane the wrapped bands would want more rows than fit, but the footer is capped so
    // the body keeps its Min(3) (specs/tui.md).
    let body = ui::body_rect(Rect::new(0, 0, 40, 6), &app);
    assert!(body.height >= 3, "the body keeps at least three rows: got {}", body.height);
}

#[test]
fn the_pr_footer_keeps_the_open_action_when_the_state_line_is_long() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Check, CheckStatus, Merge, PrSnapshot, PrView, Sync};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        number: 226,
        merge: Merge::Conflicting, // a long state line: conflicts · behind · failing · +more
        sync: Sync::Behind(3),
        checks: vec![Check { name: "ci".into(), status: CheckStatus::Failure }],
        truncated: true,
        ..common::pr_snapshot()
    }));
    // At narrow width the state line is capped so the primary `o open ↗` is never crowded off.
    let footer = footer_line(&render_at(&app, 60));
    assert!(footer.contains("o open"), "the open action survives a long state line:\n{footer}");
}

#[test]
fn pr_header_names_the_resolved_branch_and_marks_a_fork() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    let snap = |fork: bool| {
        PrView::Pr(Box::new(PrSnapshot {
            number: 226,
            head_ref: "persiyanov/feature".into(),
            head_is_fork: fork,
            ..common::pr_snapshot()
        }))
    };
    // The header shows the branch that resolved — it can differ from the local branch —
    // and marks a fork head, so a same-named fork PR is visible (specs/forge-host.md).
    app.pr = snap(false);
    let header = render(&app).lines().next().unwrap().to_string();
    assert!(header.contains("persiyanov/feature"), "resolved branch in the header:\n{header}");
    assert!(!header.contains('⑂'), "no fork marker on a same-repo head:\n{header}");
    app.pr = snap(true);
    let header = render(&app).lines().next().unwrap().to_string();
    assert!(header.contains("⑂ persiyanov/feature"), "fork head is marked:\n{header}");
    // Narrow bars drop the branch first; the chip's number stays.
    app.pr = snap(false);
    let narrow = render_at(&app, 46).lines().next().unwrap().to_string();
    assert!(!narrow.contains("persiyanov/feature"), "branch drops when narrow:\n{narrow}");
    assert!(narrow.contains("#226"), "the chip survives a narrow bar:\n{narrow}");

    let width = 80;
    let area = Rect::new(0, 0, width, 12);
    let header = render_at(&app, width).lines().next().unwrap().to_string();
    let chip_start = header.find("open #226").unwrap() as u16;
    let number = header.find("#226").unwrap() as u16;
    assert!(ui::hit_pr_open(area, &app, chip_start, 0));
    assert!(ui::hit_pr_open(area, &app, number, 0));
    assert!(!ui::hit_pr_open(area, &app, chip_start - 1, 0));
    assert!(!ui::hit_pr_open(area, &app, number, 1));
}

#[test]
fn pr_empty_states_are_calm() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::PrView;
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr = PrView::NoPr;
    let out = render(&app);
    assert!(
        out.contains("No pull request yet. Ready to ship?"),
        "ordinary absence stays brief:\n{out}"
    );
    app.pr = PrView::Detached;
    let out = render(&app);
    assert!(
        out.contains("No pull request found — HEAD is detached."),
        "detached wording stays factual:\n{out}"
    );
    app.pr = PrView::GitError("git remote get-url upstream failed".to_string());
    let out = render(&app);
    assert!(out.contains("Git read failed"), "local failures stay factual:\n{out}");
    assert!(!out.contains("GitHub unavailable"), "a local failure is not blamed on GitHub:\n{out}");
}

#[test]
fn the_footer_keeps_its_actions_alongside_a_status() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.status = "comment added".to_string();
    let footer = footer_line(&render(&app));
    // A status sits among the actions, never replacing them.
    assert!(footer.contains("comment added"), "the status shows:\n{footer}");
    assert!(
        footer.contains("c comment"),
        "the primary action persists alongside a status:\n{footer}"
    );
}

#[test]
fn empty_repo_shows_empty_states() {
    let r = Repo::init();
    r.write("seed.rs", "x\n");
    r.commit_all("init");
    let app = app_on(&r);

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
    let mut app = app_on(&r);
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
    let mut app = app_on(&r);
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
fn header_clicks_map_to_the_scope_chip() {
    let app = edited_app(); // scope uncommitted, no comments
    // Scan the header row instead of hardcoding columns, so the test survives changes
    // to the label text.
    let scope: Vec<u16> = (0..AREA.width)
        .filter(|&c| ui::hit_header(AREA, &app, app.keymap(), c, 0) == Some(HeaderHit::Scope))
        .collect();

    assert!(!scope.is_empty(), "scope chip is clickable");

    let gap = scope.iter().max().unwrap() + 1;
    assert_eq!(
        ui::hit_header(AREA, &app, app.keymap(), gap, 0),
        None,
        "the space right of the chip is inert"
    );
    assert_eq!(
        ui::hit_header(AREA, &app, app.keymap(), scope[0], 5),
        None,
        "only row 0 is the header"
    );
}

#[test]
fn file_and_diff_clicks_map_to_row_indices() {
    let app = edited_app();
    // Right pane: the first file row maps to index 0; clicking past the list misses.
    assert_eq!(ui::hit_file(AREA, &app, 120, 2, app.file_rows.len(), 0), Some(0));
    assert_eq!(ui::hit_file(AREA, &app, 120, 9, app.file_rows.len(), 0), None);
    // With the list scrolled down, the top visible row maps to that scrolled-to index.
    assert_eq!(ui::hit_file(AREA, &app, 120, 2, 50, 7), Some(7));
    assert_eq!(ui::hit_file(AREA, &app, 120, 3, 50, 7), Some(8));
    // The wheel routes by pointer: a column in the navigator is "in" the file list,
    // one in the read pane is not.
    assert!(ui::in_files_pane(AREA, &app, 120, 3));
    assert!(!ui::in_files_pane(AREA, &app, 10, 3));
    // Left pane: diff rows map top-down to diff-line indices.
    assert!(app.visible.len() > 1);
    let heights = ui::diff_row_heights(&app, AREA);
    assert_eq!(ui::hit_diff(AREA, &app, 10, 2, &heights, 0), Some(0));
    assert_eq!(ui::hit_diff(AREA, &app, 10, 3, &heights, 0), Some(1));
    // With a nonzero scroll and wrapped (multi-row) lines, the click must skip the
    // scrolled-off rows and account for each visible row's display height. Rows are
    // 2 tall each; diff_scroll=1 puts row index 1 at the top of the pane (inner.y == 2).
    let tall = [2usize, 2, 2, 2];
    assert_eq!(ui::hit_diff(AREA, &app, 10, 2, &tall, 1), Some(1)); // top visible row
    assert_eq!(ui::hit_diff(AREA, &app, 10, 3, &tall, 1), Some(1)); // its second display row
    assert_eq!(ui::hit_diff(AREA, &app, 10, 4, &tall, 1), Some(2)); // next logical row
}

#[test]
fn navigator_layout_rects_cover_every_position_and_tiny_axis() {
    let mut app = edited_app();
    let body = ui::body_rect(AREA, &app);

    for position in [
        NavigatorPosition::Right,
        NavigatorPosition::Bottom,
        NavigatorPosition::Left,
        NavigatorPosition::Top,
    ] {
        app.navigator_position = position;
        let _ = render_size(&app, AREA.width, AREA.height);
        let app_ref = &app;
        let files: Vec<(u16, u16)> = (body.y..body.y + body.height)
            .flat_map(|row| {
                (body.x..body.x + body.width)
                    .filter(move |&col| ui::in_files_pane(AREA, app_ref, col, row))
                    .map(move |col| (col, row))
            })
            .collect();
        let diff: Vec<(u16, u16)> = (body.y..body.y + body.height)
            .flat_map(|row| {
                (body.x..body.x + body.width)
                    .filter(move |&col| ui::in_diff_pane(AREA, app_ref, col, row))
                    .map(move |col| (col, row))
            })
            .collect();
        let files_x = (
            files.iter().map(|&(x, _)| x).min().unwrap(),
            files.iter().map(|&(x, _)| x).max().unwrap(),
        );
        let files_y = (
            files.iter().map(|&(_, y)| y).min().unwrap(),
            files.iter().map(|&(_, y)| y).max().unwrap(),
        );
        let diff_x = (
            diff.iter().map(|&(x, _)| x).min().unwrap(),
            diff.iter().map(|&(x, _)| x).max().unwrap(),
        );
        let diff_y = (
            diff.iter().map(|&(_, y)| y).min().unwrap(),
            diff.iter().map(|&(_, y)| y).max().unwrap(),
        );
        assert_eq!(files.len() + diff.len(), usize::from(body.width * body.height));
        assert!(!files.is_empty() && !diff.is_empty());
        assert!(
            (body.y..body.y + body.height).any(|row| {
                (body.x..body.x + body.width).any(|col| ui::hit_divider(AREA, &app, col, row))
            }),
            "divider is hittable for {position:?}"
        );
        match position {
            NavigatorPosition::Right => {
                assert!(files.iter().map(|(x, _)| x).min() > diff.iter().map(|(x, _)| x).min());
                assert_eq!(files.len() / usize::from(body.height), 44);
                assert!(!ui::hit_divider(AREA, &app, files_x.0 + 1, body.y + 4));
                assert!(!ui::hit_divider(AREA, &app, diff_x.1 - 1, body.y + 4));
            }
            NavigatorPosition::Left => {
                assert!(files.iter().map(|(x, _)| x).min() < diff.iter().map(|(x, _)| x).min());
                assert_eq!(files.len() / usize::from(body.height), 44);
                assert!(!ui::hit_divider(AREA, &app, files_x.1 - 1, body.y + 4));
                assert!(!ui::hit_divider(AREA, &app, diff_x.0 + 1, body.y + 4));
            }
            NavigatorPosition::Bottom => {
                assert!(files.iter().map(|(_, y)| y).min() > diff.iter().map(|(_, y)| y).min());
                assert_eq!(files.len() / usize::from(body.width), 9);
                assert!(!ui::hit_divider(AREA, &app, body.x + 4, files_y.0 + 1));
                assert!(!ui::hit_divider(AREA, &app, body.x + 4, diff_y.1 - 1));
            }
            NavigatorPosition::Top => {
                assert!(files.iter().map(|(_, y)| y).min() < diff.iter().map(|(_, y)| y).min());
                assert_eq!(files.len() / usize::from(body.width), 9);
                assert!(!ui::hit_divider(AREA, &app, body.x + 4, files_y.1 - 1));
                assert!(!ui::hit_divider(AREA, &app, body.x + 4, diff_y.0 + 1));
            }
        }
    }

    app.navigator_position = NavigatorPosition::Right;
    let six = Rect::new(0, 0, 6, 10);
    let row = ui::body_rect(six, &app).y;
    assert_eq!((0..6).filter(|&col| ui::in_files_pane(six, &app, col, row)).count(), 3);
    assert_eq!((0..6).filter(|&col| ui::in_diff_pane(six, &app, col, row)).count(), 3);

    let five = Rect::new(0, 0, 5, 10);
    let row = ui::body_rect(five, &app).y;
    assert_eq!((0..5).filter(|&col| ui::in_files_pane(five, &app, col, row)).count(), 2);
    assert_eq!((0..5).filter(|&col| ui::in_diff_pane(five, &app, col, row)).count(), 3);

    app.navigator_position = NavigatorPosition::Top;
    let eight = Rect::new(0, 0, 10, 10); // body height 8
    let col = ui::body_rect(eight, &app).x;
    assert_eq!((1..9).filter(|&row| ui::in_files_pane(eight, &app, col, row)).count(), 3);
    assert_eq!((1..9).filter(|&row| ui::in_diff_pane(eight, &app, col, row)).count(), 5);

    let seven = Rect::new(0, 0, 10, 7); // body height 5: navigator gets floor(5 / 2)
    let col = ui::body_rect(seven, &app).x;
    assert_eq!((1..6).filter(|&row| ui::in_files_pane(seven, &app, col, row)).count(), 2);
    assert_eq!((1..6).filter(|&row| ui::in_diff_pane(seven, &app, col, row)).count(), 3);
}

#[test]
fn pr_focus_border_tracks_tab_between_navigator_and_read_pane() {
    let mut app = edited_app();
    app.set_tab(Tab::Pr).unwrap();
    app.focus = Focus::Files;
    let body = ui::body_rect(AREA, &app);
    let nav_x = (body.x..body.x + body.width)
        .find(|&x| ui::in_files_pane(AREA, &app, x, body.y + 4))
        .unwrap();
    let read_x = (body.x..body.x + body.width)
        .find(|&x| ui::in_diff_pane(AREA, &app, x, body.y + 4))
        .unwrap();
    let (lavender, surface2) = (app.palette().lavender, app.palette().surface2);

    let focused_nav = render_buffer(&app);
    assert_eq!(focused_nav.cell((nav_x, body.y + 4)).unwrap().fg, lavender);
    assert_eq!(focused_nav.cell((read_x, body.y + 4)).unwrap().fg, surface2);

    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), AREA, &Keymap::default())
        .unwrap();
    let focused_read = render_buffer(&app);
    assert_eq!(focused_read.cell((nav_x, body.y + 4)).unwrap().fg, surface2);
    assert_eq!(focused_read.cell((read_x, body.y + 4)).unwrap().fg, lavender);
}

#[test]
fn a_zero_height_pr_navigator_does_not_consume_selection_reveal() {
    use herdr_reviewr::forge::{Comment, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.navigator_position = NavigatorPosition::Top;
    let comments = (0..30)
        .map(|i| Comment { author: format!("author-{i:02}"), ..common::comment() })
        .collect();
    app.pr = PrView::Pr(Box::new(PrSnapshot { comments, ..common::pr_snapshot() }));
    app.pr_move(10);

    let _ = render_size(&app, 80, 3); // the top navigator has no inner viewport
    let useful = dump(&render_size(&app, 80, 40));

    assert!(useful.contains("@author-10"), "the pending reveal survives the tiny frame:\n{useful}");
}

#[test]
fn a_loading_pr_navigator_does_not_consume_selection_reveal() {
    use herdr_reviewr::forge::{Check, CheckStatus, Comment, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.clear_pr();
    let _ = render(&app); // a useful viewport, but no selected row yet

    let checks = (0..20)
        .map(|i| Check { name: format!("check-{i:02}"), status: CheckStatus::Success })
        .collect();
    let comments = vec![Comment { author: "selected".into(), ..common::comment() }];
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot { checks, comments, ..common::pr_snapshot() })));
    let populated = render(&app);

    assert!(populated.contains("@selected"), "the first selectable row is revealed:\n{populated}");
}

#[test]
fn a_binary_file_shows_the_no_line_comments_message() {
    let r = Repo::init();
    r.write("logo.bin", "\0\0\0\0seed\0\0");
    r.commit_all("init");
    r.write("logo.bin", "\0\0\0\0changed\0\0\0");
    let mut app = app_on(&r);
    let idx = app.entries.iter().position(|f| f.path == "logo.bin").expect("binary file listed");
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
    let mut app = app_on(&r);
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
fn last_turn_without_an_agent_says_the_worktree_is_empty() {
    // `specs/herdr-host.md` owns when membership counts as observed; `specs/tui.md` owns the
    // wording. Only a sample that found no member may say the worktree is empty.
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.reload().unwrap();
    app.sync_agents_present(Some(false));
    let out = render(&app);
    assert!(out.contains("[last turn]"), "the scope chip reads last turn");
    assert!(out.contains("no agent works here"), "the empty-worktree state shows");
}

#[test]
fn last_turn_with_an_agent_and_no_turn_yet_waits_for_the_first() {
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.reload().unwrap();
    app.sync_agents_present(Some(true));
    let out = render(&app);
    assert!(out.contains("waiting for the first turn"), "the pre-turn state shows");
}

#[test]
fn last_turn_before_the_first_sample_waits_rather_than_asserting_emptiness() {
    // The pre-poll frame has observed nothing, so it may wait but not claim the worktree
    // is empty — stale is allowed, wrong is not (`specs/overview.md` Continuity).
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.reload().unwrap();
    let out = render(&app);
    assert!(out.contains("waiting for the first turn"), "the unknown state waits");
}

#[test]
fn all_files_tab_bar_footer_and_count_read_for_the_tab() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    r.write("a.rs", "ONE\n"); // one change
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);

    let out = render(&app);
    assert!(out.contains("1 Changes"), "tab labels carry their switch digit:\n{out}");
    assert!(out.contains("2 Files"));
    assert!(
        out.contains("1 changed"),
        "the changed count stays in the header on All files:\n{out}"
    );
    let footer = footer_line(&out);
    assert!(
        footer.trim_end().ends_with('?'),
        "the collapsed footer closes with the `?`:\n{footer}"
    );
    assert!(
        !footer.contains("changed"),
        "the changed count is not repeated in the footer:\n{footer}"
    );
    // `scope` is a `go` key now, revealed by the `?` expansion rather than crowding row 1.
    app.toggle_keys();
    let expanded = render(&app);
    assert!(expanded.contains("scope"), "the `?` expansion lists the scope keys:\n{expanded}");
    assert!(expanded.contains("move"), "and labels the movement band:\n{expanded}");
}

#[test]
fn all_files_empty_pane_reads_select_a_file() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("src/a.rs", "x\n");
    r.write("src/b.rs", "y\n"); // two children so src/ is a real collapsed dir, not a folded file
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles); // clean repo: no seed; cursor rests on collapsed src/

    let out = render(&app);
    assert!(out.contains("select a file to read"), "the empty All files read-pane copy:\n{out}");
    assert!(!out.contains("no diff"), "no diff vocabulary in the content browser:\n{out}");
}

#[test]
fn renders_a_light_theme_without_panic() {
    let mut app = edited_app();
    app.set_cli_theme(Some("catppuccin-latte".to_string()));
    // Driving the full render path with a derived light palette must not panic, and a Latte
    // color (the focused pane's lavender border) reaches the painted buffer.
    let buf = render_buffer(&app);
    let latte_lavender = herdr_reviewr::theme::resolve(Some("catppuccin-latte")).palette.lavender;
    let painted = (0..40)
        .flat_map(|y| (0..140).map(move |x| (x, y)))
        .any(|(x, y)| buf.cell((x, y)).is_some_and(|c| c.fg == latte_lavender));
    assert!(painted, "the Latte palette reaches the painted buffer");
}

/// An `edited_app` running under `[keybindings]` from a real config file.
fn rebound_app(keybindings: &str) -> App {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), format!("[keybindings]\n{keybindings}"))
        .unwrap();
    let config = herdr_reviewr::config::plugin_config_in(dir.path()).unwrap();
    let mut app = edited_app();
    app.set_plugin_config(config);
    app.focus = Focus::Diff;
    app
}

#[test]
fn hints_show_the_first_bound_key() {
    let app = rebound_app("comment = [\"ㅊ\", \"c\"]\ntab-pr = [\"g\"]\n");
    let out = render(&app);
    let footer = footer_line(&out);
    // A wide hint key spans two buffer cells, so the dump carries a placeholder space after it.
    assert!(footer.contains("ㅊ  comment"), "the hint is the first bound key:\n{footer}");
    assert!(out.contains("g PR"), "the header tab hint follows its binding:\n{out}");
    assert!(!out.contains("3 PR"), "the replaced digit is gone:\n{out}");
}

/// The header columns `hit_header` maps to `tab` under `keymap`, scanned instead of hardcoded
/// so the tests survive changes to the label text and gaps.
fn tab_hit_cols(app: &App, keymap: &herdr_reviewr::keymap::Keymap, tab: Tab) -> Vec<u16> {
    let area = Rect::new(0, 0, 140, 40);
    (0..140)
        .filter(|&c| ui::hit_header(area, app, keymap, c, 0) == Some(HeaderHit::Tab(tab)))
        .collect()
}

#[test]
fn header_hits_use_the_frame_keymap_not_the_live_one() {
    use herdr_reviewr::keymap::default_keymap;
    // The live keymap has a wide tab-changes hint, shifting every span right by one column.
    let app = rebound_app("tab-changes = [\"ㅊ\"]\n");
    for tab in [Tab::Changes, Tab::AllFiles, Tab::Pr] {
        assert_ne!(
            tab_hit_cols(&app, default_keymap(), tab),
            tab_hit_cols(&app, app.keymap(), tab),
            "the passed frame keymap decides the spans, not the app's live one"
        );
    }
}

#[test]
fn header_tab_hits_align_with_wide_hint_keys() {
    let app = rebound_app("tab-changes = [\"ㅊ\"]\n");
    let out = render(&app);
    // The wide hint spans two buffer cells, so the dump shows a placeholder space after it.
    assert!(out.contains("ㅊ  Changes"), "the wide hint renders:\n{out}");
    // Each rendered label must be clickable at its own drawn column (one dumped char per cell,
    // so the char offset of the label in row 0 is its column).
    let row0 = out.lines().next().unwrap().to_string();
    let col_of = |needle: &str| row0[..row0.find(needle).unwrap()].chars().count() as u16;
    let area = Rect::new(0, 0, 140, 40);
    for (needle, tab) in [("Changes", Tab::Changes), ("2 Files", Tab::AllFiles), ("3 PR", Tab::Pr)]
    {
        assert_eq!(
            ui::hit_header(area, &app, app.keymap(), col_of(needle), 0),
            Some(HeaderHit::Tab(tab)),
            "the drawn {needle:?} label answers its own click"
        );
    }
}

#[test]
fn the_markdown_preview_renders_styled_lines_without_a_gutter() {
    let r = Repo::init();
    r.write("README.md", "# Install\n\nRun `cargo test` for **all** checks.\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);

    // Source view: raw markdown, and the footer surfaces the way into the preview.
    app.focus = Focus::Diff;
    let source = render(&app);
    assert!(source.contains("# Install"), "source shows raw markdown:\n{source}");
    let footer = source.lines().last().unwrap();
    assert!(footer.contains("m preview"), "source discovers the preview:\n{footer}");

    app.toggle_preview();
    let out = render(&app);
    assert!(out.contains("Install"), "the heading text renders:\n{out}");
    assert!(!out.contains("# Install"), "the # markers are gone in the preview:\n{out}");
    assert!(!out.contains("**all**"), "emphasis markers are consumed:\n{out}");
    assert!(!out.contains("  1 "), "the preview has no line-number gutter:\n{out}");
    let footer = out.lines().last().unwrap();
    assert!(footer.contains("m source"), "the footer leads back to source:\n{footer}");
    assert!(!footer.contains("c comment"), "no comment key in the preview:\n{footer}");
}

#[test]
fn a_deleted_markdown_file_offers_no_preview_in_the_footer() {
    let r = Repo::init();
    r.write("gone.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.remove("gone.md");
    let mut app = app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("gone.md"));
    app.focus = Focus::Diff;

    // The deletion rows are commentable, but a deleted file has no current content, so
    // the footer never offers the inert preview toggle (specs/input.md).
    let out = render(&app);
    let footer = out.lines().last().unwrap();
    assert!(footer.contains("c comment"), "a deletion row is commentable:\n{footer}");
    assert!(!footer.contains("m preview"), "a deleted file offers no preview:\n{footer}");
}

#[test]
fn pr_bodies_render_as_markdown_and_the_description_row_pins_first() {
    use herdr_reviewr::forge::{Comment, CommentKind, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    let finding = Comment {
        kind: CommentKind::Finding,
        author: "codex".into(),
        author_is_bot: true,
        anchor: "x.rs:1".into(),
        body: "Avoid **panics** in `parse`.".into(),
        snippet: Some("-    old\n+    new".into()),
        ..common::comment()
    };
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        number: 226,
        body: "## Summary\nThis PR adds *markdown*.".into(),
        comments: vec![finding],
        ..common::pr_snapshot()
    }));

    // The cursor starts on the pinned description row; its body renders as markdown.
    let out = render(&app);
    assert!(out.contains("description"), "the description row shows:\n{out}");
    assert!(out.contains("Summary"), "the description heading renders:\n{out}");
    assert!(!out.contains("## Summary"), "markers are consumed:\n{out}");
    assert!(!out.contains("*markdown*"), "emphasis markers are consumed:\n{out}");

    // The navigator orders the PR itself first: description above checks above comments.
    let nav = right_column(&out, 68);
    let desc_at = nav.find("description").expect("description row in the nav");
    let checks_at = nav.find("checks").expect("checks section in the nav");
    let comments_at = nav.find("comments ·").expect("comments header in the nav");
    assert!(desc_at < checks_at && checks_at < comments_at, "nav order:\n{nav}");

    // The finding: the snippet stays plain +/− lines, the body renders as markdown.
    app.pr_move(1);
    let out = render(&app);
    assert!(out.contains("+    new"), "the diff hunk stays plain:\n{out}");
    assert!(out.contains("Avoid panics in parse."), "the body renders styled:\n{out}");
    assert!(!out.contains("**panics**"), "markers are consumed:\n{out}");
}

#[test]
fn pr_nav_clicks_map_the_description_and_comment_rows() {
    use herdr_reviewr::forge::{Check, CheckStatus, Comment, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    let comment = |author: &str| Comment { author: author.into(), ..common::comment() };
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        body: "the description".into(),
        checks: vec![Check { name: "ci".into(), status: CheckStatus::Success }],
        comments: vec![comment("ann"), comment("bob")],
        ..common::pr_snapshot()
    }));

    // Nav layout: description, blank, checks header, 1 check, blank, comments header,
    // then the comments. The nav inner starts one row under the tab bar's border.
    let area = Rect::new(0, 0, 140, 40);
    let x = 130; // inside the nav pane
    assert_eq!(ui::pr_nav_hit(area, &app, x, 2), Some(0), "click on the description row");
    assert_eq!(ui::pr_nav_hit(area, &app, x, 5), None, "a check row is not a cursor stop");
    assert_eq!(ui::pr_nav_hit(area, &app, x, 8), Some(1), "first comment maps past the offset");
    assert_eq!(ui::pr_nav_hit(area, &app, x, 9), Some(2), "second comment follows");
    assert_eq!(ui::pr_nav_hit(area, &app, x, 10), None, "past the last comment is dead");
}

#[test]
fn pr_navigator_scroll_is_independent_and_preserved() {
    use herdr_reviewr::forge::{Check, CheckStatus, Comment, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.navigator_position = NavigatorPosition::Bottom;
    let checks: Vec<Check> = (0..14)
        .map(|i| Check { name: format!("check-{i:02}"), status: CheckStatus::Success })
        .collect();
    let comments: Vec<Comment> = (0..8)
        .map(|i| Comment {
            author: format!("author-{i:02}"),
            body: (0..50).map(|line| format!("line-{line:02}")).collect::<Vec<_>>().join("  \n"),
            ..common::comment()
        })
        .collect();
    let snapshot = || PrSnapshot {
        checks: checks.clone(),
        comments: comments.clone(),
        ..common::pr_snapshot()
    };
    app.pr = PrView::Pr(Box::new(snapshot()));

    let selected = app.pr_selected_comment().map(|c| c.author.clone());
    let area = Rect::new(0, 0, 140, 40);
    let body = ui::body_rect(area, &app);
    let (column, row) = (body.y..body.y + body.height)
        .flat_map(|row| (body.x..body.x + body.width).map(move |column| (column, row)))
        .find(|&(column, row)| ui::in_files_pane(area, &app, column, row))
        .unwrap();
    let keymap = Keymap::default();
    let _ = render(&app); // establishes the navigator's scroll bound
    for _ in 0..10 {
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            area,
            &[],
            &keymap,
        )
        .unwrap();
    }
    let before = render(&app);
    assert!(before.contains("check-00"));
    assert!(!before.contains("check-13"));
    for _ in 0..5 {
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            area,
            &[],
            &keymap,
        )
        .unwrap();
    }
    let scrolled = render(&app);
    assert!(scrolled.contains("@author-00"), "the wheel exposes overflowed comments:\n{scrolled}");
    assert_eq!(app.pr_selected_comment().map(|c| c.author.clone()), selected);

    let (clicked_row, clicked_author) = scrolled
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            comments
                .iter()
                .find(|comment| line.contains(&format!("@{}", comment.author)))
                .map(|comment| (row as u16, comment.author.clone()))
        })
        .expect("a scrolled comment row is painted");
    handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
            column: body.x + 2,
            row: clicked_row,
            modifiers: KeyModifiers::NONE,
        },
        area,
        &[],
        &keymap,
    )
    .unwrap();
    assert_eq!(app.pr_selected_comment().map(|c| c.author.as_str()), Some(clicked_author.as_str()));

    app.apply_pr(PrView::Pr(Box::new(snapshot())));
    let refetched = render(&app);
    assert!(refetched.contains("@author-00"), "a refetch preserves navigator scroll:\n{refetched}");

    app.focus = Focus::Files;
    handle_key(&mut app, KeyEvent::from(KeyCode::PageUp), area, &keymap).unwrap();
    let paged = render(&app);
    assert!(paged.contains("check-00"), "page keys scroll the focused navigator:\n{paged}");
    assert_eq!(app.pr_selected_comment().map(|c| c.author.as_str()), Some(clicked_author.as_str()));

    handle_key(&mut app, KeyEvent::from(KeyCode::Tab), area, &keymap).unwrap();
    let nav_before_read_page = render(&app);
    assert!(nav_before_read_page.contains("line-00"));
    handle_key(&mut app, KeyEvent::from(KeyCode::PageDown), area, &keymap).unwrap();
    let read_paged = render(&app);
    assert!(!read_paged.contains("line-00"), "the focused read pane leaves its first line");
    assert!(read_paged.contains("line-20"), "PageDown advances the PR read body:\n{read_paged}");
    assert!(read_paged.contains("check-00"), "read paging leaves navigator paging unchanged");
}

#[test]
fn the_refresh_glyph_lives_in_the_tab_strip_not_the_content() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr =
        PrView::Pr(Box::new(PrSnapshot { body: "steady content".into(), ..common::pr_snapshot() }));

    let steady = render(&app);
    let row_of = |out: &str, needle: &str| {
        out.lines().position(|l| l.contains(needle)).unwrap_or(usize::MAX)
    };
    let before = row_of(&steady, "steady content");
    assert!(!steady.contains('⟳'), "the reserved cell is blank while idle");

    // The reserved cell means the glyph's appearance shifts nothing (specs/tui.md).
    app.refresh_indicator = true;
    let refreshing = render(&app);
    let header = refreshing.lines().next().unwrap();
    assert!(header.contains('⟳'), "the glyph shows in the tab strip:\n{header}");
    assert_eq!(
        row_of(&refreshing, "steady content"),
        before,
        "a refetch never shifts the content the reader is on"
    );
    assert_eq!(
        steady.lines().next().unwrap().replace(' ', "").len(),
        header.replace(' ', "").len() - '⟳'.len_utf8(),
        "the glyph fills the reserved blank cell instead of inserting one"
    );
}

#[test]
fn a_retry_notice_stays_visible_above_a_scrolled_pr_body() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.focus = Focus::Diff;
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        body: (0..80).map(|line| format!("line-{line:02}")).collect::<Vec<_>>().join("  \n"),
        ..common::pr_snapshot()
    }));

    let area = Rect::new(0, 0, 140, 40);
    let _ = render(&app);
    handle_key(&mut app, KeyEvent::from(KeyCode::PageDown), area, &Keymap::default()).unwrap();
    let scrolled = render(&app);
    assert!(!scrolled.contains("line-00"), "the setup scrolls away from the top");

    app.apply_pr(PrView::GitError("git rev-parse HEAD failed".to_string()));
    let failed = render(&app);
    assert!(failed.contains("Git read failed"), "the recovery action remains visible:\n{failed}");
    assert!(!failed.contains("line-00"), "showing the notice does not reset the reader");
}

#[test]
fn a_gitlab_repository_renders_merge_request_nouns_and_remedies() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    use herdr_reviewr::git::Forge;
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr_forge = Forge::GitLab;

    // The empty state speaks the forge's noun (`specs/forge-providers.md`).
    app.apply_pr(PrView::NoPr);
    let out = render(&app);
    assert!(out.contains("No merge request yet"), "GitLab empty state:\n{out}");

    // The chip uses GitLab's reference form.
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot { number: 42, ..common::pr_snapshot() })));
    let out = render(&app);
    assert!(out.contains("!42"), "MR reference form:\n{out}");
    assert!(!out.contains("#42"), "no GitHub reference form on GitLab:\n{out}");

    // Each failure names its own CLI and login command (`specs/forge-host.md`).
    app.apply_pr(PrView::NoCli(Forge::GitLab));
    let out = render(&app);
    assert!(out.contains("Install `glab`"), "glab install step:\n{out}");
    app.apply_pr(PrView::NotAuthed(Forge::GitLab, "git.corp.example".to_string()));
    let out = render(&app);
    assert!(out.contains("glab auth login --hostname git.corp.example"), "login remedy:\n{out}");
}

#[test]
fn an_unsupported_host_points_at_the_per_forge_host_keys() {
    use herdr_reviewr::forge::PrView;
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.apply_pr(PrView::UnsupportedHost("code.corp.example".to_string()));
    let out = render(&app);
    assert!(out.contains("code.corp.example"), "the host is named:\n{out}");
    assert!(out.contains("github_host"), "GitHub key offered:\n{out}");
    assert!(out.contains("gitlab_host"), "GitLab key offered:\n{out}");
    assert!(out.contains("azure_devops_host"), "Azure DevOps key offered:\n{out}");
}

#[test]
fn an_azure_devops_repository_renders_pr_nouns_and_remedies() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    use herdr_reviewr::git::Forge;
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr_forge = Forge::AzureDevOps;

    // The empty state speaks the forge's noun (`specs/forge-providers.md`).
    app.apply_pr(PrView::NoPr);
    let out = render(&app);
    assert!(out.contains("No pull request yet"), "Azure DevOps empty state:\n{out}");

    // The chip uses the `#` reference form.
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot { number: 12, ..common::pr_snapshot() })));
    let out = render(&app);
    assert!(out.contains("#12"), "PR reference form:\n{out}");

    // Each failure names its own CLI, extension, and login command (`specs/forge-host.md`).
    app.apply_pr(PrView::NoCli(Forge::AzureDevOps));
    let out = render(&app);
    assert!(out.contains("Install `az`"), "az install step:\n{out}");
    app.apply_pr(PrView::NoExtension(Forge::AzureDevOps));
    let out = render(&app);
    assert!(out.contains("az extension add --name azure-devops"), "extension install step:\n{out}");
    app.apply_pr(PrView::NotAuthed(Forge::AzureDevOps, "dev.azure.com".to_string()));
    let out = render(&app);
    assert!(out.contains("`az login`"), "login remedy:\n{out}");
    assert!(out.contains("az devops login"), "the PAT alternative is offered:\n{out}");
}

#[test]
fn a_short_narrow_pr_pane_keeps_the_retry_action_and_one_body_row() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr =
        PrView::Pr(Box::new(PrSnapshot { body: "steady body".into(), ..common::pr_snapshot() }));
    app.apply_pr(PrView::NotAuthed(
        herdr_reviewr::git::Forge::GitHub,
        "github.example.com".to_string(),
    ));

    let out = dump(&render_size(&app, 30, 7));
    assert!(out.contains("Not signed"), "the failure state remains visible:\n{out}");
    assert!(out.contains("press r"), "the actionable tail remains visible:\n{out}");
    assert!(out.contains("steady body"), "the preserved snapshot keeps one readable row:\n{out}");
}

#[test]
fn markdown_links_paint_click_regions_and_the_guard_gates_them() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        body: "see [the run](https://ci.example/1)".into(),
        ..common::pr_snapshot()
    }));

    let _ = render(&app);
    let hit = first_painted_link(&app);
    assert_eq!(hit.as_deref(), Some("https://ci.example/1"), "a painted region resolves");

    // The guard gates what a click can open; a refused destination is silently inert.
    app.status.clear();
    app.open_link("javascript:alert(1)");
    assert_eq!(app.status, "", "an unsupported scheme does nothing");
    app.open_link("https://a\u{202e}b");
    assert_eq!(app.status, "", "a bidi-carrying destination does nothing");
    app.open_link("#no-such-anchor");
    assert_eq!(app.status, "", "a missing anchor does nothing");
}

#[test]
fn an_anchor_click_scrolls_the_preview_to_its_heading() {
    let mut md = String::from(
        "# Top

jump [go](#section-two)

",
    );
    for i in 0..40 {
        use std::fmt::Write as _;
        let _ = write!(md, "filler paragraph {i}\n\n");
    }
    md.push_str(
        "## Section Two

the target body
",
    );
    let r = Repo::init();
    r.write("doc.md", &md);
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);

    // In source view an anchor click is inert: no anchors are painted there.
    let _ = render(&app);
    app.open_link("#section-two");
    assert_eq!(app.preview_scroll, 0, "source view ignores anchor destinations");

    app.toggle_preview();
    let _ = render(&app); // paint: anchors and link regions note themselves

    assert_eq!(app.preview_scroll, 0);
    app.open_link("#section-two");
    assert!(app.preview_scroll > 40, "the preview jumped to the heading: {}", app.preview_scroll);
    let out = render(&app);
    assert!(
        out.contains("Section Two"),
        "the heading is on screen:
{out}"
    );
    assert!(
        !out.contains("# Top"),
        "the top scrolled away:
{out}"
    );
    assert!(out.contains('┃'), "an overflowing preview shows the scrollbar thumb:\n{out}");
}

#[test]
fn a_body_that_fits_the_pane_shows_no_scrollbar() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    use std::fmt::Write as _;
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr =
        PrView::Pr(Box::new(PrSnapshot { body: "one short line".into(), ..common::pr_snapshot() }));
    let out = render(&app);
    assert!(!out.contains('┃'), "content that fits paints no thumb:\n{out}");

    // The same pane paints the thumb once its body overflows, so the absence above
    // proves fitting content, not a dead scrollbar.
    let mut long = String::new();
    for i in 0..80 {
        let _ = writeln!(long, "line {i}\n");
    }
    app.pr = PrView::Pr(Box::new(PrSnapshot { body: long, ..common::pr_snapshot() }));
    let out = render(&app);
    assert!(out.contains('┃'), "an overflowing PR body shows the thumb:\n{out}");
}

#[test]
fn the_preview_paints_link_regions_and_names_itself_in_the_title() {
    let r = Repo::init();
    r.write("README.md", "# Install\n\nsee [docs](https://docs.example/x)\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);

    let source = render(&app);
    assert!(!source.contains("· preview"), "source view has no preview marker");
    let miss = first_painted_link(&app);
    assert_eq!(miss, None, "raw source paints no link regions");

    app.toggle_preview();
    let out = render(&app);
    assert!(out.contains("README.md · preview"), "the title names the mode:\n{out}");
    let hit = first_painted_link(&app);
    assert_eq!(hit.as_deref(), Some("https://docs.example/x"));
}

#[test]
fn the_changes_tab_paints_the_markdown_preview() {
    let r = Repo::init();
    r.write("README.md", "# Install\n");
    r.commit_all("init");
    r.write("README.md", "# Install\n\nRun `cargo test` for **all** checks.\n");
    let mut app = app_on(&r);
    app.focus = Focus::Diff;

    // The Changes diff shows raw markdown, and the footer surfaces the way into the preview.
    let source = render(&app);
    assert!(source.contains("# Install"), "the diff shows raw markdown:\n{source}");
    let footer = source.lines().last().unwrap();
    assert!(footer.contains("m preview"), "the diff discovers the preview:\n{footer}");

    // The toggle paints the rendered document over the diff and names the mode in the title.
    app.toggle_preview();
    let out = render(&app);
    assert!(out.contains("README.md · preview"), "the title names the mode:\n{out}");
    assert!(out.contains("Install"), "the heading text renders:\n{out}");
    assert!(!out.contains("# Install"), "the # markers are gone in the preview:\n{out}");
    // "checks" is on the new side only (the committed side is the bare heading), so this
    // proves the preview renders current content, not the old version being diffed.
    assert!(out.contains("checks"), "the preview renders the new-side content:\n{out}");
    let footer = out.lines().last().unwrap();
    assert!(footer.contains("m source"), "the footer leads back to the diff:\n{footer}");
}

#[test]
fn an_uppercase_unicode_anchor_still_finds_its_heading() {
    use std::fmt::Write as _;
    let mut md = String::from("# Über Top\n\njump [go](#ÜBER-TOP)\n\n");
    for i in 0..40 {
        let _ = writeln!(md, "filler {i}\n");
    }
    md.push_str("## Über Ziel\n\nend\n");
    let r = Repo::init();
    r.write("doc.md", &md);
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.toggle_preview();
    let _ = render(&app);

    // The click side must Unicode-lowercase like the slugger: #ÜBER-ZIEL → über-ziel.
    app.open_link("#ÜBER-ZIEL");
    assert!(app.preview_scroll > 40, "the jump matched the slug: {}", app.preview_scroll);
}

#[test]
fn an_anchor_in_a_comment_body_jumps_past_the_snippet_offset() {
    use herdr_reviewr::forge::{Comment, CommentKind, PrSnapshot, PrView};
    use std::fmt::Write as _;
    let mut body = String::from("jump [go](#target)\n\n");
    for i in 0..60 {
        let _ = writeln!(body, "line {i}\n");
    }
    body.push_str("## Target\n\nTARGET-BODY\n");
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        comments: vec![Comment {
            kind: CommentKind::Finding,
            author: "codex".into(),
            author_is_bot: true,
            anchor: "x.rs:1".into(),
            body,
            snippet: Some("-    old\n+    new".into()),
            ..common::comment()
        }],
        ..common::pr_snapshot()
    }));
    let out = render(&app);
    assert!(out.contains("+    new"), "the snippet paints above the body:\n{out}");

    // The anchor stores its content line snippet-offset included, so the jump lands on
    // the heading, scrolling the snippet and the body's top out of view.
    app.open_link("#target");
    let out = render(&app);
    assert!(out.contains("Target"), "the heading is on screen:\n{out}");
    assert!(!out.contains("+    new"), "the snippet scrolled away:\n{out}");
    assert!(!out.contains("jump go"), "the body's top scrolled away:\n{out}");
}

// In-file find rendering (specs/find-in-file.md).
#[test]
fn the_find_band_and_match_highlight_paint() {
    let r = Repo::init();
    r.write("base.txt", "x\n");
    r.commit_all("init");
    r.write("m.rs", "let total = 1;\ncompute();\ntotal += 2;\n");
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = 0; // the first "total" row
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 140, 40);

    handle_key(&mut app, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), area, &keymap)
        .unwrap();
    for ch in "total".chars() {
        handle_key(&mut app, KeyEvent::from(KeyCode::Char(ch)), area, &keymap).unwrap();
    }

    let buf = render_buffer(&app);
    let out = dump(&buf);
    // The band carries the label, the query, and the count: two matches, the cursor on the first.
    assert!(out.contains("find"), "the band shows the find label:\n{out}");
    assert!(out.contains("1/2"), "the band shows the cursor's ordinal over the total:\n{out}");

    // A matched character reverses to the bright fill with dark text, so it reads over any row.
    let fill = app.palette().yellow;
    let ink = app.palette().surface0;
    let highlighted = (0..40u16).flat_map(|y| (0..140u16).map(move |x| (x, y))).any(|(x, y)| {
        buf.cell((x, y)).is_some_and(|c| c.symbol() == "t" && c.bg == fill && c.fg == ink)
    });
    assert!(highlighted, "a matched character reverses to the bright find highlight");
}

// Search screen rendering (specs/search.md).
mod search_screen_render {
    use super::{common, dump, render, render_size};
    use common::{Repo, app_on, enter_tab};
    use herdr_reviewr::app::{App, Mode, Tab};
    use herdr_reviewr::keymap::default_keymap;
    use herdr_reviewr::land_search_completion;
    use herdr_reviewr::search::{CodeHit, FileHit, SearchCompletion, SearchOutcome, SearchResults};
    use herdr_reviewr::{handle_key, handle_mouse, ui};
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;

    const AREA: Rect = Rect { x: 0, y: 0, width: 140, height: 40 };

    fn open_on_all_files(repo: &Repo) -> App {
        let mut app = app_on(repo);
        enter_tab(&mut app, Tab::AllFiles);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('/')), AREA, default_keymap()).unwrap();
        assert_eq!(app.mode, Mode::Search);
        app
    }

    fn key(app: &mut App, code: KeyCode) {
        handle_key(app, KeyEvent::from(code), AREA, default_keymap()).unwrap();
    }

    fn land(app: &mut App, results: SearchResults) {
        let completion = SearchCompletion { generation: 1, outcome: SearchOutcome::Ready(results) };
        land_search_completion(app, completion, 1);
    }

    #[test]
    fn screen_shows_band_chips_and_both_modes() {
        let repo = Repo::init();
        repo.write("src/registry.rs", "fn resolve() {}\nregistry.resolve()\n");
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        for c in "reg".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        land(
            &mut app,
            SearchResults {
                files: vec![FileHit { path: "src/registry.rs".into(), spans: vec![(4, 7)] }],
                code: vec![
                    CodeHit {
                        path: "src/registry.rs".into(),
                        line: 1,
                        text: "fn resolve() {}".into(),
                        spans: vec![(3, 6)],
                    },
                    CodeHit {
                        path: "src/registry.rs".into(),
                        line: 2,
                        text: "registry.resolve()".into(),
                        spans: vec![(0, 3)],
                    },
                ],
                file_total: 4,
                code_more: true,
            },
        );

        // Files mode: the band, both chips with live counts, path rows, the Files clip.
        let out = render(&app);
        let band = out.lines().find(|l| l.contains("> reg")).expect("the band row renders");
        assert!(band.contains("files 4 │ code 2+"), "both chips carry a live count: {band}");
        assert!(!band.contains('⇥'), "the chips drop the flip glyph — the footer owns the key");
        assert!(out.contains("src/registry.rs"), "a path match renders as a file row");
        assert!(out.contains("… more"), "a clipped list marks that there is more");
        assert!(out.contains("─ results"), "the results pane carries a titled rule");
        assert!(out.contains("─ preview"), "the divider row carries the preview title");
        assert!(out.contains("pick") && out.contains("open"), "the screen's footer shows");

        // Code mode: grouped rows under a header, `line:` locators, the clip.
        key(&mut app, KeyCode::Tab);
        let out = render(&app);
        assert!(out.contains("> reg"), "the flip keeps the query");
        assert!(out.contains("1: fn resolve"), "a match row shows its line number");
        assert!(out.contains("2: registry.resolve"), "grouped rows keep engine order");
        assert!(out.contains("… more"), "a cut-short grep shows there is more");
        let header_rows =
            out.lines().filter(|l| l.contains("src/registry.rs") && !l.contains(':')).count();
        assert!(header_rows >= 1, "the file emits one header row: {out}");
    }

    #[test]
    fn screen_shows_indexing_until_warm() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let app = open_on_all_files(&repo);
        let out = render(&app);
        assert!(out.contains("indexing…"), "the screen reads indexing… before the first scan");
        assert!(out.contains("files │ code"), "the count slots stay empty while warming");
    }

    #[test]
    fn no_matches_only_where_the_engine_looked() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        land(&mut app, SearchResults::default());
        let out = render(&app);
        assert!(out.contains("no matches"), "an empty warm Files result reads no matches");

        // An empty query lists nothing in Code mode — no copy at all (specs/search.md).
        key(&mut app, KeyCode::Tab);
        let out = render(&app);
        assert!(!out.contains("no matches"), "an empty query in Code mode lists nothing");
    }

    #[test]
    fn click_picks_then_opens_and_chip_click_flips() {
        let repo = Repo::init();
        repo.write("a.rs", "one\ntwo\n");
        repo.write("b.rs", "three\n");
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        land(
            &mut app,
            SearchResults {
                files: vec![
                    FileHit { path: "a.rs".into(), spans: vec![] },
                    FileHit { path: "b.rs".into(), spans: vec![] },
                ],
                code: Vec::new(),
                file_total: 2,
                code_more: false,
            },
        );
        // Paint once so the screen scroll settles, then resolve rows the frame mapped.
        let _ = dump(&render_size(&app, 140, 40));
        let hit_row = |app: &App, pick: usize| {
            (0..40u16)
                .find(|&y| ui::search_target(app, AREA, 30, y) == Some(ui::SearchTarget::Row(pick)))
                .expect("the result row is clickable")
        };
        let click = |app: &mut App, row: u16| {
            let event = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 30,
                row,
                modifiers: KeyModifiers::NONE,
            };
            handle_mouse(app, event, AREA, &[], default_keymap()).unwrap();
        };

        // A click on an unpicked row picks it; a second click opens it (specs/search.md).
        let row = hit_row(&app, 1);
        click(&mut app, row);
        assert_eq!(app.mode, Mode::Search, "the first click only picks");
        assert_eq!(app.search.as_ref().unwrap().pick, 1);
        click(&mut app, row);
        assert_eq!(app.mode, Mode::Normal, "the second click opens the pick");
        assert_eq!(app.diff_path.as_deref(), Some("b.rs"));

        // A chip click flips the mode.
        let mut app = open_on_all_files(&repo);
        let _ = dump(&render_size(&app, 140, 40));
        let band_y = ui::body_rect(AREA, &app).y;
        let chip_x = (0..140u16)
            .find(|&x| ui::search_target(&app, AREA, x, band_y) == Some(ui::SearchTarget::Chips))
            .expect("the chips are clickable");
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: chip_x,
            row: band_y,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, event, AREA, &[], default_keymap()).unwrap();
        assert_eq!(
            app.search.as_ref().unwrap().search_mode,
            herdr_reviewr::app::SearchMode::Code,
            "a chip click flips the mode"
        );
    }

    #[test]
    fn preview_centers_and_bands_the_hit() {
        let repo = Repo::init();
        let lines: Vec<String> = (1..=60).map(|i| format!("line_{i}")).collect();
        let body = lines.join("\n") + "\n";
        repo.write("a.rs", &body);
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        land(
            &mut app,
            SearchResults {
                files: Vec::new(),
                code: vec![CodeHit {
                    path: "a.rs".into(),
                    line: 30,
                    text: "line_30".into(),
                    spans: vec![(0, 7)],
                }],
                file_total: 0,
                code_more: false,
            },
        );
        key(&mut app, KeyCode::Tab);
        app.build_search_preview();

        let buf = render_size(&app, 140, 40);
        let out = dump(&buf);
        assert!(out.contains("─ preview · a.rs"), "the pane title names the previewed file");
        let y = out
            .lines()
            .position(|l| l.contains("30 line_30"))
            .expect("the hit line is visible with its number") as u16;
        let x = out.lines().nth(y as usize).unwrap().find("line_30").unwrap() as u16;
        let style = buf.cell((x, y)).expect("cell").style();
        assert_eq!(
            style.bg,
            Some(app.palette().match_hl),
            "the hit's matched span wears the match highlight: {style:?}"
        );
        assert!(!out.contains(" 1 line_1\n"), "the hit is centered, not previewed from the top");

        // PageDown moves the pane; the scroll survives the next paint.
        key(&mut app, KeyCode::PageDown);
        let scrolled = app.search.as_ref().unwrap().preview.as_ref().unwrap().scroll.get();
        let _ = render_size(&app, 140, 40);
        assert!(scrolled > 0, "PageDown scrolls the preview");
    }

    #[test]
    fn preview_highlight_lands_on_the_match_under_indentation() {
        // The worker trims each grep line's leading indentation and reports offsets into the
        // trimmed text; the preview keeps the true indentation, so the highlight must shift
        // over it and still cover the match, not slide left into the whitespace or the
        // preceding tokens (specs/search.md).
        let repo = Repo::init();
        let mut lines: Vec<String> = (1..=60).map(|i| format!("let x{i} = {i};")).collect();
        lines[29] = "    fn resolve() {}".to_string(); // line 30, four-space indented
        repo.write("a.rs", &(lines.join("\n") + "\n"));
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        land(
            &mut app,
            SearchResults {
                files: Vec::new(),
                // As the worker emits it: the trimmed line, offsets into the trimmed text.
                code: vec![CodeHit {
                    path: "a.rs".into(),
                    line: 30,
                    text: "fn resolve() {}".into(),
                    spans: vec![(3, 10)], // "resolve" within the trimmed line
                }],
                file_total: 0,
                code_more: false,
            },
        );
        key(&mut app, KeyCode::Tab);
        app.build_search_preview();

        let buf = render_size(&app, 140, 40);
        let out = dump(&buf);
        // The results row above is correctly trimmed; assert on the preview row, which keeps
        // the true indentation — that is where the trimmed spans had to be shifted.
        let preview_at =
            out.lines().position(|l| l.contains("─ preview")).expect("the preview divider");
        let below = out
            .lines()
            .skip(preview_at + 1)
            .position(|l| l.contains("fn resolve() {}"))
            .expect("the hit line previews with its indentation");
        let y = (preview_at + 1 + below) as u16;
        let line = out.lines().nth(y as usize).unwrap();
        let rx = line.find("resolve").unwrap() as u16;
        assert_eq!(
            buf.cell((rx, y)).unwrap().style().bg,
            Some(app.palette().match_hl),
            "the highlight lands on the match under indentation",
        );
        // The indentation and the preceding `fn ` keep the cursor band, not the match highlight.
        let fx = line.find("fn ").unwrap() as u16;
        assert_ne!(
            buf.cell((fx, y)).unwrap().style().bg,
            Some(app.palette().match_hl),
            "the highlight did not slide left into the un-trimmed indentation",
        );
    }

    #[test]
    fn tiny_screen_keeps_the_band() {
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        land(
            &mut app,
            SearchResults {
                files: vec![FileHit { path: "a.rs".into(), spans: vec![] }],
                code: Vec::new(),
                file_total: 1,
                code_more: false,
            },
        );
        app.build_search_preview();
        let out = dump(&render_size(&app, 24, 6));
        assert!(out.contains('>'), "the input band keeps its one row at tiny sizes");
    }

    #[test]
    fn empty_query_shows_a_placeholder_and_no_preview() {
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let app = open_on_all_files(&repo);
        // Warm but no results landed yet: the band teaches, the preview isn't blank.
        let out = render(&app);
        assert!(out.contains("Search files and code…"), "the empty query shows a placeholder");
        let mut app = app;
        land(&mut app, SearchResults::default());
        let out = render(&app);
        assert!(out.contains("no preview"), "nothing to preview shows a dim notice, not a blank");
    }

    #[test]
    fn an_elided_file_result_still_highlights_the_visible_match() {
        // A head-elided path must still mark a match that survives in the shown tail — the
        // highlight is unconditional, remapped across the elision, not dropped (specs/search.md).
        let repo = Repo::init();
        let path = "aaaaaaaaaaaaaaaaaaaa/bbbbbbbbbbbbbbbbbbbb/target_match.rs";
        repo.write(path, "x\n");
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        let at = path.find("target").unwrap() as u32;
        land(
            &mut app,
            SearchResults {
                files: vec![FileHit { path: path.into(), spans: vec![(at, at + 6)] }],
                code: Vec::new(),
                file_total: 1,
                code_more: false,
            },
        );
        // A pane narrow enough to head-elide the long path onto its tail.
        let buf = render_size(&app, 44, 20);
        let out = dump(&buf);
        let y = out
            .lines()
            .position(|l| l.contains('…') && l.contains("target"))
            .expect("the elided path row shows its tail") as u16;
        let line = out.lines().nth(y as usize).unwrap();
        let tx = line.find("target").unwrap() as u16;
        assert_eq!(
            buf.cell((tx, y)).unwrap().style().bg,
            Some(app.palette().match_hl),
            "the match highlight survives the head-elision on the visible tail",
        );
    }

    #[test]
    fn long_query_scrolls_to_keep_the_caret_visible() {
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        land(&mut app, SearchResults::default()); // warm, so the chips have a fixed width
        // The caret sits at the end of the query; a band narrower than the query must
        // scroll its head off and keep the tail (and caret) on screen.
        let query = "aaaaHEAD_bbbbccccddddeeeeffffgggg_TAILzzzz";
        for c in query.chars() {
            key(&mut app, KeyCode::Char(c));
        }
        let out = dump(&render_size(&app, 44, 12));
        let band = out.lines().find(|l| l.contains("TAIL")).expect("the caret end stays visible");
        assert!(!band.contains("HEAD"), "the overflowing head scrolls off the band: {band:?}");
    }

    #[test]
    fn a_changed_file_result_shows_its_marker_and_stats() {
        // A Files result on an uncommitted file wears the same change marker and stats as the
        // file list, alongside the match highlight (specs/search.md).
        let repo = Repo::init();
        repo.write("a.rs", "one\n");
        repo.commit_all("c");
        repo.write("a.rs", "one\ntwo\n"); // uncommitted: one added line
        let mut app = open_on_all_files(&repo);
        land(
            &mut app,
            SearchResults {
                files: vec![FileHit { path: "a.rs".into(), spans: vec![(0, 1)] }],
                code: Vec::new(),
                file_total: 1,
                code_more: false,
            },
        );
        let buf = render_size(&app, 140, 40);
        let out = dump(&buf);
        let row = out.lines().find(|l| l.contains("a.rs")).expect("the file row renders");
        assert!(row.contains("+1"), "the changed file's stats render on its row: {row:?}");
        // The match highlight coexists with the marker and stats.
        let y = out.lines().position(|l| l.contains("a.rs")).unwrap() as u16;
        let x = row.find("a.rs").unwrap() as u16;
        assert_eq!(
            buf.cell((x, y)).expect("cell").style().bg,
            Some(app.palette().match_hl),
            "the match highlight lands on the matched path character",
        );
    }

    #[test]
    fn a_poll_refreshes_the_open_preview_in_place() {
        // A landed poll rebuilds the previewed file's diff in place, so the preview follows
        // the worktree while the held results stay as queried (specs/search.md, overview.md
        // Continuity). Exercises the real reload → reconcile_world → refresh_search_preview
        // wiring, not the method in isolation.
        let repo = Repo::init();
        repo.write("a.rs", "alpha\n");
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        land(
            &mut app,
            SearchResults {
                files: Vec::new(),
                code: vec![CodeHit {
                    path: "a.rs".into(),
                    line: 1,
                    text: "alpha".into(),
                    spans: vec![(0, 5)],
                }],
                file_total: 0,
                code_more: false,
            },
        );
        key(&mut app, KeyCode::Tab);
        app.build_search_preview();
        assert!(render(&app).contains("alpha"), "the preview shows the file's content");

        // The worktree changes, then a poll lands through the synchronous reload path.
        repo.write("a.rs", "alpha\nBETA_LINE\n");
        app.reload().unwrap();
        assert!(render(&app).contains("BETA_LINE"), "the poll refreshed the preview in place");
    }
}

// Style-level emphasis coverage for the match rows (specs/search.md).
mod search_row_emphasis {
    use super::{common, dump, render_size};
    use common::{Repo, app_on, enter_tab};
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::handle_key;
    use herdr_reviewr::keymap::default_keymap;
    use herdr_reviewr::land_search_completion;
    use herdr_reviewr::search::{CodeHit, SearchCompletion, SearchOutcome, SearchResults};
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ratatui::layout::Rect;

    const AREA: Rect = Rect { x: 0, y: 0, width: 140, height: 40 };

    fn code_only(hit: CodeHit) -> SearchCompletion {
        let results =
            SearchResults { files: Vec::new(), code: vec![hit], file_total: 0, code_more: false };
        SearchCompletion { generation: 1, outcome: SearchOutcome::Ready(results) }
    }

    /// A code row too wide for the pane clips around its first matched span, keeping the
    /// `line:` locator and marking the cut head with `…` (specs/search.md).
    #[test]
    fn clipped_code_row_keeps_and_emphasizes_the_match() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('/')), AREA, default_keymap()).unwrap();

        // A long head of `x`s pushes the match past the pane, so the row clips around the
        // first matched span (`needle_marker`, 13 bytes) rather than the un-shown head.
        let text = format!("{}needle_marker tail", "x".repeat(200));
        let start = 200u32;
        let hit = CodeHit { path: "a.rs".into(), line: 1, text, spans: vec![(start, start + 13)] };
        land_search_completion(&mut app, code_only(hit), 1);
        handle_key(&mut app, KeyEvent::from(KeyCode::Tab), AREA, default_keymap()).unwrap();

        let buf = render_size(&app, 140, 40);
        let out = dump(&buf);
        let row = out
            .lines()
            .find(|l| l.contains("needle_marker"))
            .expect("the clipped row keeps the first matched span visible");
        assert!(row.contains("1:"), "the line locator survives the clip: {row}");
        assert!(row.contains("…x"), "the cut head is marked with an ellipsis: {row}");

        let y = out.lines().position(|l| l.contains("needle_marker")).unwrap() as u16;
        // Cell column = char count before the token (every cell here is one column wide).
        let byte = row.find("needle_marker").unwrap();
        let x = row[..byte].chars().count() as u16;
        assert_eq!(
            buf.cell((x, y)).expect("cell").style().bg,
            Some(app.palette().match_hl),
            "the matched span wears the match highlight",
        );
        // A cell in the clipped `…x` head keeps the selection fill, not the match highlight —
        // the band covers the match only, never spilling left across the cut.
        let ell = row.find('…').unwrap();
        let head_x = row[..ell].chars().count() as u16 + 1;
        assert_ne!(
            buf.cell((head_x, y)).expect("cell").style().bg,
            Some(app.palette().match_hl),
            "the clipped head is not highlighted",
        );
    }

    /// A tab-indented code row expands its tabs to spaces, so the indentation shows and
    /// the emphasis lands on the matched word, not shifted by the collapsed tabs
    /// (specs/search.md).
    #[test]
    fn tab_indented_code_row_expands_and_emphasizes() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('/')), AREA, default_keymap()).unwrap();

        // Two leading tabs, then `needle` — the match is at bytes 2..8 of the raw line.
        let hit = CodeHit {
            path: "a.rs".into(),
            line: 1,
            text: "\t\tneedle here".into(),
            spans: vec![(2, 8)],
        };
        land_search_completion(&mut app, code_only(hit), 1);
        handle_key(&mut app, KeyEvent::from(KeyCode::Tab), AREA, default_keymap()).unwrap();

        let buf = render_size(&app, 140, 40);
        let out = dump(&buf);
        let y = out.lines().position(|l| l.contains("needle")).unwrap();
        let row = out.lines().nth(y).unwrap();
        // Eight spaces of expanded indent sit between the locator and `needle`.
        assert!(row.contains("1:         needle"), "tabs expand to spaces: {row:?}");
        let x = row.find("needle").unwrap() as u16;
        let style = buf.cell((x, y as u16)).expect("cell").style();
        assert_eq!(
            style.bg,
            Some(app.palette().match_hl),
            "the highlight tracks the word past the expanded tabs: {style:?}"
        );
    }

    /// A multi-byte head forced through the clip path must paint, not panic — the
    /// engine's span offsets are bytes and the cut walks char boundaries.
    #[test]
    fn clipped_multibyte_code_row_paints() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('/')), AREA, default_keymap()).unwrap();

        // A wide multi-byte head (each `中` is 3 bytes, 2 columns) forces the clip's
        // char-boundary walk onto boundaries a byte/column confusion would land off.
        let head = "中".repeat(200);
        let start = head.len() as u32; // 600 bytes in
        let hit = CodeHit {
            path: "a.rs".into(),
            line: 1,
            text: format!("{head}needle tail"),
            spans: vec![(start, start + 6)],
        };
        land_search_completion(&mut app, code_only(hit), 1);
        handle_key(&mut app, KeyEvent::from(KeyCode::Tab), AREA, default_keymap()).unwrap();

        let buf = render_size(&app, 140, 40);
        let out = dump(&buf);
        let y = out
            .lines()
            .position(|l| l.contains("needle"))
            .expect("the clipped multibyte row paints without panicking") as u16;
        // The highlight starts exactly on the match, not shifted onto the multibyte head:
        // the first highlighted cell on the row is `needle`'s `n`.
        let hx = (0..buf.area.width)
            .find(|&x| buf.cell((x, y)).expect("cell").style().bg == Some(app.palette().match_hl))
            .expect("the match is highlighted");
        assert_eq!(
            buf.cell((hx, y)).expect("cell").symbol(),
            "n",
            "the match highlight lands on the match, past the multibyte head",
        );
    }
}

// --- Agent picker (specs/herdr-host.md) ----------------------------------------------------

fn agent_row(pane: &str, name: &str, state: &str, tab: &str) -> AgentChoice {
    AgentChoice { pane_id: pane.into(), name: name.into(), state: state.into(), tab: tab.into() }
}

/// One saved comment on the first added line, so the picker has a count to title itself with.
fn write_comment(app: &mut App, text: &str) {
    composing(app);
    app.input = text.to_string();
    app.submit_comment();
}

/// An app with three comments and the picker open, matching the spec's mockup.
fn picker_app() -> App {
    let mut app = edited_app();
    for text in ["one", "two", "three"] {
        write_comment(&mut app, text);
    }
    app.open_picker(vec![
        agent_row("w8:p1", "claude", "idle", "Grip Outreach"),
        agent_row("w8:p2", "release-bot", "idle", "Grip Outreach Campaign"),
        agent_row("w8:p3", "codex", "working", "3"),
    ]);
    app
}

#[test]
fn the_last_sent_row_carries_its_tag_and_no_other_row_does() {
    let mut app = edited_app();
    write_comment(&mut app, "one");
    // A prior send to release-bot arms the highlight there and tags the row, so the
    // remembered default reads before `enter` fires it (specs/herdr-host.md).
    app.last_sent_pane = Some("w8:p2".to_string());
    app.open_picker(vec![
        agent_row("w8:p1", "claude", "idle", "1"),
        agent_row("w8:p2", "release-bot", "idle", "2"),
    ]);
    assert_eq!(app.picker_cursor, 1, "the highlight arms on the last-sent agent");
    let out = render(&app);

    let tagged = out.lines().find(|l| l.contains("release-bot")).unwrap_or_default();
    assert!(tagged.contains("· last used"), "the last-sent row is tagged: {tagged:?}");
    let plain = out.lines().find(|l| l.contains("claude")).unwrap_or_default();
    assert!(!plain.contains("last used"), "no other row is tagged: {plain:?}");
}

#[test]
fn an_open_picker_dims_the_view_behind_it_but_never_the_footer() {
    let mut app = edited_app();
    write_comment(&mut app, "one");
    let plain = render_buffer(&app);
    app.open_picker(vec![
        agent_row("w8:p1", "claude", "idle", "1"),
        agent_row("w8:p2", "codex", "idle", "2"),
    ]);
    let dimmed = render_buffer(&app);

    // The tab bar recedes toward the theme base while the picker is up (specs/tui.md).
    // Locate a lettered header cell rather than assuming a column, so a header layout
    // change cannot silently repoint the assertion.
    let x = (0..plain.area.width)
        .find(|&x| {
            plain
                .cell((x, 0))
                .is_some_and(|c| c.symbol().chars().all(char::is_alphanumeric) && c.symbol() != " ")
        })
        .expect("a lettered cell in the tab bar");
    let cell = |buf: &Buffer, x: u16, y: u16| buf.cell((x, y)).unwrap().clone();
    assert_eq!(cell(&plain, x, 0).symbol(), cell(&dimmed, x, 0).symbol());
    assert_ne!(cell(&plain, x, 0).fg, cell(&dimmed, x, 0).fg, "the header cell is scrimmed");

    // The footer is the picker's own key bar, so its primary hint keeps full brightness.
    let footer_y = dimmed.area.height - 1;
    let bright =
        (0..dimmed.area.width).any(|x| dimmed.cell((x, footer_y)).is_some_and(|c| c.fg == PEACH));
    assert!(bright, "the footer's primary key hint stays at full brightness");
}

#[test]
fn neither_popup_reaches_the_footer_that_advertises_its_keys() {
    let mut app = edited_app();
    for text in ["one", "two", "three"] {
        write_comment(&mut app, text);
    }
    let rows = vec![
        agent_row("w8:p1", "claude", "idle", "Grip Outreach"),
        agent_row("w8:p2", "release-bot", "idle", "Grip Outreach Campaign"),
    ];

    // Both popups place through one rule, `body_popup`, so at every pane size the footer keeps
    // naming the keys the popup is listening for — it is the only surface that does
    // (`specs/tui.md`).
    for h in 8..=30u16 {
        app.open_list();
        let listed = dump(&render_size(&app, 44, h));
        app.close_list();
        app.open_picker(rows.clone());
        let picked = dump(&render_size(&app, 44, h));
        app.close_picker();

        for (name, out) in [("comments list", listed), ("agent picker", picked)] {
            let footer = out.lines().last().unwrap_or_default().to_string();
            assert!(
                footer.contains("esc"),
                "the {name} popup covered the footer at height {h}:\n{out}"
            );
        }
    }
}

#[test]
fn the_picker_titles_the_count_and_aligns_the_dim_trail_in_one_column() {
    let app = picker_app();
    let out = render(&app);

    assert!(out.contains("Send 3 comments to"), "the title counts the comments:\n{out}");

    let rows: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("claude") || l.contains("release-bot") || l.contains("codex"))
        .collect();
    assert_eq!(rows.len(), 3, "one row per agent:\n{out}");

    // The names pad to the widest, so every dim trail starts in the same column.
    let starts: Vec<usize> = rows
        .iter()
        .map(|l| l.find("idle").or_else(|| l.find("working")).expect("a state on every row"))
        .collect();
    assert!(starts.windows(2).all(|w| w[0] == w[1]), "trails misaligned at {starts:?}:\n{out}");

    // The tab trails behind the state, separated by the dim dot.
    assert!(rows[0].contains("idle · Grip Outreach"), "{:?}", rows[0]);
    assert!(rows[2].contains("working · 3"), "{:?}", rows[2]);
}

#[test]
fn the_picker_numbers_only_the_rows_a_digit_key_can_reach() {
    let mut app = edited_app();
    write_comment(&mut app, "one");
    let rows: Vec<AgentChoice> = (1..=11)
        .map(|i| agent_row(&format!("w8:p{i}"), &format!("agent{i}"), "idle", "1"))
        .collect();
    app.open_picker(rows);
    let out = render(&app);

    for i in 1..=9 {
        let row = out.lines().find(|l| l.contains(&format!("agent{i} "))).unwrap_or_default();
        assert!(row.contains(&format!(" {i}  ")), "row {i} carries its digit: {row:?}");
    }
    // Rows past the ninth are reached by movement, so they carry no number to press.
    let tenth = out.lines().find(|l| l.contains("agent10")).unwrap_or_default();
    assert!(!tenth.contains(" 10 "), "row 10 must not advertise an unreachable key: {tenth:?}");
}

#[test]
fn a_picker_taller_than_the_pane_scrolls_to_keep_the_highlight_visible() {
    let mut app = edited_app();
    write_comment(&mut app, "one");
    let rows: Vec<AgentChoice> = (1..=20)
        .map(|i| agent_row(&format!("w8:p{i}"), &format!("agent{i}"), "idle", "1"))
        .collect();
    app.open_picker(rows);

    // A short frame cannot show twenty rows; the last one is still reachable.
    let short = dump(&render_size(&app, 80, 12));
    assert!(!short.contains("agent20"), "the tail is clipped at this height:\n{short}");

    app.picker_goto(19);
    let scrolled = dump(&render_size(&app, 80, 12));
    assert!(scrolled.contains("agent20"), "the view follows the highlight:\n{scrolled}");

    // The popup clamps to the body band, so even this over-tall picker never covers the
    // footer — the one surface advertising its keys (specs/tui.md).
    let last_row = scrolled.lines().last().unwrap_or_default().to_string();
    assert!(last_row.contains("enter"), "the footer keeps the picker's keys: {last_row:?}");
}

#[test]
fn a_click_on_a_picker_row_moves_the_highlight_and_misses_stay_inert() {
    let mut app = picker_app();
    let area = Rect::new(0, 0, 140, 40);
    let out = render(&app);

    let (row_y, line) = out
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("codex"))
        .map(|(y, l)| (y as u16, l.to_string()))
        .expect("the codex row is painted");
    let col = line.find("codex").expect("a column inside the row") as u16;

    assert_eq!(ui::hit_picker_row(area, &app, col, row_y), Some(2));
    // The title row and everything outside the popup are inert (specs/input.md).
    assert_eq!(ui::hit_picker_row(area, &app, col, row_y - 3), None);
    assert_eq!(ui::hit_picker_row(area, &app, 0, 0), None);

    handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
            column: col,
            row: row_y,
            modifiers: KeyModifiers::NONE,
        },
        area,
        &[],
        &Keymap::default(),
    )
    .unwrap();
    assert_eq!(app.picker_cursor, 2, "a click moves the highlight to the clicked row");
}

// --- Header base label (specs/tui.md) ------------------------------------------------------

/// A repo on branch `feature` past `main`, with `origin/HEAD` naming `main` the default.
/// The repo rides along: opening the picker shells out to git at click time.
fn based_app() -> (Repo, App) {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["branch", "dev"]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    (r, app)
}

#[test]
fn the_branch_header_names_the_base_and_its_click_opens_the_picker() {
    let (_repo, mut app) = based_app();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(line0.contains("[branch] vs main"), "the bare base name follows the scope: {line0}");

    let base: Vec<u16> = (0..AREA.width)
        .filter(|&c| ui::hit_header(AREA, &app, app.keymap(), c, 0) == Some(HeaderHit::Base))
        .collect();
    assert!(!base.is_empty(), "the base label is clickable");
    let click = MouseEvent {
        kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
        column: base[0],
        row: 0,
        modifiers: KeyModifiers::NONE,
    };
    let keymap = app.keymap().clone();
    handle_mouse(&mut app, click, AREA, &[], &keymap).unwrap();
    let frame = render(&app);
    assert!(frame.contains("Pick base"), "the click opens the picker popup");
    assert!(frame.contains("dev"), "the sibling branch is a row");
    assert!(frame.contains("default"), "the default branch is marked");
}

#[test]
fn a_skipped_pick_warns_beside_the_resolved_base() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    herdr_reviewr::git::write_base_pick(r.path(), "gone").unwrap();
    r.git(&["checkout", "-q", "-b", "feature"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(line0.contains("vs main · gone missing"), "the dormant pick reads as skipped: {line0}");
}

#[test]
fn without_a_resolving_base_the_header_reads_no_base() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.git(&["checkout", "-q", "-b", "feature"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let frame = render(&app);
    let line0 = frame.lines().next().unwrap().to_string();
    assert!(line0.contains("[branch] no base"), "the empty state is named: {line0}");
    assert!(frame.contains("pick base"), "the footer advertises the picker");
}

#[test]
fn a_dormant_pick_shows_beside_the_empty_state() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    herdr_reviewr::git::write_base_pick(r.path(), "gone").unwrap();
    r.git(&["checkout", "-q", "-b", "feature"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(
        line0.contains("no base · gone missing"),
        "a dormant choice never reads as never-chosen: {line0}"
    );
}

#[test]
fn an_overlong_base_name_truncates_with_an_ellipsis() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    let long = format!("feature/{}", "x".repeat(80));
    r.git(&["branch", &long]);
    herdr_reviewr::git::write_base_pick(r.path(), &long).unwrap();
    r.git(&["checkout", "-q", "-b", "work"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = dump(&render_size(&app, 80, 20)).lines().next().unwrap().to_string();
    assert!(line0.contains("vs feature/x"), "the name paints up to the fit: {line0}");
    assert!(line0.contains('…'), "the overflow truncates with a trailing ellipsis: {line0}");
    assert!(line0.contains("1 changed"), "the right-aligned stats survive the long name: {line0}");
}

#[test]
fn a_narrow_header_never_maps_a_click_outside_the_painted_base() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    let long = format!("feature/{}", "x".repeat(60));
    r.git(&["branch", &long]);
    herdr_reviewr::git::write_base_pick(r.path(), &long).unwrap();
    r.git(&["checkout", "-q", "-b", "work"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();

    // The base label truncates to its budget at a narrow width, and the hit test walks the
    // same arithmetic the paint does: every column it claims carries painted label, and the
    // claim is one unbroken run (specs/tui.md).
    for width in [40u16, 56, 72] {
        let area = Rect { x: 0, y: 0, width, height: 12 };
        let line0 = dump(&render_size(&app, width, 12)).lines().next().unwrap().to_string();
        let cells: Vec<char> = line0.chars().collect();
        let hits: Vec<u16> = (0..width)
            .filter(|&c| ui::hit_header(area, &app, app.keymap(), c, 0) == Some(HeaderHit::Base))
            .collect();
        let Some((&first, &last)) = hits.first().zip(hits.last()) else {
            // Too narrow for even one column of the name: the base left the header whole,
            // so nothing paints a nameless `vs` and nothing claims it (specs/tui.md).
            assert!(!line0.contains("vs"), "width {width}: a nameless `vs` paints: {line0}");
            assert!(line0.contains("[branch]"), "width {width}: the scope survives: {line0}");
            continue;
        };
        assert_eq!(
            hits.len() as u16,
            last - first + 1,
            "width {width}: the base claims one unbroken run"
        );
        let claimed: String = hits.iter().map(|&c| cells[c as usize]).collect();
        assert_ne!(claimed.trim(), "vs", "width {width}: a nameless `vs` is claimed: {line0}");
        assert!(
            !claimed.ends_with(' '),
            "width {width}: the claim runs past the painted label: {line0}"
        );
        assert_eq!(
            cells.get(last as usize + 1).copied(),
            Some(' '),
            "width {width}: the claim stops short of the painted label: {line0}"
        );
    }
}

#[test]
fn an_overlong_skipped_tail_never_evicts_the_base_name() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    let long = format!("feature/{}", "x".repeat(80));
    herdr_reviewr::git::write_base_pick(r.path(), &long).unwrap();
    r.git(&["checkout", "-q", "-b", "work"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = dump(&render_size(&app, 80, 20)).lines().next().unwrap().to_string();
    assert!(line0.contains("vs main"), "the resolved name keeps first claim: {line0}");
    assert!(line0.contains("· feature/x"), "the skipped tail paints in what remains: {line0}");
    assert!(line0.contains('…'), "the tail truncates with a trailing ellipsis: {line0}");
    assert!(line0.contains("1 changed"), "the right-aligned stats survive the long tail: {line0}");
}
