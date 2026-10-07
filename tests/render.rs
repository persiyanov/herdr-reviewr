//! Render tests: `ui::render` into a `TestBackend`, asserting on the painted buffer.

mod common;

use common::{Repo, app_on, app_on_rendered, enter_tab};
use herdr_reviewr::app::{App, BaseChoice, BasePicker, BaseProbe, Focus, Mode, Tab};
use herdr_reviewr::config::NavigatorPosition;
use herdr_reviewr::diff::{Bar, MarkerKind};
use herdr_reviewr::herdr::AgentChoice;
use herdr_reviewr::keymap::Keymap;
use herdr_reviewr::model::Scope;
use herdr_reviewr::roles::{Fill, Ink};
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

/// Catppuccin surface2 — the focused cursor row's fill, in the diff and in every list.
const CURSOR_BG: ratatui::style::Color = ratatui::style::Color::Rgb(0x58, 0x5b, 0x70);
/// Catppuccin's accent (herdr's blue): the caret block and the footer's keys.
const ACCENT: ratatui::style::Color = ratatui::style::Color::Rgb(0x89, 0xb4, 0xfa);

/// The right `100-pct`% of every frame row, for pane-scoped assertions.
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
            if buf.cell((x, y)).is_some_and(|c| c.bg == ACCENT && c.symbol() == "b") {
                found = true;
            }
        }
    }
    assert!(found, "the caret block highlights the character at the caret");
}

/// The character under the caret reads on its block in every theme: the light themes' accents
/// are mid-tones, where the background color fell to 3.1:1.
#[test]
fn the_character_under_the_caret_reads_in_every_theme() {
    for theme in herdr_reviewr::theme::NAMES {
        let mut app = edited_app();
        app.set_cli_theme(Some(theme.to_string()));
        composing(&mut app);
        app.input_push('a');
        app.input_push('q');
        app.caret_left();
        let buf = render_buffer(&app);
        let caret = buf.content.iter().find(|c| c.bg == app.palette().fill(Fill::Caret));
        let caret = caret.unwrap_or_else(|| panic!("{theme}: the caret paints"));
        assert_eq!(caret.symbol(), "q", "{theme}");
        let ratio = herdr_reviewr::roles::contrast(caret.fg, caret.bg);
        assert!(ratio >= 4.5, "{theme}: the caret's character {ratio:.2} < 4.5");
    }
}

#[test]
fn backspacing_a_wide_character_leaves_the_terminal_cursor_unpainted() {
    let mut app = edited_app();
    composing(&mut app);
    app.input_push('日');
    app.input_push('本');
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| ui::render(f, &app)).unwrap();

    let before = terminal.backend().cursor_position();
    app.input_backspace();
    terminal.draw(|f| ui::render(f, &app)).unwrap();

    let cursor = terminal.backend().cursor_position();
    assert_eq!(
        (cursor.x + 2, cursor.y),
        (before.x, before.y),
        "the cursor retreats one wide character"
    );
    let cell = terminal.backend().buffer().cell(cursor).unwrap();
    assert_eq!(cell.bg, ratatui::style::Color::Reset);
    assert_eq!(cell.symbol(), " ");
}

#[test]
fn the_base_picker_anchors_the_terminal_cursor_at_its_caret() {
    let mut app = edited_app();
    app.base_picker = Some(BasePicker {
        rows: vec![BaseChoice::Branch {
            name: "main".to_string(),
            pr_base: false,
            is_default: true,
            current: false,
            tip_secs: 1,
        }],
        cursor: 0,
        query: String::new(),
        caret: 0,
        probe: BaseProbe::Idle,
    });
    app.mode = Mode::BasePick;
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| ui::render(f, &app)).unwrap();
    let empty = terminal.backend().cursor_position();

    app.input_push('日');
    terminal.draw(|f| ui::render(f, &app)).unwrap();

    let after = terminal.backend().cursor_position();
    assert_eq!(
        (after.x, after.y),
        (empty.x + 2, empty.y),
        "the cursor advances one wide character"
    );
    let cell = terminal.backend().buffer().cell(after).unwrap();
    assert_eq!(
        cell.bg,
        ratatui::style::Color::Reset,
        "end of input leaves the cursor cell unpainted"
    );
}

#[test]
fn a_height_capped_composer_scrolls_to_keep_the_caret_visible() {
    let mut app = edited_app();
    composing(&mut app);
    for _ in 0..599 {
        app.input_push('x');
    }
    app.input_push('z'); // the unique last character locates the caret in the buffer
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal.draw(|f| ui::render(f, &app)).unwrap();

    let buffer = terminal.backend().buffer();
    let cursor = terminal.backend().cursor_position();
    let (zx, zy) = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
        .find(|&(x, y)| buffer.cell((x, y)).unwrap().symbol() == "z")
        .expect("the box scrolled the last typed character into view");
    // The cursor sits where the next character lands, after `z` or on the next row.
    let inline = (cursor.x, cursor.y) == (zx + 1, zy);
    let row_start =
        (0..buffer.area.width).find(|&x| buffer.cell((x, zy)).unwrap().symbol() == "x").unwrap();
    let wrapped = (cursor.x, cursor.y) == (row_start, zy + 1);
    assert!(
        inline || wrapped,
        "the cursor sits after the text (cursor {cursor:?}, z at ({zx},{zy}))"
    );
    assert_eq!(
        buffer.cell(cursor).unwrap().symbol(),
        " ",
        "end of input leaves the cursor cell blank"
    );
}

#[test]
fn the_find_band_anchors_the_terminal_cursor_at_its_caret() {
    let r = Repo::init();
    r.write("base.txt", "x\n");
    r.commit_all("init");
    r.write("m.rs", "let total = 1;\n");
    let mut app = app_on(&r);
    app.focus = Focus::Diff;
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 140, 40);
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), area, &keymap)
        .unwrap();

    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| ui::render(f, &app)).unwrap();
    let empty = terminal.backend().cursor_position();

    handle_key(&mut app, KeyEvent::from(KeyCode::Char('日')), area, &keymap).unwrap();
    terminal.draw(|f| ui::render(f, &app)).unwrap();

    let after = terminal.backend().cursor_position();
    assert_eq!(
        (after.x, after.y),
        (empty.x + 2, empty.y),
        "the cursor advances one wide character"
    );
}

#[test]
fn caret_vertical_moves_between_wrapped_rows() {
    // "abcdef" hard-wraps at width 3 to "abc"/"def"; caret 4 (def col 1) up → 1; 1 down → 4.
    assert_eq!(ui::caret_vertical("abcdef", 4, 3, false), 1);
    assert_eq!(ui::caret_vertical("abcdef", 1, 3, true), 4);
    // Composer wrapping preserves repeated spaces so every caret index remains addressable.
    assert_eq!(ui::caret_vertical("ab  cd", 4, 2, false), 2);
    assert_eq!(ui::caret_vertical("ab  cd", 2, 2, true), 4);
    // A line exactly filling the width adds no phantom row, so one step crosses it.
    assert_eq!(ui::caret_vertical("abc\ndef", 0, 3, true), 4);
    assert_eq!(ui::caret_vertical("abc\ndef", 4, 3, false), 0);
    // The caret past the full line sits visually on the next row, and motion agrees.
    assert_eq!(ui::caret_vertical("abc\ndef", 3, 3, false), 0);
    assert_eq!(ui::caret_vertical("abc\ndef", 3, 3, true), 7);
}

#[test]
fn the_fold_hint_names_the_expand_binding() {
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

    // A rebound `expand` renames the fold label and the footer hint alike.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[keybindings]\nexpand = [\"x\"]\n").unwrap();
    app.set_plugin_config(herdr_reviewr::config::plugin_config_in(dir.path()).unwrap());
    let out = render(&app);
    assert!(out.contains("x expand"), "the rebound key names the hint:\n{out}");
    assert!(!out.contains("→ expand"), "the freed arrow leaves the hint");
}

fn edited_app() -> App {
    let r = Repo::init();
    r.write("hello.rs", "alpha\nbeta\n");
    r.commit_all("init");
    r.write("hello.rs", "alpha\nBETA\n");
    // Rendering reads cached state, so the repo can drop here.
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

    // Only the navigator: the diff header shows the full path too.
    let files_pane = right_column(&render(&app), 70);
    assert!(files_pane.contains("src/"), "the directory groups its files: {files_pane:?}");
    assert!(files_pane.contains("app.rs") && files_pane.contains("ui.rs"), "files by basename");
    assert!(!files_pane.contains("src/app.rs"), "a grouped file is not shown by full path");
    assert!(files_pane.contains("Cargo.toml"), "the top-level file shows too");
}

#[test]
fn an_expanded_directory_nests_its_children() {
    // An unmarked row still holds the chevron's width.
    let r = Repo::init();
    r.write("src/app.rs", "x\n");
    r.write("src/ui.rs", "y\n");
    r.write("tests/a.rs", "a\n");
    r.write("tests/b.rs", "b\n");
    r.write("README.md", "hi\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Files;
    app.file_cursor = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    app.expand_dir();

    let buf = render_buffer(&app);
    // Search from the files pane so a left-pane path cannot steal the match.
    let files_x0 = 140 - 140 * 32 / 100 + 1;
    let src = token_x(&buf, "src/", files_x0);
    let tests = token_x(&buf, "tests/", files_x0);
    let readme = token_x(&buf, "README.md", files_x0);
    let app_rs = token_x(&buf, "app.rs", files_x0);
    assert_eq!(src, tests, "sibling directories share a name column");
    assert_eq!(src, readme, "a root file name lines up with a root directory");
    assert!(app_rs > src, "a child file sits to the right of its parent: {app_rs} vs {src}");
}

/// First painted column of `token` in `buf` at or after `x0`. Panics if it never appears.
fn token_x(buf: &Buffer, token: &str, x0: u16) -> u16 {
    token_at(buf, token, x0).0
}

/// Where `token` first paints at or right of column `x0`, scanning rows top down.
fn token_at(buf: &Buffer, token: &str, x0: u16) -> (u16, u16) {
    let chars: Vec<char> = token.chars().collect();
    let n = chars.len() as u16;
    for y in 0..buf.area.height {
        for x in x0..=buf.area.width.saturating_sub(n) {
            let hit = (0..n).all(|i| {
                buf.cell((x + i, y)).is_some_and(|c| c.symbol() == chars[i as usize].to_string())
            });
            if hit {
                return (x, y);
            }
        }
    }
    panic!("{token} was not painted at x>={x0}");
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
    // Every break lands on a space, so no word splits.
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
    // 50 CJK glyphs span 100 columns, so they wrap to more rows than 50 ASCII.
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

    // The emphasis fill sits under a changed character, not the shared prefix.
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
fn a_line_that_gained_only_a_cr_paints_its_marker_emphasized() {
    const EMPH_INS_BG: ratatui::style::Color = ratatui::style::Color::Rgb(0x30, 0x55, 0x3f);
    // `-text`: git keeps the CR, so `git diff` reports the line changed.
    let r = Repo::init();
    r.write(".gitattributes", "* -text\n");
    r.write("e.txt", "alpha\nbeta\n");
    r.commit_all("init");
    r.write("e.txt", "alpha\r\nbeta\n");
    let mut app = app_on(&r);
    app.focus = Focus::Files; // no diff cursor, so the emphasis bg shows
    let buf = render_buffer(&app);
    let out = dump(&buf);

    // The insertion ends in the marker, and no raw CR reaches the terminal.
    let rows: Vec<&str> = out.lines().filter(|l| l.contains("alpha")).collect();
    assert!(rows.iter().any(|l| l.contains("alpha^M")), "{out}");
    assert!(rows.iter().any(|l| l.contains("alpha ") && !l.contains("alpha^M")), "{out}");
    assert!(!out.contains('\r'));
    let (x, y) = (0..140u16)
        .flat_map(|x| (0..40u16).map(move |y| (x, y)))
        .find(|&(x, y)| buf.cell((x, y)).is_some_and(|c| c.symbol() == "^"))
        .expect("the marker is painted");
    assert_eq!(buf.cell((x, y)).unwrap().bg, EMPH_INS_BG);
    assert_eq!(buf.cell((x + 1, y)).unwrap().symbol(), "M");
    assert_eq!(buf.cell((x + 1, y)).unwrap().bg, EMPH_INS_BG);
}

/// Catppuccin surface1 — the cursor fill of the pane that does not hold focus.
const UNFOCUSED_CURSOR_BG: ratatui::style::Color = ratatui::style::Color::Rgb(0x45, 0x47, 0x5a);

#[test]
fn the_diff_cursor_row_is_marked_from_either_pane() {
    // The diff cursor row fills like the file list's, softer when unfocused.
    let mut app = edited_app();
    app.focus = Focus::Diff;
    app.next_hunk();
    let cursor_y = |app: &App| 2 + app.diff_cursor as u16; // border at y=1, first row at y=2
    let fill = |app: &App, bg| {
        let buf = render_buffer(app);
        let y = cursor_y(app);
        (1..40u16).filter(|&x| buf.cell((x, y)).is_some_and(|c| c.bg == bg)).count()
    };

    assert!(fill(&app, CURSOR_BG) > 10, "the focused diff fills its cursor row with surface2");

    app.focus = Focus::Files;
    assert!(
        fill(&app, UNFOCUSED_CURSOR_BG) > 10,
        "and still marks it, a step softer, while the file list holds focus"
    );
}

#[test]
fn the_selected_file_row_fills_with_the_shared_cursor_color() {
    let app = edited_app(); // one file, file_cursor = 0, Files focused
    let buf = render_buffer(&app);
    // Files pane: right 32% of 140 cols; its border is at y=1, first content row at y=2.
    let files_x0 = 140 - 140 * 32 / 100 + 1;
    let selected =
        (files_x0..139).filter(|&x| buf.cell((x, 2)).is_some_and(|c| c.bg == CURSOR_BG)).count();
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
        (1..139u16).filter(|&x| buf.cell((x, cursor_y)).is_some_and(|c| c.bg == CURSOR_BG)).count()
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
    assert!(!out.contains("p layout"), "`p layout` drops while hidden");

    app.toggle_navigator_hidden();
    let out = render(&app);
    assert!(out.contains("z hide"), "visible, the `go` band lists the hide key");
    assert!(out.contains("p layout"), "`p layout` returns with the navigator");
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

    // An exact fit at 76 columns, which breaks if the multi-byte `−` counts as bytes.
    let header = render_at(&app, 76).lines().next().unwrap().to_string();
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
    // Too narrow: the primary sheds its label and the `?` survives.
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
    // The primary sheds its label so `send` and `?` never clip.
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

    // A status with no room to paint costs the row nothing.
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

    // At 40 columns the actions yield to the status, which nothing else repeats.
    app.status = "no agent in this workspace, press y to copy".to_string();
    let narrow = footer_line(&render_at(&app, 40));
    assert!(narrow.contains("no agent in"), "the refusal shows at 40 columns:\n{narrow}");
    assert!(narrow.contains("s send 1"), "send never drops:\n{narrow}");
    assert!(narrow.trim_end().ends_with('?'), "the `?` never drops:\n{narrow}");
    assert!(!narrow.contains("d delete"), "the cursor's actions yield to the status:\n{narrow}");

    // With room for both, nothing yields.
    let wide = footer_line(&render_at(&app, 120));
    assert!(
        wide.contains("no agent in this workspace, press y to copy"),
        "a wide row shows the whole refusal:\n{wide}"
    );
    assert!(wide.contains("d delete"), "and keeps the cursor's actions:\n{wide}");

    // Below a legible width the status drops rather than paint a lone `·` promising a message.
    let tiny = footer_line(&render_at(&app, 20));
    assert!(!tiny.contains("agent"), "no room for a legible message, so none is painted:\n{tiny}");
    assert!(tiny.contains("s send 1"), "send still never drops:\n{tiny}");

    // A status never pushes `?` off, at any width row 1's fixed parts fit (14+).
    for w in 14..=140u16 {
        let row = footer_line(&render_at(&app, w));
        assert!(row.trim_end().ends_with('?'), "the `?` left the row at width {w}:\n{row}");
    }

    // The comments list shows the status too, keeping room for its `…`.
    app.open_list();
    let listed = footer_line(&render_at(&app, 40));
    assert!(listed.contains("no agent in"), "the refusal shows in the list at 40:\n{listed}");
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
    // A short pane caps the footer so the body keeps 3 rows.
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
        comments_truncated: true,
        checks_truncated: true,
        ..common::pr_snapshot()
    }));
    // At narrow width the state line is capped so the primary `o open ↗` is never crowded off.
    let footer = footer_line(&render_at(&app, 60));
    assert!(footer.contains("o open"), "the open action survives a long state line:\n{footer}");
}

