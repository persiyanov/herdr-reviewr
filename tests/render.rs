//! Render tests: drive `ui::render` through ratatui's `TestBackend` and assert on
//! the painted buffer, so the layout and component wiring are checked for real.

mod common;

use common::Repo;
use herdr_reviewr::app::{App, Focus, Tab};
use herdr_reviewr::model::Scope;
use herdr_reviewr::ui::{self, HeaderHit};
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

/// Render at a specific width (height fixed), for footer fit-to-width assertions.
fn render_at(app: &App, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    dump(terminal.backend().buffer())
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
    assert!(out.contains("changed"), "the header shows the changed count");
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
}

#[test]
fn pr_empty_states_name_candidates_detachment_and_the_ambiguity_count() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::PrView;
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.set_tab(Tab::Pr).unwrap();
    // The empty state names what was queried, so resolution is inspectable, never silent.
    let names = ["alpha", "beta", "gamma", "delta", "epsilon"];
    app.pr = PrView::NoPr(names.iter().map(|s| (*s).to_string()).collect());
    let out = render(&app);
    assert!(
        out.contains("no PR for alpha, beta, gamma +2 more yet"),
        "candidates are named, capped at three:\n{out}"
    );
    app.pr = PrView::NoPr(Vec::new());
    let out = render(&app);
    assert!(out.contains("detached HEAD"), "detached wording for no candidates:\n{out}");
    app.pr = PrView::Ambiguous(3);
    let out = render(&app);
    assert!(out.contains("3 open PRs"), "the ambiguity count shows:\n{out}");
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

#[test]
fn all_files_tab_bar_footer_and_count_read_for_the_tab() {
    use herdr_reviewr::app::Tab;
    let r = Repo::init();
    r.write("a.rs", "one\n");
    r.commit_all("init");
    r.write("a.rs", "ONE\n"); // one change
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();

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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
    app.set_tab(Tab::AllFiles).unwrap(); // clean repo: no seed; cursor rests on collapsed src/

    let out = render(&app);
    assert!(out.contains("select a file to read"), "the empty All files left pane copy:\n{out}");
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
    let app = rebound_app("comment = [\"ㅊ\", \"c\"]\ntab-pr = [\"p\"]\n");
    let out = render(&app);
    let footer = footer_line(&out);
    // A wide hint key spans two buffer cells, so the dump carries a placeholder space after it.
    assert!(footer.contains("ㅊ  comment"), "the hint is the first bound key:\n{footer}");
    assert!(out.contains("p PR"), "the header tab hint follows its binding:\n{out}");
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
fn pr_bodies_render_as_markdown_and_the_description_row_pins_first() {
    use herdr_reviewr::forge::{Comment, CommentKind, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
fn the_refetch_indicator_lives_in_the_title_not_the_content() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
fn markdown_links_paint_click_regions_and_the_guard_gates_them() {
    use herdr_reviewr::forge::{PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, None);
    app.reload().unwrap();
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
