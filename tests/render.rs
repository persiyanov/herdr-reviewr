//! Render tests: drive `ui::render` through ratatui's `TestBackend` and assert on
//! the painted buffer, so the layout and component wiring are checked for real.

mod common;

use common::{Repo, app_on};
use herdr_reviewr::app::{App, Focus, Tab};
use herdr_reviewr::config::NavigatorPosition;
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
fn invalid_config_replaces_the_entire_sidebar_with_its_error() {
    let mut app = edited_app();
    app.set_config_error(
        "config /tmp/reviewr/config.toml: invalid value for `theme`; expected a built-in theme name"
            .to_string(),
    );

    let out = render(&app);

    assert!(out.contains("config /tmp/reviewr/config.toml"));
    assert!(out.contains("expected a built-in theme name"));
    assert!(out.contains("The config reloads automatically."));
    assert!(!out.contains("Changes"), "normal sidebar chrome must be hidden");
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

    // 62 columns is the exact fit. The totals' `−` is multi-byte, so this breaks if the
    // header measures bytes instead of display width.
    let header = render_at(&app, 62).lines().next().unwrap().to_string();
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
fn the_footer_drops_to_fit_and_marks_the_clip() {
    let mut app = edited_app();
    on_changed_line(&mut app); // diff focus, content line → c comment · v select · …
    // Wide: every action fits, nothing is clipped.
    let wide = footer_line(&render_at(&app, 120));
    assert!(
        wide.contains("c comment") && wide.contains("v select") && !wide.contains('…'),
        "wide footer shows all actions, no clip marker:\n{wide}"
    );
    // Narrow: the primary survives, the least-relevant actions are trimmed, and `…` marks it.
    let narrow = footer_line(&render_at(&app, 18));
    assert!(narrow.contains("c comment"), "the primary action is never dropped:\n{narrow}");
    assert!(narrow.contains('…'), "the clip is marked with …:\n{narrow}");
    assert!(!narrow.contains("v select"), "the least-relevant action is trimmed:\n{narrow}");
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
    let narrow = render_at(&app, 44).lines().next().unwrap().to_string();
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
fn pr_empty_states_are_calm_and_keep_the_ambiguity_count() {
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
    app.pr = PrView::Ambiguous(3);
    let out = render(&app);
    assert!(out.contains("3 open PRs"), "the ambiguity count shows:\n{out}");
    assert!(out.contains("Keep one open, then press r."), "the remedy shows:\n{out}");
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
fn header_clicks_map_to_scope_and_send() {
    let app = edited_app(); // scope uncommitted, no comments
    // Scan the header row instead of hardcoding columns, so the test survives changes
    // to the label/button text.
    let scope: Vec<u16> = (0..AREA.width)
        .filter(|&c| ui::hit_header(AREA, &app, app.keymap(), c, 0) == Some(HeaderHit::Scope))
        .collect();
    let send: Vec<u16> = (0..AREA.width)
        .filter(|&c| ui::hit_header(AREA, &app, app.keymap(), c, 0) == Some(HeaderHit::Send))
        .collect();

    assert!(!scope.is_empty(), "scope chip is clickable");
    assert!(!send.is_empty(), "send button is clickable");
    assert!(scope.iter().max() < send.iter().min(), "scope is left of the button, no overlap");
    assert!(*send.iter().max().unwrap() < AREA.width);

    let gap = scope.iter().max().unwrap() + 1;
    assert_eq!(
        ui::hit_header(AREA, &app, app.keymap(), gap, 0),
        None,
        "the space between controls is inert"
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
    let body = ui::body_rect(AREA);

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
    let row = ui::body_rect(six).y;
    assert_eq!((0..6).filter(|&col| ui::in_files_pane(six, &app, col, row)).count(), 3);
    assert_eq!((0..6).filter(|&col| ui::in_diff_pane(six, &app, col, row)).count(), 3);

    let five = Rect::new(0, 0, 5, 10);
    let row = ui::body_rect(five).y;
    assert_eq!((0..5).filter(|&col| ui::in_files_pane(five, &app, col, row)).count(), 2);
    assert_eq!((0..5).filter(|&col| ui::in_diff_pane(five, &app, col, row)).count(), 3);

    app.navigator_position = NavigatorPosition::Top;
    let eight = Rect::new(0, 0, 10, 10); // body height 8
    let col = ui::body_rect(eight).x;
    assert_eq!((1..9).filter(|&row| ui::in_files_pane(eight, &app, col, row)).count(), 3);
    assert_eq!((1..9).filter(|&row| ui::in_diff_pane(eight, &app, col, row)).count(), 5);

    let seven = Rect::new(0, 0, 10, 7); // body height 5: navigator gets floor(5 / 2)
    let col = ui::body_rect(seven).x;
    assert_eq!((1..6).filter(|&row| ui::in_files_pane(seven, &app, col, row)).count(), 2);
    assert_eq!((1..6).filter(|&row| ui::in_diff_pane(seven, &app, col, row)).count(), 3);
}

#[test]
fn pr_focus_border_tracks_tab_between_navigator_and_read_pane() {
    let mut app = edited_app();
    app.set_tab(Tab::Pr).unwrap();
    app.focus = Focus::Files;
    let body = ui::body_rect(AREA);
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

#[test]
fn all_files_tab_bar_footer_and_count_read_for_the_tab() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    r.write("a.rs", "ONE\n"); // one change
    let mut app = app_on(&r);
    app.set_tab(Tab::AllFiles).unwrap();

    let out = render(&app);
    assert!(out.contains("1 Changes"), "tab labels carry their switch digit:\n{out}");
    assert!(out.contains("2 All files"));
    assert!(
        out.contains("1 changed"),
        "the changed count stays in the header on All files:\n{out}"
    );
    let footer = footer_line(&out);
    assert!(footer.contains("scope"), "the footer shows context actions on All files:\n{footer}");
    assert!(
        !footer.contains("changed"),
        "the changed count is not repeated in the footer:\n{footer}"
    );
}

#[test]
fn a_narrow_overflowing_header_does_not_mis_map_a_click_to_send() {
    let r = Repo::init();
    r.write("a.rs", "x\n");
    r.commit_all("init");
    r.write("a.rs", "y\n");
    let app = app_on(&r);

    // At a narrow sidebar width the two-tab header overflows and the Send button is off-screen.
    // No on-screen column may map to Send — the old right-aligned hit-zone landed a phantom Send
    // over the chip/tab region, swallowing those clicks as a Send.
    let width: u16 = 34;
    let area = Rect::new(0, 0, width, 40);
    let phantom =
        (0..width).any(|c| ui::hit_header(area, &app, app.keymap(), c, 0) == Some(HeaderHit::Send));
    assert!(!phantom, "no on-screen column mis-maps to Send when the narrow header overflows");

    // At a wide width the Send button is right-aligned and clickable.
    let wide = Rect::new(0, 0, 140, 40);
    let send =
        (0..140).any(|c| ui::hit_header(wide, &app, app.keymap(), c, 0) == Some(HeaderHit::Send));
    assert!(send, "Send is clickable when the header fits");
}

#[test]
fn all_files_empty_pane_reads_select_a_file() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("src/a.rs", "x\n");
    r.write("src/b.rs", "y\n"); // two children so src/ is a real collapsed dir, not a folded file
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::AllFiles).unwrap(); // clean repo: no seed; cursor rests on collapsed src/

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
    let app = rebound_app("comment = [\"ㅊ\", \"c\"]\ntab-pr = [\"z\"]\n");
    let out = render(&app);
    let footer = footer_line(&out);
    // A wide hint key spans two buffer cells, so the dump carries a placeholder space after it.
    assert!(footer.contains("ㅊ  comment"), "the hint is the first bound key:\n{footer}");
    assert!(out.contains("z PR"), "the header tab hint follows its binding:\n{out}");
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
    for (needle, tab) in
        [("Changes", Tab::Changes), ("2 All files", Tab::AllFiles), ("3 PR", Tab::Pr)]
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
    app.set_tab(Tab::AllFiles).unwrap();

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
    // the footer never offers the inert preview toggle (specs/tui.md).
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
    let body = ui::body_rect(area);
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
fn the_refetch_indicator_lives_in_the_title_not_the_content() {
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

    app.set_pr_refreshing(true);
    let refreshing = render(&app);
    assert!(refreshing.contains("refreshing…"), "the indicator shows:\n{refreshing}");
    assert_eq!(
        row_of(&refreshing, "steady content"),
        before,
        "a refetch never shifts the content the reader is on"
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
fn a_short_narrow_pr_pane_keeps_the_retry_action_and_one_body_row() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr =
        PrView::Pr(Box::new(PrSnapshot { body: "steady body".into(), ..common::pr_snapshot() }));
    app.apply_pr(PrView::NotAuthed("github.example.com".to_string()));

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
    app.set_tab(Tab::AllFiles).unwrap();

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
    app.set_tab(Tab::AllFiles).unwrap();

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
    app.set_tab(Tab::AllFiles).unwrap();
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