#[test]
fn the_pr_footer_names_a_capped_list_in_the_pane() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Check, CheckStatus, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    app.pr = PrView::Pr(Box::new(PrSnapshot { comments_truncated: true, ..common::pr_snapshot() }));
    let footer = footer_line(&render(&app));
    assert!(footer.contains("newest 100 comments"), "{footer}");
    assert!(!footer.contains("+more on"), "{footer}");

    app.pr = PrView::Pr(Box::new(PrSnapshot {
        checks: vec![Check { name: "ci".into(), status: CheckStatus::Success }],
        checks_truncated: true,
        ..common::pr_snapshot()
    }));
    let footer = footer_line(&render(&app));
    assert!(footer.contains("newest 100 checks"), "{footer}");
    assert!(!footer.contains("+more on"), "{footer}");

    app.pr = PrView::Pr(Box::new(PrSnapshot {
        comments_truncated: true,
        checks_truncated: true,
        ..common::pr_snapshot()
    }));
    let footer = footer_line(&render(&app));
    assert!(footer.contains("newest 100 comments"), "{footer}");
    assert!(footer.contains("newest 100 checks"), "{footer}");
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
    // The header shows the resolved head branch and marks a fork.
    app.pr = snap(false);
    let header = render(&app).lines().next().unwrap().to_string();
    assert!(header.contains("persiyanov/feature"), "resolved branch in the header:\n{header}");
    assert!(!header.contains('⑂'), "no fork marker on a same-repo head:\n{header}");
    app.pr = snap(true);
    let header = render(&app).lines().next().unwrap().to_string();
    assert!(header.contains("⑂ persiyanov/feature"), "fork head is marked:\n{header}");
    // Narrow bars drop the branch first; the chip's number stays.
    app.pr = snap(false);
    let narrow = render_at(&app, 58).lines().next().unwrap().to_string();
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
        out.contains("No pull request for a detached HEAD."),
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

/// The editor's failure has to reach the reviewer on the frame it happened, from either pane.
#[test]
fn the_footer_shows_an_editor_failure_from_either_pane() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.status = "editor failed: No such file or directory (os error 2)".to_string();
    let footer = footer_line(&render(&app));
    assert!(footer.contains("editor failed"), "on the read pane:\n{footer}");

    app.focus = herdr_reviewr::app::Focus::Files;
    let footer = footer_line(&render(&app));
    assert!(footer.contains("editor failed"), "and on the navigator:\n{footer}");

    // At the pane width the reviewer actually runs, not only the test default.
    let footer = footer_line(&render_at(&app, 120));
    assert!(footer.contains("editor failed"), "at 120 columns:\n{footer}");
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
    // Scan the header rather than hardcode columns.
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
    // The wheel routes by the pane under the pointer.
    assert!(ui::in_files_pane(AREA, &app, 120, 3));
    assert!(!ui::in_files_pane(AREA, &app, 10, 3));
    // Left pane: diff rows map top-down to diff-line indices.
    assert!(app.visible.len() > 1);
    let heights = ui::diff_row_heights(&app, AREA);
    assert_eq!(ui::hit_diff(AREA, &app, 10, 2, &heights, 0), Some(0));
    assert_eq!(ui::hit_diff(AREA, &app, 10, 3, &heights, 0), Some(1));
    // Rows 2 tall, scrolled by 1: row 1 tops the pane at inner.y == 2.
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
    let (accent, border) =
        (app.palette().mark(Ink::Accent, Fill::Base), app.palette().mark(Ink::Border, Fill::Base));

    let focused_nav = render_buffer(&app);
    assert_eq!(focused_nav.cell((nav_x, body.y + 4)).unwrap().fg, accent);
    assert_eq!(focused_nav.cell((read_x, body.y + 4)).unwrap().fg, border);

    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), AREA, &Keymap::default())
        .unwrap();
    let focused_read = render_buffer(&app);
    assert_eq!(focused_read.cell((nav_x, body.y + 4)).unwrap().fg, border);
    assert_eq!(focused_read.cell((read_x, body.y + 4)).unwrap().fg, accent);
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
    assert!(out.contains("binary file · no line comments"), "binary diff message shown:\n{out}");
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
    // Only a sample that found no member may say the worktree is empty.
    let r = Repo::init();
    r.write("a.rs", "a\n");
    r.commit_all("init");
    let mut app = App::new(r.path_buf(), Scope::LastTurn, None);
    app.reload().unwrap();
    app.sync_turn(herdr_reviewr::turn::TurnReport {
        agents_present: Some(false),
        ..Default::default()
    });
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
    app.sync_turn(herdr_reviewr::turn::TurnReport {
        agents_present: Some(true),
        ..Default::default()
    });
    let out = render(&app);
    assert!(out.contains("waiting for the first turn"), "the pre-turn state shows");
}

#[test]
fn last_turn_before_the_first_sample_waits_rather_than_asserting_emptiness() {
    // Before any poll it may wait, never claim emptiness (Continuity).
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
    // A derived light palette renders, and its accent reaches the buffer.
    let buf = render_buffer(&app);
    let latte = herdr_reviewr::theme::resolve(Some("catppuccin-latte")).palette;
    let latte_blue = latte.mark(Ink::Accent, Fill::Base);
    let painted = (0..40)
        .flat_map(|y| (0..140).map(move |x| (x, y)))
        .any(|(x, y)| buf.cell((x, y)).is_some_and(|c| c.fg == latte_blue));
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
    let app = rebound_app("comment = [\"ㅊ\", \"c\"]\ntab-pr = [\"x\"]\n");
    let out = render(&app);
    let footer = footer_line(&out);
    // A wide hint key spans two buffer cells, so the dump carries a placeholder space after it.
    assert!(footer.contains("ㅊ  comment"), "the hint is the first bound key:\n{footer}");
    assert!(out.contains("x PR"), "the header tab hint follows its binding:\n{out}");
    assert!(!out.contains("3 PR"), "the replaced digit is gone:\n{out}");
}

/// The header columns `hit_header` maps to `tab`, scanned.
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
    // Each label is clickable where it is drawn.
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
fn a_markdown_file_paints_rendered_rows_numbered_by_block() {
    let r = Repo::init();
    r.write("README.md", "# Install\n\nRun `cargo test` for **all** checks.\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);

    // Rendered by default, blocks numbered by source line, the footer offering source.
    app.focus = Focus::Diff;
    let out = render(&app);
    assert!(out.contains("  1 Install"), "the heading's row carries line 1:\n{out}");
    assert!(!out.contains("# Install"), "the # markers are gone rendered:\n{out}");
    assert!(!out.contains("**all**"), "emphasis markers are consumed:\n{out}");
    assert!(out.contains("  3 Run cargo test"), "the paragraph's row carries line 3:\n{out}");
    let footer = out.lines().last().unwrap();
    assert!(footer.contains("m source"), "the footer leads to source:\n{footer}");
    assert!(footer.contains("c comment"), "rendered rows take comments:\n{footer}");

    // Source view: raw markdown, and the footer leads back.
    app.toggle_rendered();
    let source = render(&app);
    assert!(source.contains("# Install"), "source shows raw markdown:\n{source}");
    let footer = source.lines().last().unwrap();
    assert!(footer.contains("m rendered"), "source leads back to the rendered view:\n{footer}");
}

#[test]
fn a_block_is_numbered_on_its_content_never_on_the_gap_above_it() {
    let r = Repo::init();
    r.write("README.md", "> a\n>\n> b\n\n```rust\nlet x = 1;\n```\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    let out = render(&app);
    // A quote's second paragraph: its `▎` gap row is no content, so the number sits on `b`.
    assert!(out.contains("  3 ▎ b"), "the quote's block numbers its text:\n{out}");
    assert!(!out.lines().any(|l| l.contains("  3 ▎ ") && !l.contains('b')), "{out}");
    // A fenced block owns its fences and numbers its code.
    assert!(out.contains("  5   let x = 1;"), "the code block numbers its code:\n{out}");
}

#[test]
fn a_deleted_markdown_file_offers_no_rendered_toggle_in_the_footer() {
    let r = Repo::init();
    r.write("gone.md", "# Doc\n\nbody\n");
    r.commit_all("init");
    r.remove("gone.md");
    let mut app = app_on(&r);
    assert_eq!(app.diff_path.as_deref(), Some("gone.md"));
    app.focus = Focus::Diff;

    // A deleted file never offers the rendered toggle.
    let out = render(&app);
    let footer = out.lines().last().unwrap();
    assert!(footer.contains("c comment"), "a deletion row is commentable:\n{footer}");
    assert!(!footer.contains("m rendered"), "a deleted file offers no rendered view:\n{footer}");
}

#[test]
fn pr_bodies_render_as_markdown_and_the_description_row_pins_first() {
    use herdr_reviewr::forge::{Comment, CommentKind, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    let place = herdr_reviewr::forge::FindingPlace::from_anchor(
        "x.rs:1",
        Some(herdr_reviewr::model::Side::New),
    );
    let finding = Comment {
        kind: CommentKind::Finding,
        author: "codex".into(),
        author_is_bot: true,
        anchor: place.anchor(),
        place: Some(place),
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

    // The finding: the snippet paints as Diff-view rows, the body renders as markdown.
    app.pr_move(1);
    let out = render(&app);
    assert!(out.contains("old"), "the deletion row paints:\n{out}");
    assert!(out.contains("new"), "the insertion row paints:\n{out}");
    assert!(!out.contains("+    new"), "the hunk is not raw +/- text:\n{out}");
    assert!(out.contains("Avoid panics in parse."), "the body renders styled:\n{out}");
    assert!(!out.contains("**panics**"), "markers are consumed:\n{out}");
}

#[test]
fn a_finding_range_paints_as_diff_rows() {
    use herdr_reviewr::forge::{Comment, CommentKind, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    let hunk = format!(
        concat!(
            "@@ -16,10 +16,10 @@\n",
            " OUT_ABOVE\n",
            " a\n",
            " b\n",
            " c\n",
            " ctx\n",
            "-    let x = foo(a);\n",
            "+    let x = bar(a); SNIP_HEAD{}SNIP_TAIL\n",
            " tail\n",
            " d\n",
            " e\n",
            " OUT_BELOW\n",
        ),
        "x".repeat(80),
    );
    let finding = |anchor: &str, body: &str| {
        let place = herdr_reviewr::forge::FindingPlace::from_anchor(
            anchor,
            Some(herdr_reviewr::model::Side::New),
        );
        Comment {
            kind: CommentKind::Finding,
            author: "codex".into(),
            author_is_bot: true,
            anchor: place.anchor(),
            place: Some(place),
            body: body.into(),
            snippet: Some(hunk.clone()),
            ..common::comment()
        }
    };
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        comments: vec![finding("x.rs:21", "keep this"), finding("x.rs:16", "second finding")],
        ..common::pr_snapshot()
    }));
    app.wrap = false;
    let out = render(&app);
    // The nav label is `x.rs:21`; the gutter must also paint in the read pane.
    let read = out
        .lines()
        .map(|l| l.chars().take(l.chars().count() * 68 / 100).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        read.lines().any(|l| l.contains("21") && l.contains("foo")),
        "the gutter 21 sits on the deletion row:\n{read}"
    );
    assert!(out.contains("foo"), "the deletion in range paints:\n{out}");
    assert!(out.contains("bar"), "the insertion in range paints:\n{out}");
    assert!(out.contains("SNIP_TAIL"), "the snippet wraps even when wrap is off:\n{out}");
    assert!(out.contains("ctx") && out.contains("tail"), "the three-line margin paints:\n{out}");
    assert!(!out.contains("OUT_ABOVE"), "context beyond the margin is omitted:\n{out}");
    assert!(!out.contains("OUT_BELOW"), "following context beyond the margin is omitted:\n{out}");
    assert!(!out.contains("@@"), "the hunk header does not paint:\n{out}");
    assert!(
        out.contains("Comment on line +21"),
        "an insertion in the range keeps the + sign:\n{out}"
    );
    assert!(out.contains("keep this"), "the body follows the range:\n{out}");

    app.pr_move(1);
    let out = render(&app);
    assert!(out.contains("Comment on line 16"), "a context range has no sign:\n{out}");
    assert!(out.contains("OUT_ABOVE"), "the other finding's range paints:\n{out}");
    assert!(!out.contains("foo"), "the first finding's deletion does not linger:\n{out}");
    assert!(!out.contains("bar"), "the first finding's insertion does not linger:\n{out}");
    assert!(out.contains("second finding"), "the selected body follows its range:\n{out}");
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

    // Description, blank, checks header, a check, blank, comments header, comments.
    let area = Rect::new(0, 0, 140, 40);
    let x = 130; // inside the nav pane
    let hit = |app: &App, y: u16| {
        ui::pr_nav_display_row(area, app, x, y, false)
            .and_then(|row| ui::pr_nav_cursor_at(app, row))
    };
    assert_eq!(hit(&app, 2), Some(0), "click on the description row");
    assert_eq!(hit(&app, 5), None, "a check row is not a cursor stop");
    assert_eq!(hit(&app, 8), Some(1), "first comment maps past the offset");
    assert_eq!(hit(&app, 9), Some(2), "second comment follows");
    assert_eq!(hit(&app, 10), None, "past the last comment is dead");
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
            &herdr_reviewr::export::Clipboard,
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
            &herdr_reviewr::export::Clipboard,
        )
        .unwrap();
    }
    let scrolled = render(&app);
    assert!(scrolled.contains("@author-00"), "the wheel exposes overflowed comments:\n{scrolled}");
    assert_eq!(app.pr_selected_comment().map(|c| c.author.clone()), selected);

    // A click acts at release, so press and release an unselected comment.
    let current = app.pr_selected_comment().map(|c| c.author.clone());
    let (clicked_row, clicked_author) = scrolled
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            comments
                .iter()
                .find(|comment| line.contains(&format!("@{}", comment.author)))
                .filter(|comment| Some(&comment.author) != current.as_ref())
                .map(|comment| (row as u16, comment.author.clone()))
        })
        .expect("a scrolled, unselected comment row is painted");
    for kind in [
        MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
        MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left),
    ] {
        handle_mouse(
            &mut app,
            MouseEvent {
                kind,
                column: body.x + 2,
                row: clicked_row,
                modifiers: KeyModifiers::NONE,
            },
            area,
            &[],
            &keymap,
            &herdr_reviewr::export::Clipboard,
        )
        .unwrap();
    }
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

    // The reserved cell means the glyph's appearance shifts nothing.
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

    // The empty state speaks the forge's noun.
    app.apply_pr(PrView::NoPr);
    let out = render(&app);
    assert!(out.contains("No merge request yet"), "GitLab empty state:\n{out}");

    // The chip uses GitLab's reference form.
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot { number: 42, ..common::pr_snapshot() })));
    let out = render(&app);
    assert!(out.contains("!42"), "MR reference form:\n{out}");
    assert!(!out.contains("#42"), "no GitHub reference form on GitLab:\n{out}");

    // Each failure names its own CLI and login command.
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

    // The empty state speaks the forge's noun.
    app.apply_pr(PrView::NoPr);
    let out = render(&app);
    assert!(out.contains("No pull request yet"), "Azure DevOps empty state:\n{out}");

    // The chip uses the `#` reference form.
    app.apply_pr(PrView::Pr(Box::new(PrSnapshot { number: 12, ..common::pr_snapshot() })));
    let out = render(&app);
    assert!(out.contains("#12"), "PR reference form:\n{out}");

    // Each failure names its own CLI, extension, and login command.
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
fn an_anchor_click_moves_the_rendered_cursor_to_its_heading() {
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
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);

    // In source view an anchor click is inert: no heading anchors are rendered there.
    app.toggle_rendered();
    let _ = render(&app);
    app.open_link("#section-two");
    assert_eq!(app.diff_cursor, 0, "source view ignores anchor destinations");

    app.toggle_rendered();
    let _ = render(&app);
    assert_eq!(app.diff_cursor, 0);
    app.open_link("#section-two");
    assert!(app.diff_cursor > 40, "the cursor jumped to the heading: {}", app.diff_cursor);
    assert_eq!(app.visible[app.diff_cursor].text(), "Section Two");
    assert_eq!(app.diff_scroll, app.diff_cursor, "the heading tops the pane");
    let out = render(&app);
    assert!(out.contains("Section Two"), "the heading is on screen:\n{out}");
    assert!(!out.contains("jump go"), "the top scrolled away:\n{out}");
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

    // Overflowing, the same pane paints the thumb, so the absence above is real.
    let mut long = String::new();
    for i in 0..80 {
        let _ = writeln!(long, "line {i}\n");
    }
    app.pr = PrView::Pr(Box::new(PrSnapshot { body: long, ..common::pr_snapshot() }));
    let out = render(&app);
    assert!(out.contains('┃'), "an overflowing PR body shows the thumb:\n{out}");
}

#[test]
fn rendered_rows_paint_link_and_details_regions() {
    let r = Repo::init();
    r.write(
        "README.md",
        "# Install\n\nsee [docs](https://docs.example/x)\n\n\
         <details>\n<summary>More</summary>\n\nhidden body\n\n</details>\n",
    );
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let cell_of = |buf: &Buffer, needle: &str| -> (u16, u16) {
        let out = dump(buf);
        let (y, line) =
            out.lines().enumerate().find(|(_, l)| l.contains(needle)).expect("painted needle");
        let x = line[..line.find(needle).unwrap()].chars().count();
        (u16::try_from(x).unwrap(), u16::try_from(y).unwrap())
    };

    // The link's first cell resolves; the cell before it does not.
    let buf = render_buffer(&app);
    assert!(dump(&buf).contains("README.md · rendered"), "the title names the mode");
    let (x, y) = cell_of(&buf, "docs");
    assert_eq!(app.painted_link_at(x, y).as_deref(), Some("https://docs.example/x"));
    assert_eq!(app.painted_link_at(x - 1, y), None, "the region starts at the link text");

    // A click on the summary opens the disclosure in place.
    let (x, y) = cell_of(&buf, "More");
    assert_eq!(app.painted_details_at(x, y).as_deref(), Some("More#0"));
    assert!(!dump(&buf).contains("hidden body"), "the disclosure starts collapsed");
    let area = Rect::new(0, 0, 140, 40);
    let heights = ui::diff_row_heights(&app, area);
    let keymap = Keymap::default();
    for kind in [
        MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
        MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left),
    ] {
        let m = MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE };
        handle_mouse(&mut app, m, area, &heights, &keymap, &herdr_reviewr::export::Clipboard)
            .unwrap();
    }
    assert!(render(&app).contains("hidden body"), "the click opened the disclosure");

    // Source paints no regions.
    app.toggle_rendered();
    let source = render(&app);
    assert!(!source.contains("· rendered"), "source view has no rendered marker");
    assert_eq!(first_painted_link(&app), None, "raw source paints no link regions");
}

#[test]
fn the_changes_tab_paints_rendered_markdown() {
    let r = Repo::init();
    r.write("README.md", "# Install\n");
    r.commit_all("init");
    r.write("README.md", "# Install\n\nRun `cargo test` for **all** checks.\n");
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;

    // The Changes tab opens the markdown file rendered and names the mode in the title.
    let out = render(&app);
    assert!(out.contains("README.md · rendered"), "the title names the mode:\n{out}");
    assert!(out.contains("Install"), "the heading text renders:\n{out}");
    assert!(!out.contains("# Install"), "the # markers are gone rendered:\n{out}");
    // "checks" exists only on the new side.
    assert!(out.contains("checks"), "the render shows the new-side content:\n{out}");
    let footer = out.lines().last().unwrap();
    assert!(footer.contains("m source"), "the footer leads to the diff:\n{footer}");

    // The toggle paints the diff of the raw markdown.
    app.toggle_rendered();
    let source = render(&app);
    assert!(source.contains("# Install"), "the diff shows raw markdown:\n{source}");
    let footer = source.lines().last().unwrap();
    assert!(footer.contains("m rendered"), "the diff leads back:\n{footer}");
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
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    let _ = render(&app);

    // The click side must Unicode-lowercase like the slugger: #ÜBER-ZIEL → über-ziel.
    app.open_link("#ÜBER-ZIEL");
    assert!(app.diff_cursor > 40, "the jump matched the slug: {}", app.diff_cursor);
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
            place: Some(herdr_reviewr::forge::FindingPlace::from_anchor(
                "x.rs:1",
                Some(herdr_reviewr::model::Side::New),
            )),
            body,
            snippet: Some("-    old\n+    new".into()),
            ..common::comment()
        }],
        ..common::pr_snapshot()
    }));
    let out = render(&app);
    assert!(out.contains("new"), "the snippet paints above the body:\n{out}");

    // The jump lands on the heading, past the snippet.
    app.open_link("#target");
    let out = render(&app);
    assert!(out.contains("Target"), "the heading is on screen:\n{out}");
    assert!(!out.contains("new"), "the snippet scrolled away:\n{out}");
    assert!(!out.contains("jump go"), "the body's top scrolled away:\n{out}");
}

#[test]
fn a_finding_paints_its_replies_in_the_read_pane() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Comment, CommentKind, PrSnapshot, PrView, Reply};
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
            place: Some(herdr_reviewr::forge::FindingPlace::from_anchor(
                "x.rs:1",
                Some(herdr_reviewr::model::Side::New),
            )),
            body: "the finding".into(),
            replies: vec![Reply {
                author: "persijano".into(),
                author_is_bot: false,
                body: "Addressed in abc".into(),
                created_at: "2026-06-27T11:30:00Z".into(),
            }],
            ..common::comment()
        }],
        ..common::pr_snapshot()
    }));
    let out = render(&app);
    assert!(out.contains("the finding"), "{out}");
    assert!(out.contains("@codex · "), "root byline with age:\n{out}");
    assert!(out.contains("@persijano · "), "reply byline with age:\n{out}");
    assert!(out.contains("Addressed in abc"), "{out}");
    assert!(out.contains('─'), "a rule separates turns:\n{out}");
    assert!(!out.contains("open on"), "{out}");
    assert!(!out.contains("↳"), "{out}");
}

#[test]
fn details_expand_on_the_pr_tab_and_reset_on_row_change() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Comment, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Pr).unwrap();
    let body = "<details> <summary>About Codex</summary>\n\nchrome lives here\n\n</details>";
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        comments: vec![
            Comment { author: "codex".into(), body: body.into(), ..common::comment() },
            Comment { author: "ann".into(), body: "plain".into(), ..common::comment() },
        ],
        ..common::pr_snapshot()
    }));
    let out = render(&app);
    assert!(out.contains("About Codex"), "{out}");
    assert!(!out.contains("chrome lives here"), "{out}");

    app.expand_pr_details();
    let out = render(&app);
    assert!(out.contains("chrome lives here"), "{out}");

    app.apply_pr(PrView::Pr(Box::new(PrSnapshot {
        comments: vec![
            Comment { author: "codex".into(), body: body.into(), ..common::comment() },
            Comment { author: "ann".into(), body: "plain".into(), ..common::comment() },
        ],
        ..common::pr_snapshot()
    })));
    let out = render(&app);
    assert!(out.contains("chrome lives here"), "same thread keeps it open:\n{out}");

    app.pr_move(1);
    let out = render(&app);
    assert!(out.contains("plain"), "{out}");
    assert!(!out.contains("chrome lives here"), "row change collapses:\n{out}");

    app.pr_move(-1);
    let _ = render(&app);
    let hit = (0..40u16)
        .flat_map(|y| (0..140u16).map(move |x| (x, y)))
        .find_map(|(x, y)| app.painted_details_at(x, y));
    let key = hit.expect("summary is clickable after a paint");
    app.toggle_details(&key);
    let out = render(&app);
    assert!(out.contains("chrome lives here"), "click opens:\n{out}");
}

// In-file find rendering.
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
    let fill = app.palette().fill(Fill::Highlight);
    let ink = app.palette().ink(Ink::Text, Fill::Highlight);
    let highlighted = (0..40u16).flat_map(|y| (0..140u16).map(move |x| (x, y))).any(|(x, y)| {
        buf.cell((x, y)).is_some_and(|c| c.symbol() == "t" && c.bg == fill && c.fg == ink)
    });
    assert!(highlighted, "a matched character reverses to the bright find highlight");
}

// Search screen rendering.
mod search_screen_render {
    use super::{Fill, common, dump, render, render_size};
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
    fn the_band_anchors_the_terminal_cursor_at_its_caret() {
        let repo = Repo::init();
        repo.write("src/registry.rs", "fn resolve() {}\n");
        repo.commit_all("c");
        let mut app = open_on_all_files(&repo);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| ui::render(f, &app)).unwrap();
        let empty = terminal.backend().cursor_position();

        key(&mut app, KeyCode::Char('日'));
        terminal.draw(|f| ui::render(f, &app)).unwrap();

        let after = terminal.backend().cursor_position();
        assert_eq!(
            (after.x, after.y),
            (empty.x + 2, empty.y),
            "the cursor advances one wide character"
        );
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
        assert!(out.contains("↑↓ move") && out.contains("enter open"), "the screen's footer shows");

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

        // An empty query lists nothing in Code mode — no copy at all.
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
            handle_mouse(
                app,
                event,
                AREA,
                &[],
                default_keymap(),
                &herdr_reviewr::export::Clipboard,
            )
            .unwrap();
        };

        // A click on an unpicked row picks it; a second click opens it.
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
        handle_mouse(
            &mut app,
            event,
            AREA,
            &[],
            default_keymap(),
            &herdr_reviewr::export::Clipboard,
        )
        .unwrap();
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
            Some(app.palette().fill(Fill::Highlight)),
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
        // The engine's offsets skip indentation the preview keeps; the highlight must shift.
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
        // The preview row is where the spans had to shift.
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
            Some(app.palette().fill(Fill::Highlight)),
            "the highlight lands on the match under indentation",
        );
        // The indentation and the preceding `fn ` keep the cursor band, not the match highlight.
        let fx = line.find("fn ").unwrap() as u16;
        assert_ne!(
            buf.cell((fx, y)).unwrap().style().bg,
            Some(app.palette().fill(Fill::Highlight)),
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
        // A head-elided path still marks a match in its shown tail.
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
            Some(app.palette().fill(Fill::Highlight)),
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
        // A narrow band scrolls the head off, keeping the tail and caret.
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
        // A Files result wears the file list's marker and stats.
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
            Some(app.palette().fill(Fill::Highlight)),
            "the match highlight lands on the matched path character",
        );
    }

    #[test]
    fn a_poll_refreshes_the_open_preview_in_place() {
        // Through the real reload path, the preview follows a poll; results stay (Continuity).
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

// Style-level emphasis coverage for the match rows.
mod search_row_emphasis {
    use super::{Fill, common, dump, render_size};
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

    /// A too-wide code row clips around its match, keeping `line:` and marking the cut.
    #[test]
    fn clipped_code_row_keeps_and_emphasizes_the_match() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('/')), AREA, default_keymap()).unwrap();

        // A long head pushes the match off the pane.
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
            Some(app.palette().fill(Fill::Highlight)),
            "the matched span wears the match highlight",
        );
        // The clipped head keeps the selection fill, not the match highlight.
        let ell = row.find('…').unwrap();
        let head_x = row[..ell].chars().count() as u16 + 1;
        assert_ne!(
            buf.cell((head_x, y)).expect("cell").style().bg,
            Some(app.palette().fill(Fill::Highlight)),
            "the clipped head is not highlighted",
        );
    }

    /// A tab-indented code row expands its tabs, the emphasis still on the match.
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
            Some(app.palette().fill(Fill::Highlight)),
            "the highlight tracks the word past the expanded tabs: {style:?}"
        );
    }

    /// A multi-byte head through the clip paints rather than panics.
    #[test]
    fn clipped_multibyte_code_row_paints() {
        let repo = Repo::init();
        repo.write("a.rs", "fn a() {}\n");
        repo.commit_all("c");
        let mut app = app_on(&repo);
        enter_tab(&mut app, Tab::AllFiles);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('/')), AREA, default_keymap()).unwrap();

        // `中` is 3 bytes and 2 columns, so bytes and columns diverge.
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
        // The first highlighted cell is `needle`'s `n`.
        let hx = (0..buf.area.width)
            .find(|&x| {
                buf.cell((x, y)).expect("cell").style().bg
                    == Some(app.palette().fill(Fill::Highlight))
            })
            .expect("the match is highlighted");
        assert_eq!(
            buf.cell((hx, y)).expect("cell").symbol(),
            "n",
            "the match highlight lands on the match, past the multibyte head",
        );
    }
}

// --- Agent picker ----------------------------------------------------

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
    // A prior send arms and tags its row.
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

    // The tab bar recedes under the picker; find a lettered cell, not a fixed column.
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
        (0..dimmed.area.width).any(|x| dimmed.cell((x, footer_y)).is_some_and(|c| c.fg == ACCENT));
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

    // At every size the popups leave the footer visible.
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

    assert!(out.contains("send 3 comments to"), "the title counts the comments:\n{out}");

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

    // Even an over-tall picker leaves the footer visible.
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
    // The title row and everything outside the popup are inert.
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
        &herdr_reviewr::export::Clipboard,
    )
    .unwrap();
    assert_eq!(app.picker_cursor, 2, "a click moves the highlight to the clicked row");
}

// --- Header base label ------------------------------------------------------

/// Branch `feature` past default `main`; the repo is returned for the picker's git calls.
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
    handle_mouse(&mut app, click, AREA, &[], &keymap, &herdr_reviewr::export::Clipboard).unwrap();
    let frame = render(&app);
    assert!(frame.contains("base · 3 branches"), "the click opens the picker popup");
    assert!(frame.contains("dev"), "the sibling branch is a row");
    assert!(frame.contains("default"), "the default branch is marked");
    assert!(frame.contains("current"), "the checked-out branch is marked");
    assert!(!frame.contains('★'), "no glyph: the trail words carry the facts");

    // Filter, three rows, two borders, and no idle probe row.
    let top = frame.lines().position(|l| l.contains("┌ base")).unwrap();
    let bottom = frame.lines().skip(top).position(|l| l.contains("└────")).unwrap();
    assert_eq!(bottom, 5, "top border, filter line, three rows, bottom border: {frame}");
}

#[test]
fn the_picker_title_counts_matches_while_filtering() {
    let (r, mut app) = based_app();
    app.open_base_picker();
    assert!(render(&app).contains("base · 3 branches"));
    app.input_push('d');
    assert!(render(&app).contains("base · 1/3"), "matched over total: {}", render(&app));
    app.close_base_picker();

    // A non-branch pick is listed but never counted.
    herdr_reviewr::git::write_base_pick(r.path(), "HEAD~1").unwrap();
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    let frame = render(&app);
    assert!(frame.contains("HEAD~1"), "{frame}");
    assert!(frame.contains("base · 3 branches"), "{frame}");
    app.input_push('e');
    let frame = render(&app);
    assert!(
        frame.contains("base · 2/3"),
        "dev and feature match, the rev row counts nowhere: {frame}"
    );
}

#[test]
fn a_probe_row_still_fits_when_every_branch_matches() {
    // A probe hit grows a full box by one row.
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["checkout", "-q", "-b", "v1.2-hotfix"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    r.git(&["branch", "-D", "main"]);
    r.git(&["tag", "v1.2", "HEAD~1"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "v1.2".chars() {
        app.input_push(ch);
    }
    app.run_base_probe();
    let frame = render(&app);
    assert!(frame.contains("v1.2-hotfix"), "{frame}");
    assert!(frame.contains('(') && frame.contains("v1.2 "), "the tag row paints: {frame}");
    let rows: std::collections::BTreeSet<usize> = (0..AREA.height)
        .flat_map(|row| (0..AREA.width).map(move |col| (col, row)))
        .filter_map(|(col, row)| ui::hit_base_picker_row(AREA, &app, col, row))
        .collect();
    assert_eq!(rows.into_iter().collect::<Vec<_>>(), [0, 1], "the hit row is inside the box");
}

fn now_minus(secs: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(secs)
}

/// A row carrying every trail word, painted at a width that fits and at widths that do not.
fn worst_case_picker(app: &mut App) {
    app.base_picker = Some(BasePicker {
        rows: vec![BaseChoice::Branch {
            name: "main".to_string(),
            pr_base: true,
            is_default: true,
            current: true,
            tip_secs: now_minus(2 * 3600),
        }],
        cursor: 0,
        query: String::new(),
        caret: 0,
        probe: BaseProbe::Idle,
    });
    app.mode = Mode::BasePick;
}

#[test]
fn a_narrow_pane_sheds_trail_words_before_the_name() {
    let mut app = edited_app();
    worst_case_picker(&mut app);
    let wide = dump(&render_size(&app, 80, 20));
    assert!(wide.contains("main"), "{wide}");
    assert!(wide.contains("pr base · default · current · 2h"), "every word fits at 80: {wide}");

    // Narrow, the trail drops from the right before the name clips.
    let narrow = dump(&render_size(&app, 34, 20));
    assert!(narrow.contains("main"), "{narrow}");
    assert!(narrow.contains("pr base"), "the first word survives: {narrow}");
    assert!(!narrow.contains("2h"), "the age goes first: {narrow}");
    let tiny = dump(&render_size(&app, 14, 20));
    assert!(tiny.contains("main"), "the name outlives every word: {tiny}");
    assert!(!tiny.contains("pr base"), "{tiny}");
}

#[test]
fn a_probe_row_is_clickable_below_the_matches() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["branch", "v1.2-hotfix"]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    r.git(&["tag", "v1.2", "HEAD~1"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "v1.2".chars() {
        app.input_push(ch);
    }
    app.run_base_probe();
    let frame = render(&app);
    assert!(frame.contains("v1.2-hotfix"), "{frame}");
    let hits: Vec<(u16, u16, usize)> = (0..AREA.height)
        .flat_map(|row| (0..AREA.width).map(move |col| (col, row)))
        .filter_map(|(col, row)| {
            ui::hit_base_picker_row(AREA, &app, col, row).map(|i| (col, row, i))
        })
        .collect();
    let rows: std::collections::BTreeSet<usize> = hits.iter().map(|h| h.2).collect();
    assert_eq!(
        rows.into_iter().collect::<Vec<_>>(),
        [0, 1],
        "both rows hit-test, the probe row too"
    );
    let probe_y = hits.iter().find(|h| h.2 == 1).unwrap().1;
    let branch_y = hits.iter().find(|h| h.2 == 0).unwrap().1;
    assert_eq!(probe_y, branch_y + 1, "the probe row sits under the match");
}

#[test]
fn a_named_rev_paints_the_spelling_and_abbrev() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    herdr_reviewr::git::write_base_pick(r.path(), "HEAD~1").unwrap();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    assert!(
        line0.contains(&format!("vs HEAD~1 ({short})")),
        "a named rev paints the spelling and the abbreviated SHA: {line0}"
    );
}

#[test]
fn a_sha_pick_paints_once() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    herdr_reviewr::git::write_base_pick(r.path(), &parent).unwrap();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(line0.contains(&format!("vs {short}")), "a SHA spelling paints once: {line0}");
    assert!(
        !line0.contains(&format!("vs {short} (")),
        "a SHA spelling does not repeat as a marker: {line0}"
    );

    herdr_reviewr::git::write_base_pick(r.path(), &short).unwrap();
    app.reload().unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(
        line0.contains(&format!("vs {short}")),
        "an abbreviated SHA spelling paints once: {line0}"
    );
    assert!(
        !line0.contains(&format!("vs {short} (")),
        "an abbreviated SHA spelling does not repeat as a marker: {line0}"
    );
}

#[test]
fn a_flag_named_rev_paints_the_same_form() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let mut app = App::new(r.path_buf(), Scope::Branch, Some("HEAD~1".to_string()));
    app.reload().unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    assert!(
        line0.contains(&format!("vs HEAD~1 ({short})")),
        "the --base flag uses the same paint: {line0}"
    );
}

#[test]
fn a_probe_row_is_the_typed_spelling() {
    let (r, mut app) = based_app();
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    app.open_base_picker();
    for ch in "HEAD~1".chars() {
        app.input_push(ch);
    }
    app.run_base_probe();
    let frame = render(&app);
    assert!(frame.contains("HEAD~1"), "the probe row is the typed spelling:\n{frame}");
    assert!(
        frame.contains(&format!("({short})")),
        "a named rev is marked with the abbreviated SHA:\n{frame}"
    );
}

#[test]
fn a_probe_row_right_aligns_the_sha_like_the_open_list() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["branch", "dev"]);
    r.git(&["branch", &format!("release/{}", "x".repeat(40))]);
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    app.open_base_picker();
    for ch in "HEAD~1".chars() {
        app.input_push(ch);
    }
    app.run_base_probe();
    let marker = format!("({short})");
    let picker_row = |frame: &str| {
        frame
            .lines()
            .find(|l| l.contains(&marker) && !l.contains("vs "))
            .expect("the picker row paints")
            .to_string()
    };
    let probe = picker_row(&render(&app));
    let probe_at = probe.find(&marker).unwrap();
    app.base_picker_pick().unwrap();
    app.open_base_picker();
    let open = picker_row(&render(&app));
    let open_at = open.find(&marker).unwrap();
    assert_eq!(
        probe_at, open_at,
        "typing a rev puts `(sha)` in the same column as opening the list:\nopen: {open}\nprobe: {probe}"
    );
    let name_end = probe.find("HEAD~1").expect("the spelling paints") + "HEAD~1".len();
    assert!(probe_at > name_end + 2, "the SHA is right-aligned, not glued to the name:\n{probe}");
}

#[test]
fn a_short_sha_prefix_probe_completes_to_the_abbrev() {
    let (r, mut app) = based_app();
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    let prefix = short[..4].to_string();
    app.open_base_picker();
    for ch in prefix.chars() {
        app.input_push(ch);
    }
    app.run_base_probe();
    let bp = app.base_picker.as_ref().unwrap();
    assert_eq!(bp.visible()[0].name(), short, "the row is the abbreviated SHA, not the prefix");
    let frame = render(&app);
    assert!(
        frame.lines().any(|l| l.contains(&short) && !l.contains(&format!("({short})"))),
        "a unique prefix completes to the abbreviated SHA with no marker:\n{frame}"
    );
}

#[test]
fn a_seven_char_sha_probe_is_not_marked() {
    let (r, mut app) = based_app();
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    app.open_base_picker();
    for ch in short.chars() {
        app.input_push(ch);
    }
    app.run_base_probe();
    let frame = render(&app);
    assert!(frame.contains(&short), "the probe row is the abbreviated SHA:\n{frame}");
    assert!(
        !frame.contains(&format!("({short})")),
        "a spelling that already is that SHA carries no marker:\n{frame}"
    );
}

#[test]
fn a_skipped_named_rev_uses_the_stored_spelling() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    herdr_reviewr::git::write_base_pick(r.path(), "HEAD~1").unwrap();
    r.git(&["checkout", "-q", "-b", "feature"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(
        line0.contains("vs main · HEAD~1 missing"),
        "a skipped non-branch spelling uses the stored spelling: {line0}"
    );
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
    r.git(&["branch", "-m", "main", "trunk"]); // no `main`/`master`: no default to fall back on
    r.git(&["checkout", "-q", "-b", "feature"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let frame = render(&app);
    let line0 = frame.lines().next().unwrap().to_string();
    assert!(line0.contains("[branch] no base"), "the empty state is named: {line0}");
    assert!(frame.contains("B base"), "the footer advertises the picker");
}

#[test]
fn a_local_only_repo_has_its_main_as_the_base() {
    // Without a remote, a local `main` is the default.
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(line0.contains("[branch] vs main"), "the local main is the base: {line0}");
    assert!(!line0.contains("missing"), "nothing is skipped: {line0}");
    assert!(line0.contains("1 changed"), "the branch diffs against it: {line0}");

    // On `main` itself the base is still `main`: the scope is the uncommitted diff.
    r.git(&["checkout", "-q", "main"]);
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(line0.contains("[branch] vs main"), "{line0}");
}

#[test]
fn a_dormant_pick_shows_beside_the_empty_state() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.git(&["branch", "-m", "main", "trunk"]); // no `main`/`master`: no default to fall back on
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
fn a_named_rev_clips_the_spelling_and_keeps_the_sha() {
    let r = Repo::init();
    r.write("hello.rs", "alpha\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("hello.rs", "alpha\nBETA\n");
    r.commit_all("edit");
    let parent = r.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    let long = format!("release-{}", "x".repeat(80));
    r.git(&["tag", &long, &parent]);
    herdr_reviewr::git::write_base_pick(r.path(), &long).unwrap();
    let mut app = app_on(&r);
    app.set_scope(Scope::Branch).unwrap();
    let line0 = dump(&render_size(&app, 80, 20)).lines().next().unwrap().to_string();
    let short = herdr_reviewr::git::abbreviate_oid(&parent);
    assert!(line0.contains(&format!("({short})")), "the SHA marker survives the clip: {line0}");
    assert!(line0.contains('…'), "the spelling truncates: {line0}");
    assert!(line0.contains("1 changed"), "the right-aligned stats survive: {line0}");
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

    // Truncated, the label's hit columns are exactly its painted run.
    for width in [52u16, 68, 84] {
        let area = Rect { x: 0, y: 0, width, height: 12 };
        let line0 = dump(&render_size(&app, width, 12)).lines().next().unwrap().to_string();
        let cells: Vec<char> = line0.chars().collect();
        let hits: Vec<u16> = (0..width)
            .filter(|&c| ui::hit_header(area, &app, app.keymap(), c, 0) == Some(HeaderHit::Base))
            .collect();
        let Some((&first, &last)) = hits.first().zip(hits.last()) else {
            // No room for a name: no `vs`, no claim.
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
    let line0 = dump(&render_size(&app, 92, 20)).lines().next().unwrap().to_string();
    assert!(line0.contains("vs main"), "the resolved name keeps first claim: {line0}");
    assert!(line0.contains("· feature/x"), "the skipped tail paints in what remains: {line0}");
    assert!(line0.contains('…'), "the tail truncates with a trailing ellipsis: {line0}");
    assert!(line0.contains("1 changed"), "the right-aligned stats survive the long tail: {line0}");
}

// ---- mouse text selection ----

/// A repo with one uncommitted three-line file, for selection geometry.
fn selection_app() -> (Repo, App) {
    let r = Repo::init();
    r.write("base.rs", "fn main() {}\n");
    r.commit_all("init");
    r.write("m.rs", "alpha beta\n\tif x {\n日本 z\n");
    let app = app_on(&r);
    (r, app)
}

#[test]
fn the_hovered_row_shows_a_plus_in_its_change_bar_cell() {
    let (_repo, mut app) = selection_app();
    let area = Rect::new(0, 0, 140, 40);
    let inner = ui::read_inner_rect(area, &app);

    // No hover: the insertion row paints its change bar.
    let buf = render_buffer(&app);
    assert_eq!(buf.cell((inner.x, inner.y)).unwrap().symbol(), "▌");

    // Hover puts `[+]` over the number, never the change bar.
    app.hover = Some((inner.x + 8, inner.y));
    let buf = render_buffer(&app);
    assert_eq!(buf.cell((inner.x, inner.y)).unwrap().symbol(), "▌");
    assert_eq!(buf.cell((inner.x + 1, inner.y)).unwrap().symbol(), "[");
    assert_eq!(buf.cell((inner.x + 2, inner.y)).unwrap().symbol(), "+");
    assert_eq!(buf.cell((inner.x + 3, inner.y)).unwrap().symbol(), "]");
    // The unhovered row below keeps its bar and number.
    assert_eq!(buf.cell((inner.x, inner.y + 1)).unwrap().symbol(), "▌");
}

#[test]
fn the_plus_button_right_aligns_in_a_wide_number_field() {
    // A wider number field right-aligns `[+]`, like its numbers.
    let r = Repo::init();
    r.write("base.rs", "fn main() {}\n");
    r.commit_all("init");
    let body = (1..=1000).fold(String::new(), |mut s, i| {
        use std::fmt::Write;
        let _ = writeln!(s, "line {i}");
        s
    });
    r.write("long.rs", &body);
    let mut app = app_on(&r);
    let area = Rect::new(0, 0, 140, 40);
    let inner = ui::read_inner_rect(area, &app);

    app.hover = Some((inner.x + 8, inner.y));
    let buf = render_buffer(&app);
    assert_eq!(buf.cell((inner.x, inner.y)).unwrap().symbol(), "▌");
    assert_eq!(buf.cell((inner.x + 1, inner.y)).unwrap().symbol(), " ", "left pad, not `[`");
    assert_eq!(buf.cell((inner.x + 2, inner.y)).unwrap().symbol(), "[");
    assert_eq!(buf.cell((inner.x + 3, inner.y)).unwrap().symbol(), "+");
    assert_eq!(buf.cell((inner.x + 4, inner.y)).unwrap().symbol(), "]");
    // The unhovered row below right-aligns its number in the same field.
    assert_eq!(buf.cell((inner.x + 4, inner.y + 1)).unwrap().symbol(), "2");
}

#[test]
fn the_text_selection_highlights_the_dragged_span() {
    use herdr_reviewr::selection::{Point, Surface, TextDrag};
    let (_repo, mut app) = selection_app();
    let area = Rect::new(0, 0, 140, 40);
    let inner = ui::read_inner_rect(area, &app);
    let sel_bg = app.palette().fill(Fill::Selection);
    // The cursor on a selected row: the selection fill still wins.
    app.diff_cursor = 1;

    // `beta` on row 0 through char 1 (`本`) of row 2: a three-row stream selection.
    app.gesture = herdr_reviewr::selection::Gesture::Text {
        drag: TextDrag {
            surface: Surface::Read,
            anchor: Point { row: 0, chr: 6 },
            extent: Point { row: 2, chr: 1 },
        },
        count: 1,
    };
    let buf = render_buffer(&app);
    let bg = |x: u16, y: u16| buf.cell((x, y)).unwrap().style().bg;
    // The first row runs from the anchor character, not whole.
    assert_eq!(bg(inner.x + 5 + 6, inner.y), Some(sel_bg));
    assert_ne!(bg(inner.x + 5, inner.y), Some(sel_bg));
    assert_ne!(bg(inner.x + 5 + 5, inner.y), Some(sel_bg));
    // Row 1 lies whole between the endpoints: tab expansion through its last char.
    assert_eq!(bg(inner.x + 5, inner.y + 1), Some(sel_bg));
    assert_eq!(bg(inner.x + 5 + 6, inner.y + 1), Some(sel_bg));
    // The last row ends at its end character, both wide glyphs included.
    assert_eq!(bg(inner.x + 5, inner.y + 2), Some(sel_bg));
    assert_eq!(bg(inner.x + 5 + 2, inner.y + 2), Some(sel_bg));
    assert_ne!(bg(inner.x + 5 + 4, inner.y + 2), Some(sel_bg));
}

// --- Commit picker and the commits header --------------------

/// `main` with four commits and an uncommitted edit; the shas root first.
fn commits_app() -> (Repo, App, Vec<String>) {
    let r = Repo::init();
    r.write("root.rs", "r\n");
    r.commit_all("root");
    r.write("one.rs", "1\n");
    r.commit_all("one");
    r.write("two.rs", "2\n");
    r.commit_all("two");
    r.write("three.rs", "3\n");
    r.commit_all("Stop counting git's own lock files");
    r.write("root.rs", "dirty\n");
    let shas: Vec<String> =
        r.git(&["rev-list", "--reverse", "HEAD"]).lines().map(str::to_string).collect();
    let app = app_on(&r);
    (r, app, shas)
}

fn short(sha: &str) -> &str {
    &sha[..7]
}

#[test]
fn the_commit_picker_paints_rows_a_run_bar_and_its_count() {
    let (r, mut app, shas) = commits_app();
    app.open_commit_picker();
    app.commit_picker_anchor();
    app.commit_picker_move(2);
    let buf = render_buffer(&app);
    let out = dump(&buf);
    assert!(out.contains("commits · last 50"), "the title names the universe:\n{out}");
    assert!(out.contains(short(&shas[3])), "a row leads with the sha:\n{out}");
    assert!(out.contains("Stop counting git's own lock files"), "then the subject:\n{out}");
    // The author sits in its own right-aligned column, the checked-out branch is not a ref.
    let rows: Vec<&str> = out.lines().filter(|l| l.contains("  Test ")).collect();
    assert_eq!(rows.len(), 4, "every row carries the author:\n{out}");
    let ends: std::collections::HashSet<usize> =
        rows.iter().map(|l| l[..l.find("  Test ").unwrap()].chars().count()).collect();
    assert_eq!(ends.len(), 1, "the author column is one edge:\n{out}");
    assert!(!out.contains("· main"), "the checked-out branch is never a ref:\n{out}");
    let rows: Vec<&str> = out.lines().filter(|l| l.contains("▎")).collect();
    assert_eq!(rows.len(), 3, "the run carries a bar:\n{out}");
    assert!(
        !out.lines().any(|l| l.contains("▎") && l.contains(short(&shas[0]))),
        "the root is outside the run"
    );
    let footer = footer_line(&out);
    assert!(footer.contains("enter open 3"), "the footer counts the run: {footer}");
    assert!(footer.contains("v select") && footer.contains("esc clear"), "{footer}");
    // The footer stays bright, the view behind recedes.
    let plain = {
        let mut a = app_on(&r);
        a.focus = Focus::Files;
        render_buffer(&a)
    };
    let x = (0..plain.area.width)
        .find(|&x| {
            plain
                .cell((x, 0))
                .is_some_and(|c| c.symbol().chars().all(char::is_alphanumeric) && c.symbol() != " ")
        })
        .expect("a lettered cell in the tab bar");
    assert_ne!(
        plain.cell((x, 0)).unwrap().fg,
        buf.cell((x, 0)).unwrap().fg,
        "the header is scrimmed"
    );

    // Without an anchor the hint is a plain `open` and `esc` cancels.
    app.commit_picker_escape();
    let footer = footer_line(&render(&app));
    assert!(footer.contains("enter open") && !footer.contains("open 3"), "{footer}");
    assert!(footer.contains("esc cancel"), "{footer}");
    // A click on a row moves the highlight, a click on the highlight picks.
    let row_y = (0..40u16)
        .find(|&y| {
            (0..140u16).any(|x| {
                buf.cell((x, y)).is_some_and(|c| {
                    c.symbol() == short(&shas[1]).chars().next().unwrap().to_string()
                })
            }) && dump(&buf).lines().nth(y as usize).is_some_and(|l| l.contains(short(&shas[1])))
        })
        .expect("the row for `one`");
    let col = dump(&buf).lines().nth(row_y as usize).unwrap().find(short(&shas[1])).unwrap() as u16;
    let hit = ui::hit_commit_picker_row(AREA, &app, col, row_y);
    assert_eq!(hit, Some(2), "the row under the pointer");
}

#[test]
fn the_commits_header_names_the_pick_and_its_verdict() {
    let (r, mut app, shas) = commits_app();
    app.open_commit_picker();
    app.commit_picker_pick().unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(
        line0
            .contains(&format!("[commits] {} Stop counting git's own lock files", short(&shas[3]))),
        "a run of one paints sha and subject: {line0}"
    );
    assert!(line0.contains("1 changed"), "{line0}");
    // The pick name is clickable and opens the picker.
    let cols: Vec<u16> = (0..AREA.width)
        .filter(|&c| ui::hit_header(AREA, &app, app.keymap(), c, 0) == Some(HeaderHit::Pick))
        .collect();
    assert!(!cols.is_empty(), "the pick name is a header hit");
    let footer = footer_line(&render(&app));
    assert!(
        !footer.contains("G commits"),
        "row 1 never carries the picker key while picking works"
    );
    app.keys_expanded = true;
    let expanded = render(&app);
    assert!(expanded.contains("u/b/t/g scope"), "the go band names four scopes:\n{expanded}");
    assert!(expanded.contains("G commits"), "and the picker key:\n{expanded}");
    app.keys_expanded = false;

    // A run paints `a..b (N)`.
    app.open_commit_picker();
    app.commit_picker_anchor();
    app.commit_picker_move(2);
    app.commit_picker_pick().unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(
        line0.contains(&format!("[commits] {}..{} (3)", short(&shas[1]), short(&shas[3]))),
        "a run paints its ends and count: {line0}"
    );

    // Truncation keeps the sha and the marker, the subject clips.
    app.open_commit_picker();
    app.commit_picker_escape(); // drop the restored anchor: a run of one again
    app.commit_picker_pick().unwrap();
    let narrow = render_at(&app, 84).lines().next().unwrap().to_string();
    assert!(narrow.contains(short(&shas[3])), "the sha survives: {narrow}");
    assert!(narrow.contains('…'), "the subject clips: {narrow}");

    // Off branch: the marker follows the pick.
    r.git(&["reset", "-q", "--hard", &shas[2]]);
    r.write("three.rs", "again\n");
    r.commit_all("three again");
    common::land_world(&mut app);
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(line0.contains("· off branch"), "{line0}");
    let narrow = render_at(&app, 84).lines().next().unwrap().to_string();
    assert!(narrow.contains("· off branch"), "the marker survives truncation: {narrow}");
    // And the picker shows the pick as a row above the list.
    app.open_commit_picker();
    let out = render(&app);
    let pick_line = out.lines().find(|l| l.contains("· off branch")).expect("the pick row");
    assert!(pick_line.contains(short(&shas[3])), "{pick_line}");
    app.close_commit_picker();

    // Gone: both panes read the message, row 1 leads with the picker key.
    r.git(&["reflog", "expire", "--expire=now", "--all"]);
    r.git(&["gc", "-q", "--prune=now"]);
    common::land_world(&mut app);
    let out = render(&app);
    let line0 = out.lines().next().unwrap().to_string();
    assert!(line0.contains("· gone"), "{line0}");
    assert_eq!(
        out.matches(&format!("commit {} is gone", short(&shas[3]))).count(),
        2,
        "both panes:\n{out}"
    );
    let footer = footer_line(&out);
    assert!(footer.trim_start().starts_with("G commits"), "{footer}");
    assert!(footer.contains("u/b/t scope"), "the other three scopes: {footer}");
}

#[test]
fn a_gone_run_paints_no_count_and_a_verdict_paints_only_in_commits() {
    let (r, mut app, shas) = commits_app();
    app.open_commit_picker();
    app.commit_picker_anchor();
    app.commit_picker_move(1);
    app.commit_picker_pick().unwrap();
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(line0.contains("(2)"), "{line0}");
    // Rewrite the tip: the run is off branch, and the header says so in `commits` only.
    r.git(&["reset", "-q", "--hard", &shas[1]]);
    r.write("three.rs", "rewritten\n");
    r.commit_all("three again");
    common::land_world(&mut app);
    assert!(render(&app).lines().next().unwrap().contains("· off branch"));
    app.set_scope(Scope::Uncommitted).unwrap();
    app.open_commit_picker();
    let out = render(&app);
    let pick_line = out.lines().find(|l| l.contains("..")).expect("the pick row");
    assert!(pick_line.contains("(2)") && !pick_line.contains("off branch"), "{pick_line}");
    app.close_commit_picker();
    // Pruned: no count beside `· gone`.
    app.set_scope(Scope::Commits).unwrap();
    r.git(&["reflog", "expire", "--expire=now", "--all"]);
    r.git(&["gc", "-q", "--prune=now"]);
    common::land_world(&mut app);
    let line0 = render(&app).lines().next().unwrap().to_string();
    assert!(line0.contains("· gone") && !line0.contains("(0)"), "{line0}");
}

#[test]
fn a_wide_glyph_author_keeps_the_age_column() {
    let (r, mut app, _) = commits_app();
    r.write("four.rs", "4\n");
    r.git(&["add", "-A"]);
    r.git(&["commit", "-q", "-m", "four", "--author=田中太郎 <t@example.com>"]);
    app.open_commit_picker();
    let out = render(&app);
    let ages: Vec<usize> = out
        .lines()
        .skip(1)
        .filter(|l| l.contains("  ") && (l.contains("Test") || l.contains("田")))
        // The test backend dumps one char per cell, so a char index is a column.
        .map(|l| {
            let cells: Vec<char> = l.trim_end_matches([' ', '│']).chars().collect();
            cells.iter().rposition(|c| *c == ' ').unwrap()
        })
        .collect();
    assert!(out.contains("田"), "{out}");
    assert!(ages.len() >= 2 && ages.iter().all(|&a| a == ages[0]), "ages align:\n{out}");
}

#[test]
fn the_picker_trail_counts_comments_and_a_tall_list_says_more() {
    let (r, mut app, shas) = commits_app();
    // A comment on `two`.
    app.open_commit_picker();
    app.commit_picker_move(1);
    app.commit_picker_pick().unwrap();
    app.select_file(0).unwrap();
    app.focus = Focus::Diff;
    app.diff_cursor = app.diff.rows.iter().position(|r| r.marker() == '+').unwrap();
    app.start_comment();
    app.input_push('x');
    app.submit_comment();
    app.open_commit_picker();
    let out = render(&app);
    let two = out.lines().skip(1).find(|l| l.contains(&shas[2][..7])).unwrap();
    assert!(two.contains("✎ 1"), "the commented commit counts its comments: {two}");
    let three = out.lines().skip(1).find(|l| l.contains(&shas[3][..7])).unwrap();
    assert!(!three.contains('✎'), "an uncommented one does not: {three}");
    app.close_commit_picker();

    // Forty more commits: a 12-row terminal clips the list and says so.
    for i in 0..40 {
        r.write("n.rs", &format!("{i}\n"));
        r.commit_all(&format!("n{i}"));
    }
    app.open_commit_picker();
    let buf = render_size(&app, 140, 12);
    let out = dump(&buf);
    let more = out.lines().find(|l| l.contains("… ")).expect("the clip line");
    assert!(more.contains("more"), "{more}");
    assert!(!out.contains(&shas[0][..7]), "the root is below the clip");
    // The clip line is not a row: a click on it is inert.
    let y = out.lines().position(|l| l.contains("… ")).unwrap() as u16;
    let x = more.find('…').unwrap() as u16;
    assert_eq!(ui::hit_commit_picker_row(Rect::new(0, 0, 140, 12), &app, x, y), None);
}

#[test]
fn an_empty_universe_names_itself() {
    let r = Repo::init();
    let mut app = app_on(&r);
    app.open_commit_picker();
    assert_eq!(app.mode, Mode::CommitPick);
    let out = render(&app);
    assert!(out.contains("no commits yet"), "{out}");
    let footer = footer_line(&out);
    assert!(
        footer.contains("esc cancel") && !footer.contains("enter"),
        "only the exit offers: {footer}"
    );
    app.commit_picker_pick().unwrap();
    assert_eq!(app.mode, Mode::CommitPick, "enter does nothing");
    app.close_commit_picker();

    // On the base branch itself the range is empty, so the universe is the last 50.
    r.write("a.rs", "a\n");
    r.commit_all("init");
    r.set_origin_default("main", "main");
    let mut app = app_on(&r);
    app.open_commit_picker();
    let out = render(&app);
    assert!(out.contains("commits · last 50"), "{out}");
    assert!(out.contains("init"), "{out}");
}

#[test]
fn a_row_shows_one_ref_by_what_matters_most() {
    let (r, mut app, shas) = commits_app();
    r.set_origin_default("main", &shas[1]);
    r.git(&["update-ref", "refs/remotes/origin/feature", &shas[3]]);
    r.git(&["branch", "spike", &shas[3]]);
    r.git(&["tag", "v1", &shas[3]]);
    r.git(&["branch", "other", &shas[2]]);
    app.open_commit_picker();
    let out = render(&app);
    let top = out.lines().skip(1).find(|l| l.contains(&shas[3][..7])).unwrap();
    assert!(top.contains("origin/feature"), "a remote tip outranks a tag and a branch: {top}");
    assert!(!top.contains("spike") && !top.contains("v1"), "one ref only: {top}");
    let two = out.lines().skip(1).find(|l| l.contains(&shas[2][..7])).unwrap();
    assert!(two.contains("other"), "a lone local branch shows: {two}");
    app.close_commit_picker();
    r.git(&["branch", "feat/two", &shas[2]]);
    r.git(&["tag", "v0", &shas[2]]);
    app.open_commit_picker();
    let out = render(&app);
    let two = out.lines().skip(1).find(|l| l.contains(&shas[2][..7])).unwrap();
    assert!(two.contains("tag: v0") && !two.contains("feat/two"), "a slash is no remote: {two}");
    app.close_commit_picker();

    // The open PR's head outranks every ref.
    app.pr = herdr_reviewr::forge::PrView::Pr(Box::new(herdr_reviewr::forge::PrSnapshot {
        head_oid: shas[3].clone(),
        ..common::pr_snapshot()
    }));
    app.open_commit_picker();
    let out = render(&app);
    let top = out.lines().skip(1).find(|l| l.contains(&shas[3][..7])).unwrap();
    assert!(top.contains("  pr ") && !top.contains("origin/feature"), "{top}");
}

/// The default-right navigator's first inner column on the 140-wide test frame.
const FILES_X0: u16 = 140 - 140 * 32 / 100 + 1;
/// Its last inner column: the frame edge less the right border.
const FILES_X1: u16 = 140 - 2;

/// The files-pane row holding `token`: its y and its untrimmed inner text.
fn files_row_at(buf: &Buffer, token: &str) -> (u16, String) {
    let row = |y: u16| -> String {
        (FILES_X0..=FILES_X1).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect()
    };
    let y = (0..buf.area.height)
        .find(|&y| row(y).contains(token))
        .unwrap_or_else(|| panic!("no files-pane row holds {token:?}"));
    (y, row(y))
}

/// The files-pane row holding `token`, trailing padding trimmed.
fn files_row(app: &App, token: &str) -> String {
    files_row_at(&render_buffer(app), token).1.trim_end().to_string()
}

/// Whether the row holding `token` ends in a dot in the `M` marker's color.
fn dot_at_edge(app: &App, token: &str) -> bool {
    let buf = render_buffer(app);
    let (y, _) = files_row_at(&buf, token);
    let cell = buf.cell((FILES_X1, y)).unwrap();
    cell.symbol() == "•" && cell.style().fg == Some(m_marker_fg(&buf))
}

/// The color of the `M` marker on the fixtures' edited `zz.rs` row.
fn m_marker_fg(buf: &Buffer) -> ratatui::style::Color {
    let (y, text) = files_row_at(buf, "M zz.rs");
    let x = FILES_X0 + text.find("M zz.rs").unwrap() as u16;
    buf.cell((x, y)).unwrap().style().fg.expect("the marker is colored")
}

/// `src/` and `docs/` committed, `src/ui.rs` and `zz.rs` edited, `zz.rs` for the color.
fn dotted_repo() -> Repo {
    let r = Repo::init();
    r.write("src/app.rs", "x\n");
    r.write("src/ui.rs", "y\n");
    r.write("docs/a.md", "a\n");
    r.write("docs/b.md", "b\n");
    r.write("zz.rs", "z\n");
    r.commit_all("init");
    r.write("src/ui.rs", "y2\n");
    r.write("zz.rs", "z2\n");
    r
}

#[test]
fn a_collapsed_all_files_folder_with_a_change_wears_a_dot() {
    let r = dotted_repo();
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert!(dot_at_edge(&app, "src/"), "src/ holds the edit: {:?}", files_row(&app, "src/"));
    assert!(!files_row(&app, "docs/").contains('•'), "docs/ holds no change");
    assert!(files_row(&app, "src/").starts_with("▸ src/"), "the chevron is the first column");

    // Expanded, the children carry their own marker and the folder drops the dot.
    app.focus = Focus::Files;
    app.file_cursor = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    app.expand_dir();
    assert!(!files_row(&app, "src/").contains('•'), "an expanded folder wears no dot");
    assert!(files_row(&app, "ui.rs").starts_with("  M ui.rs"), "the child carries the marker");
}

#[test]
fn a_kept_change_under_an_ignored_folder_wears_the_same_dot() {
    // The folder name dims, the dot keeps its color.
    let r = dotted_repo();
    r.write(".gitignore", "vendor/\n");
    r.write("vendor/lib.rs", "v\n");
    r.write("vendor/other.rs", "o\n");
    r.git(&["add", "-f", "vendor/lib.rs", "vendor/other.rs", ".gitignore"]);
    r.commit_all("vendor");
    r.write("vendor/lib.rs", "v2\n");
    r.write("zz.rs", "z3\n");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert!(dot_at_edge(&app, "vendor/"), "{:?}", files_row(&app, "vendor/"));
}

#[test]
fn a_folder_whose_only_change_has_no_row_wears_no_dot() {
    // A staged deletion has no row, so no dot; a plain `rm` keeps its `D` row and the dot.
    let r = dotted_repo();
    r.write("gone/a.rs", "a\n");
    r.write("gone/b.rs", "b\n");
    r.write("rmd/a.rs", "a\n");
    r.write("rmd/b.rs", "b\n");
    r.commit_all("more");
    r.git(&["rm", "-q", "gone/a.rs"]);
    std::fs::remove_file(r.path_buf().join("rmd/a.rs")).unwrap();
    r.write("zz.rs", "z3\n");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert!(!files_row(&app, "gone/").contains('•'), "{:?}", files_row(&app, "gone/"));
    assert!(dot_at_edge(&app, "rmd/"), "{:?}", files_row(&app, "rmd/"));
}

#[test]
fn a_collapsed_changes_folder_wears_no_dot_and_reserves_nothing() {
    // `Changes` paints no dot.
    let r = dotted_repo();
    r.write("src/app.rs", "x2\n"); // two changed files keep `src/` a directory row
    let mut app = app_on(&r);
    app.focus = Focus::Files;
    app.file_cursor = app.file_rows.iter().position(|r| r.dir_path() == Some("src")).unwrap();
    app.collapse_dir();
    assert!(!files_row(&app, "src/").contains('•'), "no dot on Changes");
    let wide = "n".repeat((FILES_X1 - FILES_X0) as usize - 2); // `▸ ` + name + `/` fills the row
    r.write(&format!("{wide}/a.rs"), "a\n");
    r.write(&format!("{wide}/b.rs"), "b\n");
    app.reload().unwrap();
    let row = files_row(&app, &wide[..20]);
    assert_eq!(row, format!("▾ {wide}/"), "the exact-fit name is whole on Changes");
    enter_tab(&mut app, Tab::AllFiles);
    assert!(dot_at_edge(&app, "…"), "{:?}", files_row(&app, "…"));
    assert_eq!(
        files_row(&app, "…"),
        format!("▸ …{}/ •", &wide[3..]),
        "the reserve elides two columns"
    );
}

#[test]
fn the_folder_dot_follows_the_scope() {
    let r = Repo::init();
    r.write("src/a.rs", "x\n");
    r.write("src/b.rs", "y\n");
    r.write("zz.rs", "z\n");
    r.commit_all("base");
    r.git(&["checkout", "-q", "-b", "feature"]);
    r.write("src/a.rs", "x2\n");
    r.write("zz.rs", "z2\n");
    r.commit_all("feature work"); // committed on the branch, worktree clean
    let mut app = App::new(r.path_buf(), Scope::Uncommitted, Some("main".to_string()));
    app.reload().unwrap();
    enter_tab(&mut app, Tab::AllFiles);
    assert!(!files_row(&app, "src/").contains('•'), "nothing uncommitted under src/");

    app.set_scope(Scope::Branch).unwrap();
    common::land_world(&mut app);
    assert!(dot_at_edge(&app, "src/"), "the branch scope changed src/a.rs");

    app.set_scope(Scope::Uncommitted).unwrap();
    common::land_world(&mut app);
    assert!(!files_row(&app, "src/").contains('•'), "back to uncommitted, the dot clears");
}

#[test]
fn a_long_folder_name_leaves_room_for_the_dot() {
    let r = Repo::init();
    let dir = "a_directory_name_far_wider_than_the_files_pane_can_ever_hold_at_this_width";
    r.write(&format!("{dir}/one.rs"), "1\n");
    r.write(&format!("{dir}/two.rs"), "2\n");
    r.write("zz.rs", "z\n");
    r.commit_all("init");
    r.write(&format!("{dir}/one.rs"), "1b\n");
    r.write("zz.rs", "z2\n");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    assert!(dot_at_edge(&app, "…"), "{:?}", files_row(&app, "…"));
    let row = files_row(&app, "…");
    assert!(row.starts_with("▸ …") && row.contains("this_width/ •"), "head-elided: {row:?}");
    let collapsed_name = row.trim_end_matches(" •").to_string();

    app.focus = Focus::Files;
    app.file_cursor = app.file_rows.iter().position(|r| r.dir_path() == Some(dir)).unwrap();
    app.expand_dir();
    let row = files_row(&app, "…");
    assert_eq!(row.replacen('▾', "▸", 1), collapsed_name, "the name reads the same expanded");
}

#[test]
fn rendered_cards_sit_under_the_last_row_of_their_block() {
    // A paragraph wrapping to several rendered rows, its source line 4 (`beta`) deleted.
    let r = Repo::init();
    let long = "gamma ".repeat(30) + "omega";
    r.write("doc.md", &format!("# Head\n\nalpha\nbeta\n{long}\n\n- item\n"));
    r.commit_all("init");
    r.write("doc.md", &format!("# Head\n\nalpha\n{long}\n\n- item\n"));
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;

    // From source: one comment per side.
    app.toggle_rendered();
    for (marker, text) in [(' ', "on alpha"), ('-', "on the removal")] {
        app.diff_cursor = app
            .visible
            .iter()
            .position(|row| row.marker() == marker && row.text() == marker_text(marker))
            .unwrap();
        app.start_comment();
        for ch in text.chars() {
            app.input_push(ch);
        }
        app.submit_comment();
    }
    assert_eq!(app.store.len(), 2);

    app.toggle_rendered();
    assert!(app.rendered_active());
    let out = render(&app);
    let lines: Vec<&str> = out.lines().collect();
    let y = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
    let (first, last, item) = (y("alpha gamma"), y("omega"), y("item"));
    assert!(first < last, "the paragraph wraps to several rows:\n{out}");
    for text in ["on alpha", "on the removal"] {
        let card = y(text);
        assert!(last < card && card < item, "{text:?} sits under the block's last row:\n{out}");
    }

    // A draft on the paragraph's first row opens where its card will sit.
    app.store.take(1);
    app.store.take(0);
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("alpha")).unwrap();
    app.start_comment();
    let out = render(&app);
    let lines: Vec<&str> = out.lines().collect();
    let y = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
    assert!(y("omega") < y("comment ·"), "the composer splices under the block:\n{out}");
}

/// The fixture row's text a marker picks out.
fn marker_text(marker: char) -> &'static str {
    if marker == '-' { "beta" } else { "alpha" }
}

#[test]
fn a_hovered_rendered_row_shows_the_plus_button_and_a_commented_block_its_accent() {
    let r = Repo::init();
    r.write("doc.md", "# Head\n\nbody\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    let area = Rect::new(0, 0, 140, 40);
    let inner = ui::read_inner_rect(area, &app);

    app.hover = Some((inner.x + 8, inner.y));
    let buf = render_buffer(&app);
    assert_eq!(buf.cell((inner.x + 1, inner.y)).unwrap().symbol(), "[");
    assert_eq!(buf.cell((inner.x + 2, inner.y)).unwrap().symbol(), "+");
    assert_eq!(buf.cell((inner.x + 3, inner.y)).unwrap().symbol(), "]");

    // A comment on the heading paints its number in the comment accent.
    app.hover = None;
    app.diff_cursor = 0;
    app.start_comment();
    app.input_push('x');
    app.submit_comment();
    let buf = render_buffer(&app);
    let num = buf.cell((inner.x + 3, inner.y)).unwrap();
    assert_eq!(num.symbol(), "1");
    // The cursor sits on it, so the comment color resolves on the cursor fill.
    let comment = app.palette().ink(Ink::Comment, Fill::Cursor);
    assert_eq!(num.fg, comment, "the commented block's number wears your comment color");
}

#[test]
fn rendered_change_marks_paint_bars_and_marker_rows() {
    let r = Repo::init();
    r.write(
        "doc.md",
        "# Head\n\nsame para\n\nold words\n\nkeep one\n\ngone para\n\nkeep\n\n<!-- x -->\n\n- item\n",
    );
    r.commit_all("init");
    r.write(
        "doc.md",
        "# Head\n\nsame para\n\nnew words\n\nkeep one\n\nkeep\n\n<!-- y -->\n\n- item\n- added\n",
    );
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    app.diff_cursor = 0;
    assert!(app.rendered_active());
    let area = Rect::new(0, 0, 140, 40);
    let inner = ui::read_inner_rect(area, &app);
    let buf = render_buffer(&app);
    let pal = *app.palette();
    // The bar cell and text of the painted read-pane row showing `needle`.
    let row_of = |needle: &str| {
        (inner.y..inner.y + inner.height)
            .find(|&y| {
                let line: String = (inner.x..inner.x + inner.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect();
                line.contains(needle)
            })
            .unwrap_or_else(|| panic!("{needle} painted:\n{}", dump(&buf)))
    };
    let bar = |needle: &str| {
        let cell = buf.cell((inner.x, row_of(needle))).unwrap();
        (cell.symbol().to_string(), cell.fg)
    };
    let ink = |ink| pal.mark(ink, Fill::Base);
    let bar_cell = |b: Bar| {
        let i = if b == Bar::Added { Ink::Added } else { Ink::Modified };
        ("▌".to_string(), ink(i))
    };
    let marker_cell = |k: MarkerKind| {
        let i = if k == MarkerKind::Removed { Ink::Removed } else { Ink::Modified };
        ("▌".to_string(), ink(i))
    };
    assert_eq!(bar("new words"), bar_cell(Bar::Modified), "a modified block is amber");
    assert_eq!(bar("added"), bar_cell(Bar::Added), "a block that only gained is green");
    assert_eq!(bar("same para").0, " ", "an unchanged block wears no bar");
    assert_eq!(bar("− 1 line removed"), marker_cell(MarkerKind::Removed));
    assert_eq!(
        bar("⚠ 1 changed line doesn't render · m to see"),
        marker_cell(MarkerKind::Unrendered)
    );
    // The removed marker sits where the block was, between its neighbours.
    let (words, removed, keep) = (row_of("keep one"), row_of("line removed"), row_of("9 keep"));
    assert!(words < removed && removed < keep, "{}", dump(&buf));

    // A narrow pane cuts a marker to its width with `…`, never past the pane's edge.
    let narrow = render_size(&app, 50, 40);
    let marker = dump(&narrow).lines().find(|l| l.contains("⚠")).map(str::to_string);
    let marker = marker.unwrap_or_else(|| panic!("the marker paints:\n{}", dump(&narrow)));
    assert!(marker.contains('…'), "{marker:?}");
}

#[test]
fn find_lights_its_matches_on_rendered_rows() {
    let r = Repo::init();
    r.write("doc.md", "# Head\n\nsome **needle** here\n");
    r.commit_all("init");
    let mut app = app_on_rendered(&r);
    enter_tab(&mut app, Tab::AllFiles);
    app.focus = Focus::Diff;
    assert!(app.rendered_active());
    app.open_find();
    for ch in "needle".chars() {
        app.input_push(ch);
    }
    let buf = render_buffer(&app);
    let pal = *app.palette();
    let area = Rect::new(0, 0, 140, 40);
    let inner = ui::read_inner_rect(area, &app);
    let lit: String = (inner.y..inner.y + inner.height)
        .flat_map(|y| (inner.x..inner.x + inner.width).map(move |x| (x, y)))
        .filter_map(|(x, y)| {
            buf.cell((x, y))
                .filter(|c| c.bg == pal.fill(Fill::Highlight))
                .map(|c| c.symbol().to_string())
        })
        .collect();
    assert_eq!(lit, "needle", "only the match lights:\n{}", dump(&buf));
}

#[test]
fn a_collapsed_summary_paints_the_changes_its_body_hides() {
    let r = Repo::init();
    let doc = |body: &str| {
        format!("Intro\n\n<details>\n<summary>More</summary>\n\n{body}\n\n</details>\n")
    };
    r.write("doc.md", &doc("body one"));
    r.commit_all("init");
    r.write("doc.md", &doc("body two"));
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    app.toggle_details("More#0");
    let out = render(&app);
    assert!(out.contains("▸ More  · 2 changed lines"), "the summary names what it hides:\n{out}");
}

#[test]
fn the_quit_question_owns_the_footer_and_the_blocked_screen() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    app.start_comment();
    app.input_push('n');
    app.submit_comment();
    app.request_quit();
    let row = footer_line(&render_at(&app, 100));
    assert!(
        row.contains("Q quit (1 pending)") && row.contains("esc cancel"),
        "the footer is the question:\n{row}"
    );
    assert!(row.contains("s send 1") && row.contains("y copy"), "{row}");
    assert!(!row.trim_end().ends_with('?'), "the question owns the bar:\n{row}");

    // A pane-width row keeps the way out, even with a status left over from the comment.
    app.status = "comment added".to_string();
    let narrow = footer_line(&render_at(&app, 40));
    assert!(narrow.contains("Q quit") && narrow.contains("esc cancel"), "{narrow}");
    assert!(!narrow.contains("comment added"), "the question owns the row:\n{narrow}");

    app.set_config_error("config: invalid value for `theme`".to_string());
    let out = render(&app);
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "1 unsent comment comes back once the config is fixed. Press Q to quit and lose it."
        ),
        "{out}"
    );
}

#[test]
fn the_pr_tab_names_the_forge_and_words_checks_one_way() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Check, CheckStatus, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.pr_forge = herdr_reviewr::git::Forge::GitLab;
    assert!(render(&app).contains("3 MR"), "GitLab's tab says MR");

    app.set_tab(Tab::Pr).unwrap();
    for (status, rollup) in [
        (CheckStatus::Success, "✓ 2 checks passed"),
        (CheckStatus::Failure, "✗ 2 checks failing"),
        (CheckStatus::Running, "● checks running"),
        // A skipped check never reads as passed.
        (CheckStatus::Skipped, "⊘ checks skipped"),
    ] {
        let check = |name: &str| Check { name: name.into(), status };
        app.pr = PrView::Pr(Box::new(PrSnapshot {
            checks: vec![check("ci"), check("lint")],
            ..common::pr_snapshot()
        }));
        let out = render_at(&app, 140);
        assert!(footer_line(&out).contains(rollup), "the footer says {rollup}:\n{out}");
        assert_eq!(
            out.matches(rollup).count(),
            2,
            "the footer and the navigator say {rollup}:\n{out}"
        );
    }
}

#[test]
fn a_trimmed_modal_row_always_shows_its_ellipsis() {
    let mut app = edited_app();
    on_changed_line(&mut app);
    for n in 1..=100 {
        app.start_comment();
        app.input_push('n');
        app.submit_comment();
        if ![1, 9, 10, 99, 100].contains(&n) {
            continue;
        }
        app.request_quit();
        for w in 30..=80u16 {
            let row = footer_line(&render_at(&app, w));
            let row = row.trim_end();
            assert!(row.chars().count() <= usize::from(w), "{n} at {w} overflows:\n{row}");
            if !row.contains("y copy") {
                assert!(row.ends_with('…'), "{n} at {w} trims without its `…`:\n{row}");
            }
        }
        app.confirming_quit = false;
    }
}

/// Chrome paints by meaning: a popup's border is the accent, the composer's is your comment
/// color, the footer's status message is plain text, in every theme.
#[test]
fn chrome_paints_roles_by_meaning() {
    let any_fg = |buf: &Buffer, color| {
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .any(|(x, y)| buf.cell((x, y)).is_some_and(|c| c.fg == color))
    };
    for theme in ["catppuccin", "ayu"] {
        let mut app = edited_app();
        app.set_cli_theme(Some(theme.to_string()));
        let p = *app.palette();
        on_changed_line(&mut app);

        // The composer's border wears your comment color.
        app.start_comment();
        assert!(any_fg(&render_buffer(&app), p.mark(Ink::Comment, Fill::Base)), "{theme}");
        app.input_push('x');
        app.submit_comment();

        // The comment list is a popup: its border is the accent.
        app.open_list();
        let buf = render_buffer(&app);
        // The border cell just left of the popup's title: the footer's keys are the accent too.
        let (x, y) = token_at(&buf, "Comments", 0);
        let border = buf.cell((x - 2, y)).unwrap();
        assert_eq!(border.fg, p.mark(Ink::Accent, Fill::Base), "{theme}: {:?}", border.symbol());
        app.close_list();

        // The status message reports in plain text on the footer bar.
        app.status = "comment added".to_string();
        let buf = render_buffer(&app);
        let footer_y = buf.area.height - 1;
        let row: String =
            (0..buf.area.width).map(|x| buf.cell((x, footer_y)).unwrap().symbol()).collect();
        let byte = row.find("comment added").expect("the status shows");
        let at = u16::try_from(row[..byte].chars().count()).unwrap();
        let status = buf.cell((at, footer_y)).map(|c| c.fg);
        assert_eq!(status, Some(p.ink(Ink::Text, Fill::Bar)), "{theme}: the status is plain text");
    }
}

/// Fills stack in one order: a find match wins over the diff row under it, and a line range
/// picked for a comment paints the selection fill, never the unfocused cursor's.
#[test]
fn fills_stack_in_one_order() {
    let mut app = edited_app();
    let p = *app.palette();
    on_changed_line(&mut app);
    let added_row = app.diff_cursor;
    let bgs = |app: &App| {
        let buf = render_buffer(app);
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| buf.cell((x, y)).map(|c| c.bg))
            .collect::<Vec<_>>()
    };

    // A match on an added row paints the highlight fill over the row's own.
    app.open_find();
    for ch in "BETA".chars() {
        app.input_push(ch);
    }
    assert!(bgs(&app).contains(&p.fill(Fill::Highlight)), "the match lights over the added row");
    app.close_find();

    // A picked line range paints the selection fill, distinct from the unfocused cursor.
    // Anchor at the top, cursor on the added row: the rows between are the range.
    app.diff_cursor = 0;
    app.toggle_select();
    app.diff_cursor = added_row;
    let painted = bgs(&app);
    assert!(painted.contains(&p.fill(Fill::Selection)), "the range wears the selection fill");
}

/// The buffer cell where `needle` first paints.
fn cell_of<'a>(buf: &'a Buffer, needle: &str) -> &'a ratatui::buffer::Cell {
    buf.cell(token_at(buf, needle, 0)).unwrap()
}

/// Inline code in rendered markdown sits on the code chip; the cursor row's fill stacks above
/// it.
#[test]
fn inline_code_wears_its_chip_under_everything_but_the_cursor() {
    let r = Repo::init();
    r.write("doc.md", "# Head\n\nalpha\n");
    r.commit_all("init");
    r.write("doc.md", "# Head\n\nrun `cargo test` now\n\nomega\n");
    let mut app = app_on_rendered(&r);
    app.focus = Focus::Diff;
    let p = *app.palette();
    let row_of = |app: &App, needle: &str| {
        app.visible.iter().position(|row| row.text().contains(needle)).unwrap()
    };

    app.diff_cursor = row_of(&app, "omega");
    assert_eq!(cell_of(&render_buffer(&app), "cargo").bg, p.fill(Fill::Code));
    app.diff_cursor = row_of(&app, "cargo");
    assert_eq!(cell_of(&render_buffer(&app), "cargo").bg, p.fill(Fill::Cursor));
}

/// Running and queued checks are yellow, like herdr's working dot and GitHub's pending checks;
/// a draft PR's chip is muted.
#[test]
fn running_checks_are_yellow_and_a_draft_is_muted() {
    use herdr_reviewr::app::Tab;
    use herdr_reviewr::forge::{Check, CheckStatus, PrSnapshot, PrView};
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    let p = *app.palette();
    app.set_tab(Tab::Pr).unwrap();
    let check = |name: &str, status| Check { name: name.into(), status };
    app.pr = PrView::Pr(Box::new(PrSnapshot {
        is_draft: true,
        checks: vec![check("ci", CheckStatus::Running), check("lint", CheckStatus::Pending)],
        ..common::pr_snapshot()
    }));
    let buf = render_buffer(&app);
    let yellow = [Fill::Base, Fill::Bar, Fill::Cursor, Fill::CursorInactive]
        .map(|on| p.mark(Ink::Warning, on));
    // Each check row's glyph; the one-line rollup is plain text for every status.
    for row in ["● ci", "○ lint"] {
        let fg = cell_of(&buf, row).fg;
        assert!(yellow.contains(&fg), "{row:?} paints {fg:?}, not yellow");
    }
    assert_eq!(cell_of(&buf, "draft").fg, p.ink(Ink::TextMuted, Fill::Bar));
}

/// A syntax token on the cursor row keeps the legibility it has on the plain background:
/// tokyo-night-day's dim comment, which its cursor fill would otherwise wash out.
#[test]
fn syntax_on_the_cursor_row_keeps_its_legibility() {
    use herdr_reviewr::roles::contrast;
    let r = Repo::init();
    r.write("a.rs", "// quiet note\nfn x() {}\n");
    r.commit_all("init");
    r.write("a.rs", "// quiet note\nfn y() {}\n");
    let mut app = app_on(&r);
    app.set_cli_theme(Some("tokyo-night-day".to_string()));
    app.reload().unwrap(); // highlight with this theme's syntax colors
    app.focus = Focus::Diff;
    let p = *app.palette();
    let row_of = |app: &App, needle: &str| {
        app.visible.iter().position(|row| row.text().contains(needle)).unwrap()
    };

    app.diff_cursor = row_of(&app, "fn y");
    let plain = cell_of(&render_buffer(&app), "quiet").fg;
    app.diff_cursor = row_of(&app, "quiet");
    let buf = render_buffer(&app);
    let lit = cell_of(&buf, "quiet");
    assert_eq!(lit.bg, p.fill(Fill::Cursor), "the comment sits on the cursor row");
    let want = contrast(plain, p.fill(Fill::Base)).min(4.5) - 0.05;
    let got = contrast(lit.fg, lit.bg);
    assert!(got >= want, "{got:.2} on the cursor row, {want:.2} on the background");
}

/// A syntax token on the cursor row keeps its hue, even one that shares a role's color: nord's
/// keyword blue is its comment color, and resolving it as that role once grayed it.
#[test]
fn syntax_on_the_cursor_row_keeps_its_hue() {
    use palette::{IntoColor, Oklch, Srgb};
    let polar = |c: ratatui::style::Color| {
        let ratatui::style::Color::Rgb(r, g, b) = c else { panic!("{c:?}") };
        let lch: Oklch<f64> = Srgb::new(r, g, b).into_format::<f64>().into_linear().into_color();
        (lch.hue.into_positive_degrees(), lch.chroma)
    };
    let theme = "nord";
    let r = Repo::init();
    r.write(
        "a.rs",
        "fn x() {}
struct Item;
",
    );
    r.commit_all("init");
    r.write(
        "a.rs",
        "fn x() {}
struct Item;
// edit
",
    );
    let mut app = app_on(&r);
    app.set_cli_theme(Some(theme.to_string()));
    app.reload().unwrap(); // highlight with this theme's syntax colors
    app.focus = Focus::Diff;
    let row_of = |app: &App, needle: &str| {
        app.visible.iter().position(|row| row.text().contains(needle)).unwrap()
    };
    app.diff_cursor = row_of(&app, "edit");
    let plain = cell_of(&render_buffer(&app), "struct").fg;
    app.diff_cursor = row_of(&app, "struct");
    let lit = cell_of(&render_buffer(&app), "struct").fg;
    let ((hue, chroma), (lit_hue, lit_chroma)) = (polar(plain), polar(lit));
    let turn = (hue - lit_hue).abs().min(360.0 - (hue - lit_hue).abs());
    assert!(turn < 10.0, "{theme}: hue {hue:.0}° became {lit_hue:.0}°");
    // A light cursor row can force a token to the edge of sRGB, where chroma must give;
    // a blend toward body text loses far more.
    assert!(lit_chroma >= chroma * 0.5, "{theme}: chroma {chroma:.3} fell to {lit_chroma:.3}");
}

/// The line field paints its label, the typed number, the file's line count at the right and
/// its own footer; its digits light no find match. The `?` bar lists `:`.
#[test]
fn the_line_field_paints_its_number_count_and_footer() {
    let r = Repo::init();
    r.write("base.txt", "x\n");
    r.commit_all("init");
    let text = (1..=2048).fold(String::new(), |mut text, i| {
        use std::fmt::Write as _;
        writeln!(text, "row {i}").unwrap();
        text
    });
    r.write("m.rs", &text);
    let mut app = app_on(&r);
    let keymap = Keymap::default();
    let area = Rect::new(0, 0, 140, 40);
    let key = |app: &mut App, code| handle_key(app, KeyEvent::from(code), area, &keymap).unwrap();

    key(&mut app, KeyCode::Char('?'));
    assert!(dump(&render_buffer(&app)).contains(": line"), "the go band lists `:`");
    key(&mut app, KeyCode::Char('?'));

    key(&mut app, KeyCode::Char(':'));
    for ch in "1337".chars() {
        key(&mut app, KeyCode::Char(ch));
    }
    let out = dump(&render_buffer(&app));
    assert!(out.contains("line 1337"), "the label and the number:\n{out}");
    assert!(out.contains("of 2048"), "the file's line count:\n{out}");
    assert!(footer_line(&out).contains("enter go"), "the field's footer:\n{out}");

    // The digits are a line, not a search: `row 1337` lights no match.
    let hl = app.palette().fill(Fill::Highlight);
    assert!(!render_buffer(&app).content.iter().any(|c| c.bg == hl), "no find highlight");
}

#[test]
fn four_opens_releases_and_every_tab_key_works_from_it() {
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    let area = Rect::new(0, 0, 140, 40);
    let keymap = Keymap::default();
    for (key, tab) in [
        ('4', Tab::Releases),
        ('1', Tab::Changes),
        ('4', Tab::Releases),
        ('2', Tab::AllFiles),
        ('4', Tab::Releases),
        ('3', Tab::Pr),
        ('4', Tab::Releases),
    ] {
        handle_key(&mut app, KeyEvent::from(KeyCode::Char(key)), area, &keymap).unwrap();
        assert_eq!(app.tab, tab, "`{key}`");
    }
    let header = render(&app).lines().next().unwrap().to_string();
    assert!(header.contains("4 Releases"), "the fourth tab, led by its key:\n{header}");
    for tab in [Tab::Changes, Tab::AllFiles, Tab::Pr, Tab::Releases] {
        assert!(!tab_hit_cols(&app, &keymap, tab).is_empty(), "{tab:?} is clickable");
    }
    app.keys_expanded = true;
    let expanded = render(&app);
    assert!(expanded.contains("1·2·3·4 tabs"), "the key list names all four:\n{expanded}");
}

#[test]
fn the_releases_tab_key_rebinds_like_the_others() {
    let mut app = rebound_app("tab-releases = [\"x\"]\n");
    let area = Rect::new(0, 0, 140, 40);
    let keymap = app.keymap().clone();
    handle_key(&mut app, KeyEvent::from(KeyCode::Char('4')), area, &keymap).unwrap();
    assert_eq!(app.tab, Tab::Changes, "the replaced digit is free");
    handle_key(&mut app, KeyEvent::from(KeyCode::Char('x')), area, &keymap).unwrap();
    assert_eq!(app.tab, Tab::Releases);
    let out = render(&app);
    assert!(out.contains("x Releases") && !out.contains("4 Releases"), "{out}");
}

#[test]
fn each_releases_state_without_a_list_says_what_to_do() {
    use herdr_reviewr::git::Forge;
    use herdr_reviewr::releases::ReleasesView;
    let r = Repo::init();
    r.write("x.rs", "y\n");
    r.commit_all("init");
    let mut app = app_on(&r);
    app.set_tab(Tab::Releases).unwrap();
    let pending = render(&app);
    assert!(!pending.contains("loading"), "a quick load flashes nothing:\n{pending}");
    for (view, needle) in [
        (ReleasesView::Loading, "loading…"),
        (ReleasesView::NotGitHub(Forge::GitLab), "reads GitHub only. This repository is on GitLab"),
        (ReleasesView::NotGitHub(Forge::AzureDevOps), "on Azure DevOps"),
        (ReleasesView::NoCli, "Install `gh`, then press r"),
        (ReleasesView::NotAuthed("github.com".into()), "gh auth login --hostname github.com"),
        (ReleasesView::NeedsForgeRemote, "needs a GitHub remote named origin."),
        (ReleasesView::NoDefaultBranch, "no default branch yet"),
        (ReleasesView::Error("rate limited".into()), "GitHub unavailable: rate limited"),
    ] {
        app.apply_releases(view.clone());
        let out = render(&app);
        assert!(out.contains(needle), "{view:?}:\n{out}");
    }
}

#[test]
fn each_tab_shows_its_own_binding_for_a_shared_key() {
    use herdr_reviewr::releases::ReleasesView;
    let mut app = edited_app();
    app.focus = Focus::Diff;
    app.keys_expanded = true;
    let changes = render(&app);
    assert!(changes.contains("c comment"), "{changes}");
    assert!(!changes.contains("new release"), "{changes}");
    app.set_tab(Tab::Releases).unwrap();
    app.apply_releases(ReleasesView::NoCli);
    let releases = render(&app);
    assert!(
        !releases.contains("c comment"),
        "no comment key where comments do not exist: {releases}"
    );
}
