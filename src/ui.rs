//! Rendering the Changes view: tab bar, file list, diff, comment box, list, status.
//!
//! See `specs/tui.md`. The layout is a header tab bar, a body split into the diff
//! (left) and the file list (right), and a status bar. While composing, the comment
//! box is spliced inline into the diff under the selected line; the comments-list
//! overlay is drawn on top when open. Rendering reads `App` only; all state changes
//! live in `app.rs`.

use std::rc::Rc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Focus, Mode};
use crate::diff::{FileState, Row};
use crate::file_list::RowKind;
use crate::model::{ChangeKind, Comment};

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let rows = vrows(area);
    let (diff_area, files_area) = body_split(&rows, app.list_pct);

    render_tab_bar(frame, app, rows[0]);
    render_diff_view(frame, app, diff_area);
    render_file_list(frame, app, files_area);
    render_status_bar(frame, app, rows[2]);

    if app.mode == Mode::List {
        render_comments_list(frame, app, area);
    }
}

/// The vertical bands: tab bar, body, status. The comment input is inline in the
/// diff, not a band of its own. The status band grows so its hints wrap rather than
/// truncate at narrow widths.
fn vrows(area: Rect) -> Rc<[Rect]> {
    let footer = footer_height(area.width);
    Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(footer)])
        .split(area)
}

/// Rows the status band needs for the longest hint line to wrap at `width` (1–3).
fn footer_height(width: u16) -> u16 {
    // Widest footer (counts + status + the Normal-mode hints) is ~150 columns.
    const FOOTER_COLS: u16 = 150;
    FOOTER_COLS.div_ceil(width.max(1)).clamp(1, 3)
}

/// The body split into `(diff, files)` outer rects, the file list taking `list_pct` percent.
fn body_split(rows: &[Rect], list_pct: u16) -> (Rect, Rect) {
    let body = Layout::horizontal([
        Constraint::Percentage(100 - list_pct),
        Constraint::Percentage(list_pct),
    ])
    .split(rows[1]);
    (body[0], body[1])
}

/// The whole body band (between the tab bar and status bar), for divider hit-testing.
#[must_use]
pub fn body_rect(area: Rect) -> Rect {
    vrows(area)[1]
}

/// Whether `(col, row)` lands on the draggable divider between the two panes.
#[must_use]
pub fn hit_divider(area: Rect, list_pct: u16, col: u16, row: u16) -> bool {
    let rows = vrows(area);
    let (_, files_area) = body_split(&rows, list_pct);
    let in_body = row >= rows[1].y && row < rows[1].y + rows[1].height;
    // A 3-column grab zone straddling the abutting pane borders.
    in_body && col + 1 >= files_area.x && col <= files_area.x + 1
}

/// The file-row index a click at `(col, row)` lands on, or `None` if outside the list.
/// `file_scroll` is the top visible row, so a click maps to the scrolled-to row.
#[must_use]
pub fn hit_file(
    area: Rect,
    list_pct: u16,
    col: u16,
    row: u16,
    n_files: usize,
    file_scroll: usize,
) -> Option<usize> {
    let rows = vrows(area);
    let (_, files_area) = body_split(&rows, list_pct);
    let inner = inner_rect(files_area);
    if !contains(inner, col, row) {
        return None;
    }
    let idx = (row - inner.y) as usize + file_scroll;
    (idx < n_files).then_some(idx)
}

/// The number of file rows visible in the file pane, used to clamp the file-list scroll.
#[must_use]
pub fn file_viewport_height(area: Rect, list_pct: u16) -> usize {
    let rows = vrows(area);
    let (_, files_area) = body_split(&rows, list_pct);
    inner_rect(files_area).height as usize
}

/// Whether `(col, row)` falls in the file pane, so the wheel scrolls the list it is over.
#[must_use]
pub fn in_files_pane(area: Rect, list_pct: u16, col: u16, row: u16) -> bool {
    let rows = vrows(area);
    let (_, files_area) = body_split(&rows, list_pct);
    contains(files_area, col, row)
}

/// The logical diff-row index a click at `(col, row)` lands on, or `None` if outside the
/// diff pane. `heights` (display rows per logical row) and `diff_scroll` reproduce the
/// painted window, so a click on any display line of a wrapped row maps to that row.
#[must_use]
pub fn hit_diff(
    area: Rect,
    list_pct: u16,
    col: u16,
    row: u16,
    heights: &[usize],
    diff_scroll: usize,
) -> Option<usize> {
    let rows = vrows(area);
    let (diff_area, _) = body_split(&rows, list_pct);
    let inner = inner_rect(diff_area);
    if !contains(inner, col, row) {
        return None;
    }
    let target = (row - inner.y) as usize;
    let mut acc = 0;
    for (li, h) in heights.iter().enumerate().skip(diff_scroll) {
        acc += h;
        if target < acc {
            return Some(li);
        }
    }
    None
}

/// The number of diff rows visible in the diff pane, used to clamp the scroll.
#[must_use]
pub fn diff_viewport_height(area: Rect, list_pct: u16) -> usize {
    let rows = vrows(area);
    let (diff_area, _) = body_split(&rows, list_pct);
    inner_rect(diff_area).height as usize
}

/// The display height (rows on screen) of each visible logical diff row, honoring wrap.
#[must_use]
pub fn diff_row_heights(app: &App, area: Rect) -> Vec<usize> {
    let rows = vrows(area);
    let (diff_area, _) = body_split(&rows, app.list_pct);
    let width = inner_rect(diff_area).width as usize;
    let total_lines: usize =
        app.diff.rows.iter().map(|r| if r.is_content() { 1 } else { r.hidden() }).sum();
    let gutter_w = gutter_width(total_lines);
    // A row's display height is its wrapped code lines plus any inline comment cards under
    // it (excluding a card whose comment is being edited), so scroll-clamping and hit-testing
    // match what the renderer paints.
    let cards = app.comment_cards();
    let editing = editing_comment(app);
    app.visible
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let base = row_height(r, gutter_w, width, app.wrap);
            let card: usize = cards[i]
                .iter()
                .filter(|&&ci| Some(ci) != editing)
                .filter_map(|&ci| app.store.get(ci))
                .map(|c| comment_card_lines(c, width).len())
                .sum();
            base + card
        })
        .collect()
}

/// The store index of the comment currently being edited, whose inline card is hidden in
/// favor of its edit box; `None` when not editing.
fn editing_comment(app: &App) -> Option<usize> {
    match app.mode {
        Mode::Composing { editing } => editing,
        _ => None,
    }
}

/// Rows the inline comment box occupies at the diff pane's `width`: the wrapped body height
/// (so the box grows as text wraps, not only on explicit newlines) plus the two borders.
/// Uses the same wrapped lines as [`render_composer`], so the reserved height matches.
#[must_use]
pub fn composer_height(app: &App, width: usize) -> usize {
    composer_lines(app, composer_content_width(width)).len() + 2
}

/// The text width inside the comment box: the diff pane width minus its two borders.
fn composer_content_width(width: usize) -> usize {
    width.saturating_sub(2).max(1)
}

/// The diff pane's inner content width for the full terminal `area`, so the event loop can
/// reserve the comment box without a `Frame` (mirrors [`diff_viewport_height`]).
#[must_use]
pub fn diff_inner_width(area: Rect, list_pct: u16) -> usize {
    let rows = vrows(area);
    let (diff_area, _) = body_split(&rows, list_pct);
    inner_rect(diff_area).width as usize
}

/// The comment box's display lines at `content_w`: each input line word-wrapped (via the
/// diff's [`wrap_segments`], so the box wraps exactly as it renders), the last carrying the
/// cursor block.
fn composer_lines(app: &App, content_w: usize) -> Vec<Line<'static>> {
    let mut texts: Vec<String> = Vec::new();
    for logical in app.input.split('\n') {
        texts.extend(wrap_text(logical, content_w));
    }
    let last = texts.len() - 1; // always ≥ 1: an empty input yields one empty line
    texts
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            if i == last {
                Line::from(vec![Span::raw(t), Span::styled("█", Style::default().fg(cat::PEACH))])
            } else {
                Line::from(t)
            }
        })
        .collect()
}

/// Word-wrap a plain string to `width` columns, reusing the diff's [`wrap_segments`] so the
/// break rule (last space, hard-break an over-wide word, width-aware) is identical.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let cells: Vec<Cell> = s
        .chars()
        .map(|ch| Cell {
            ch,
            w: UnicodeWidthChar::width(ch).unwrap_or(0),
            fg: cat::TEXT,
            emph: false,
        })
        .collect();
    wrap_segments(&cells, width)
        .into_iter()
        .map(|(a, b)| cells[a..b].iter().map(|c| c.ch).collect())
        .collect()
}

/// A clickable region in the header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderHit {
    Scope,
    Send,
}

/// Which header control a click at `(col, row)` lands on, if any.
#[must_use]
pub fn hit_header(area: Rect, app: &App, col: u16, row: u16) -> Option<HeaderHit> {
    if row != area.y {
        return None;
    }
    let scope_start = HEADER_PREFIX.len() as u16;
    let scope_end = scope_start + scope_chip(app).len() as u16;
    let button_start = area.width.saturating_sub(send_button(app).len() as u16);
    if (scope_start..scope_end).contains(&col) {
        Some(HeaderHit::Scope)
    } else if col >= button_start && col < area.width {
        Some(HeaderHit::Send)
    } else {
        None
    }
}

const HEADER_PREFIX: &str = " Changes  ";

fn scope_chip(app: &App) -> String {
    format!("[{}]", app.scope.label())
}

fn send_button(app: &App) -> String {
    format!("[ Send ({}) ]", app.store.len())
}

fn render_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let chip = scope_chip(app);
    let suffix = format!("  {} file(s)", app.files.len());
    let button = send_button(app);
    let used = HEADER_PREFIX.len() + chip.len() + suffix.len() + button.len();
    let pad = (area.width as usize).saturating_sub(used);

    // A quiet surface bar: the title in lavender, the clickable scope and Send controls
    // accented so they read as buttons without a loud full-width fill.
    let bar = Style::default().bg(cat::SURFACE0);
    let send_fg = if app.store.is_empty() { cat::OVERLAY0 } else { cat::GREEN };
    let line = Line::from(vec![
        Span::styled(HEADER_PREFIX, bar.fg(cat::LAVENDER).add_modifier(Modifier::BOLD)),
        Span::styled(chip, bar.fg(cat::YELLOW).add_modifier(Modifier::BOLD)),
        Span::styled(suffix, bar.fg(cat::OVERLAY0)),
        Span::styled(" ".repeat(pad), bar),
        Span::styled(button, bar.fg(send_fg).add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_file_list(frame: &mut Frame, app: &App, area: Rect) {
    let block = bordered("Files", app.focus == Focus::Files);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.file_rows.is_empty() {
        frame.render_widget(dim_paragraph("no changes"), inner);
        return;
    }

    let width = inner.width as usize;
    // Window the rows to the scrolled-to viewport; `file_scroll` keeps the cursor on screen.
    let items: Vec<ListItem> = app
        .file_rows
        .iter()
        .enumerate()
        .skip(app.file_scroll)
        .take(inner.height as usize)
        .map(|(i, row)| {
            // The selected row fills with the cursor color, dimmed when the list is unfocused.
            let fill = (i == app.file_cursor).then(|| cursor_bg(app.focus == Focus::Files));
            let indent = "  ".repeat(row.depth);
            match &row.kind {
                RowKind::Dir { expanded, .. } => {
                    let arrow = if *expanded { "▾ " } else { "▸ " };
                    let spans = vec![
                        Span::styled(
                            format!("{indent}{arrow}"),
                            Style::default().fg(cat::OVERLAY0),
                        ),
                        Span::styled(
                            format!("{}/", row.name),
                            Style::default().fg(cat::SUBTEXT0).add_modifier(Modifier::BOLD),
                        ),
                    ];
                    selectable_row(spans, width, fill)
                }
                RowKind::File { change, additions, deletions, .. } => {
                    file_row_item(&indent, *change, &row.name, *additions, *deletions, width, fill)
                }
            }
        })
        .collect();
    frame.render_widget(List::new(items), inner);
}

/// A file row: `<indent><marker> <name> <stats>` — the marker colored by kind, the basename
/// bright with its parent directories dimmed, and the `+a −d` stats right-aligned against the
/// pane edge. A name too wide for the row keeps its tail behind a leading `…/`.
fn file_row_item(
    indent: &str,
    change: ChangeKind,
    name: &str,
    additions: u32,
    deletions: u32,
    width: usize,
    fill: Option<Color>,
) -> ListItem<'static> {
    let marker = format!("{} ", change.marker());
    let stats = stats_str(additions, deletions);
    let gap = if stats.is_empty() { 0 } else { 2 };
    let fixed = indent.width() + marker.width() + stats.width() + gap;
    let shown = elide_head(name, width.saturating_sub(fixed).max(1));
    // Dim the parent directories of a collapsed-chain name; keep the basename bright.
    let (dim, base) = match shown.rfind('/') {
        Some(p) => (&shown[..=p], &shown[p + 1..]),
        None => ("", shown.as_str()),
    };

    let mut spans = vec![
        Span::styled(indent.to_string(), text_style()),
        Span::styled(marker, Style::default().fg(kind_color(change.marker()))),
    ];
    if !dim.is_empty() {
        spans.push(Span::styled(dim.to_string(), Style::default().fg(cat::OVERLAY0)));
    }
    spans.push(Span::styled(base.to_string(), text_style()));
    if !stats.is_empty() {
        let used: usize = spans.iter().map(Span::width).sum();
        let pad = width.saturating_sub(used + stats.width());
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(stats_spans(additions, deletions));
    }
    selectable_row(spans, width, fill)
}

/// The `+a −d` stats text, dropping a side that is zero (`+210`, `−4`, or empty); used to
/// measure the stats column. [`stats_spans`] paints the same text in green/red.
fn stats_str(additions: u32, deletions: u32) -> String {
    match (additions, deletions) {
        (0, 0) => String::new(),
        (a, 0) => format!("+{a}"),
        (0, d) => format!("−{d}"),
        (a, d) => format!("+{a} −{d}"),
    }
}

/// The `+a −d` stats as colored spans: additions in green, deletions in red, matching the
/// diff's add/remove hues. Same glyphs (and width) as [`stats_str`].
fn stats_spans(additions: u32, deletions: u32) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if additions > 0 {
        spans.push(Span::styled(format!("+{additions}"), Style::default().fg(cat::GREEN)));
    }
    if additions > 0 && deletions > 0 {
        spans.push(Span::raw(" "));
    }
    if deletions > 0 {
        spans.push(Span::styled(format!("−{deletions}"), Style::default().fg(cat::RED)));
    }
    spans
}

/// Shorten `name` to `max` columns by eliding its head behind a leading `…`, preferring to
/// cut at a path separator so a partial directory name never shows.
fn elide_head(name: &str, max: usize) -> String {
    if name.width() <= max {
        return name.to_string();
    }
    let budget = max.saturating_sub(1); // a column for the `…`
    let mut tail = String::new();
    let mut w = 0;
    for ch in name.chars().rev() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        tail.insert(0, ch);
        w += cw;
    }
    if let Some(slash) = tail.find('/') {
        tail = tail[slash..].to_string();
    }
    format!("…{tail}")
}

/// A saved comment as inline display lines: a quiet box titled with the comment's location
/// (in the comment-yellow accent) holding its wrapped text. Spliced read-only under the
/// commented line so a submitted comment stays visible while reviewing.
fn comment_card_lines(c: &Comment, width: usize) -> Vec<Line<'static>> {
    const INDENT: usize = 2;
    let box_w = width.saturating_sub(INDENT).max(10);
    let text_w = box_w.saturating_sub(4).max(1); // inside "│ " … " │"
    let border = Style::default().fg(cat::OVERLAY0);
    let title = Style::default().fg(cat::PEACH).add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(cat::TEXT);
    let pad = || Span::raw(" ".repeat(INDENT));

    let label = truncate_width(&format!(" comment · {} ", c.location()), box_w.saturating_sub(3));
    let fill = box_w.saturating_sub(3 + label.width());
    let mut lines = vec![Line::from(vec![
        pad(),
        Span::styled("╭─", border),
        Span::styled(label, title),
        Span::styled(format!("{}╮", "─".repeat(fill)), border),
    ])];

    for logical in c.text.split('\n') {
        for piece in wrap_text(logical, text_w) {
            let gap = " ".repeat(text_w.saturating_sub(piece.width()));
            lines.push(Line::from(vec![
                pad(),
                Span::styled("│ ", border),
                Span::styled(piece, body_style),
                Span::styled(format!("{gap} │"), border),
            ]));
        }
    }

    lines.push(Line::from(vec![
        pad(),
        Span::styled(format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))), border),
    ]));
    lines
}

/// Truncate `s` to `max` display columns, marking a cut with a trailing `…`.
fn truncate_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

fn render_diff_view(frame: &mut Frame, app: &App, area: Rect) {
    let title = match (&app.diff_path, &app.diff.previous_path) {
        (Some(new), Some(old)) => format!("{old} → {new}"),
        (Some(new), None) => new.clone(),
        (None, _) => "Diff".to_string(),
    };
    let block = bordered(&title, app.focus == Focus::Diff);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.visible.is_empty() {
        let msg = match app.diff.state {
            FileState::Binary => "binary — no line comments",
            FileState::TooLarge => "file too large to diff",
            FileState::Normal => "no diff",
        };
        frame.render_widget(dim_paragraph(msg), inner);
        return;
    }

    let height = inner.height as usize;
    if height == 0 {
        return;
    }
    let width = inner.width as usize;
    // Size the gutter to the file's largest line number, so it does not resize when a
    // fold toggles (folds hide lines but keep the numbering).
    let total_lines: usize =
        app.diff.rows.iter().map(|r| if r.is_content() { 1 } else { r.hidden() }).sum();
    let gutter_w = gutter_width(total_lines);
    let layout = RowLayout {
        gutter_w,
        width,
        h_scroll: app.h_scroll,
        wrap: app.wrap,
        focused: app.focus == Focus::Diff,
    };
    let commented = app.commented_lines();
    let cards = app.comment_cards();
    let editing = editing_comment(app);
    let (lo, hi) = app.selection_range();
    let selecting = app.focus == Focus::Diff && app.select_anchor.is_some();

    // One logical row → its 1+ wrapped display lines, then any saved-comment cards anchored
    // to it. The cursor/selection apply to the code line's display rows, not the cards. The
    // card of a comment being edited is hidden — its edit box stands in for it.
    let row_lines = |i: usize| -> Vec<Line> {
        let state = RowState {
            commented: commented.contains(&i),
            cursor: app.focus == Focus::Diff && i == app.diff_cursor,
            selected: selecting && i >= lo && i <= hi,
        };
        let mut lines = render_row(&app.visible[i], layout, state);
        for &ci in &cards[i] {
            if Some(ci) != editing
                && let Some(c) = app.store.get(ci)
            {
                lines.extend(comment_card_lines(c, width));
            }
        }
        lines
    };
    // Display lines for the logical rows in `range`, in order.
    let display = |range: std::ops::Range<usize>| -> Vec<Line> {
        range.flat_map(&row_lines).collect::<Vec<_>>()
    };

    let rows = app.visible.len();
    if !app.composing() {
        // Fill the pane from `diff_scroll`'s first display line; clamp keeps the cursor in.
        let mut out = display(app.diff_scroll..rows);
        out.truncate(height);
        frame.render_widget(Paragraph::new(out), inner);
        return;
    }

    // Composing: splice the input box under the last selected line, in display rows.
    // Cap the box at height-1 so a comment taller than the viewport can't hide its anchor.
    let box_h = composer_height(app, width).min(height.saturating_sub(1)).max(1);
    let diff_budget = height - box_h;
    let anchor = hi.clamp(app.diff_scroll, rows.saturating_sub(1));
    let above = display(app.diff_scroll..anchor + 1);
    // Keep the anchor's last display line just above the box when `above` overflows.
    let above: Vec<Line> =
        if above.len() > diff_budget { above[above.len() - diff_budget..].to_vec() } else { above };
    let remaining = diff_budget - above.len();
    let mut below = display(anchor + 1..rows);
    below.truncate(remaining);

    let slots = Layout::vertical([
        Constraint::Length(above.len() as u16),
        Constraint::Length(box_h as u16),
        Constraint::Length(below.len() as u16),
    ])
    .split(inner);
    if !above.is_empty() {
        frame.render_widget(Paragraph::new(above), slots[0]);
    }
    render_composer(frame, app, slots[1]);
    if !below.is_empty() {
        frame.render_widget(Paragraph::new(below), slots[2]);
    }
}

// --- Catppuccin Mocha palette (one source for every color the chrome and diff use,
// so the frame and the syntect-highlighted body share a single theme) ----------------

mod cat {
    use ratatui::style::Color::{self, Rgb};
    // Surfaces, dark to light.
    pub(super) const SURFACE0: Color = Rgb(0x31, 0x32, 0x44);
    pub(super) const SURFACE1: Color = Rgb(0x45, 0x47, 0x5a);
    pub(super) const SURFACE2: Color = Rgb(0x58, 0x5b, 0x70);
    pub(super) const OVERLAY0: Color = Rgb(0x6c, 0x70, 0x86);
    pub(super) const OVERLAY1: Color = Rgb(0x7f, 0x84, 0x9c);
    // Text.
    pub(super) const SUBTEXT0: Color = Rgb(0xa6, 0xad, 0xc8);
    pub(super) const TEXT: Color = Rgb(0xcd, 0xd6, 0xf4);
    // Accents.
    pub(super) const RED: Color = Rgb(0xf3, 0x8b, 0xa8);
    pub(super) const GREEN: Color = Rgb(0xa6, 0xe3, 0xa1);
    pub(super) const YELLOW: Color = Rgb(0xf9, 0xe2, 0xaf);
    pub(super) const PEACH: Color = Rgb(0xfa, 0xb3, 0x87);
    pub(super) const MAUVE: Color = Rgb(0xcb, 0xa6, 0xf7);
    pub(super) const LAVENDER: Color = Rgb(0xb4, 0xbe, 0xfe);
}

// Structural diff fills tuned for the dark base; syntax token colors come from the theme.
const DEL_BG: Color = Color::Rgb(0x45, 0x23, 0x2f);
const INS_BG: Color = Color::Rgb(0x1f, 0x3a, 0x2a);
// Word-emphasis fills — a brighter shade of the row tint over the changed words.
const EMPH_DEL_BG: Color = Color::Rgb(0x6e, 0x34, 0x46);
const EMPH_INS_BG: Color = Color::Rgb(0x30, 0x55, 0x3f);
// A passive < selection < cursor brightness ramp: the cursor is the focal point, an active
// selection outranks a passive fold separator.
const CURSOR_BG: Color = cat::SURFACE2;
const SEL_BG: Color = cat::SURFACE1;
const FOLD_BG: Color = cat::SURFACE0;

/// The cursor-row fill: full brightness in the focused pane, a step dimmer when the pane is
/// not focused, so which pane has the cursor reads at a glance.
fn cursor_bg(focused: bool) -> Color {
    if focused { CURSOR_BG } else { cat::SURFACE1 }
}

/// The line-number column width for a diff of `rows` lines.
fn gutter_width(rows: usize) -> usize {
    rows.to_string().len().max(3)
}

/// The gutter prefix width: the change bar plus the right-aligned line number and a space.
fn gutter_prefix_width(gutter_w: usize) -> usize {
    1 + gutter_w + 1
}

/// How many display rows a row needs: 1 for a fold or with wrap off, else the number of
/// word-wrapped segments its (tab-expanded) content fills. Shares [`wrap_segments`] with
/// the renderer so per-row geometry stays aligned with what gets painted.
fn row_height(row: &Row, gutter_w: usize, width: usize, wrap: bool) -> usize {
    if !wrap || matches!(row, Row::Fold { .. }) {
        return 1;
    }
    let code_width = width.saturating_sub(gutter_prefix_width(gutter_w)).max(1);
    wrap_segments(&code_cells(row, false), code_width).len()
}

/// The diff-pane layout: constant for a frame.
#[derive(Clone, Copy)]
struct RowLayout {
    gutter_w: usize,
    width: usize,
    h_scroll: usize,
    wrap: bool,
    /// Whether the diff pane is focused — dims the cursor row when it is not.
    focused: bool,
}

/// A row's per-row highlight state.
#[derive(Clone, Copy)]
struct RowState {
    commented: bool,
    cursor: bool,
    selected: bool,
}

/// A diff row as one or more full-width display lines: a left change bar, the line
/// number, then syntax-colored code tinted red/green. With wrap on, a long line breaks
/// into `code_width`-wide rows; a continuation row carries a blank gutter so numbers
/// stay aligned. With wrap off, the line is one row scrolled by `h_scroll`.
fn render_row(row: &Row, layout: RowLayout, state: RowState) -> Vec<Line<'static>> {
    let RowLayout { gutter_w, width, h_scroll, wrap, focused } = layout;
    let RowState { commented, cursor, selected } = state;
    if let Row::Fold { .. } = row {
        let label = if cursor {
            format!("  ⋯  {} unmodified lines — → expand", row.hidden())
        } else {
            format!("  ⋯  {} unmodified lines", row.hidden())
        };
        let mut line = Line::from(Span::styled(label, Style::default().fg(cat::SUBTEXT0)));
        if let Some(pad) = width.checked_sub(line.width()).filter(|p| *p > 0) {
            line.push_span(Span::raw(" ".repeat(pad)));
        }
        let bg = if cursor { cursor_bg(focused) } else { FOLD_BG };
        return vec![line.style(Style::default().bg(bg).add_modifier(Modifier::BOLD))];
    }
    let num = row.new_no().or_else(|| row.old_no()).map_or(String::new(), |n| n.to_string());
    // A commented line's number takes the peach comment accent; others sit a step brighter
    // than the dim chrome so they stay legible while read.
    let num_color = if commented { cat::PEACH } else { cat::OVERLAY1 };
    let (bar, bar_color) = match row.marker() {
        '-' => ("▌", cat::RED),
        '+' => ("▌", cat::GREEN),
        _ => (" ", cat::OVERLAY0),
    };
    let row_bg = if cursor {
        Some(cursor_bg(focused))
    } else if selected {
        Some(SEL_BG)
    } else {
        match row.marker() {
            '-' => Some(DEL_BG),
            '+' => Some(INS_BG),
            _ => None,
        }
    };

    // Word emphasis brightens the changed words, unless the row's fill is a cursor or
    // selection bg, which wins for readability.
    let emph_on = !cursor && !selected;
    let emph_bg = match row.marker() {
        '-' => EMPH_DEL_BG,
        '+' => EMPH_INS_BG,
        _ => INS_BG,
    };
    let cells = code_cells(row, emph_on);

    let prefix_w = gutter_prefix_width(gutter_w);
    let code_width = width.saturating_sub(prefix_w).max(1);
    // Without wrap the line is one chunk scrolled by `h_scroll`; with wrap, word-wrapped
    // segments, the first numbered and the rest blank-gutter.
    let chunks: Vec<&[Cell]> = if wrap {
        wrap_segments(&cells, code_width).into_iter().map(|(s, e)| &cells[s..e]).collect()
    } else {
        vec![cells.get(skip_columns(&cells, h_scroll)..).unwrap_or(&[])]
    };

    chunks
        .into_iter()
        .enumerate()
        .map(|(k, chunk)| {
            let gutter = if k == 0 {
                vec![
                    Span::styled(bar, Style::default().fg(bar_color)),
                    Span::styled(format!("{num:>gutter_w$} "), Style::default().fg(num_color)),
                ]
            } else {
                // A continuation row keeps the change bar but blanks the number column.
                vec![
                    Span::styled(bar, Style::default().fg(bar_color)),
                    Span::raw(" ".repeat(prefix_w - 1)),
                ]
            };
            let mut spans = gutter;
            spans.extend(cells_to_spans(chunk, emph_bg));
            let mut line = Line::from(spans);
            if let Some(pad) = width.checked_sub(line.width()).filter(|p| *p > 0) {
                line.push_span(Span::raw(" ".repeat(pad)));
            }
            match row_bg {
                Some(bg) => line.style(Style::default().bg(bg)),
                None => line,
            }
        })
        .collect()
}

fn rgb(c: crate::diff::Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Tabs expand to this many columns.
const TAB: usize = 4;

/// Greedy word wrap over display cells into half-open ranges, one per display row.
///
/// Breaks at the last space that fits within `width`, falling back to a hard break when a
/// single word is wider than the column. Leading spaces on a continuation are dropped so a
/// break landing just before a space doesn't leave an almost-empty row. An empty line still
/// yields one (empty) range so it occupies a row. The renderer and [`row_height`] share this
/// so what's measured matches what's painted.
fn wrap_segments(cells: &[Cell], width: usize) -> Vec<(usize, usize)> {
    if cells.is_empty() {
        return vec![(0, 0)];
    }
    let mut segs = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        // Take as many cells as fit within `width` columns, always at least one (so a glyph
        // wider than the column still gets its own row rather than stalling).
        let mut col = 0;
        let mut limit = start;
        while limit < cells.len() {
            let cw = cells[limit].w;
            if col + cw > width && limit > start {
                break;
            }
            col += cw;
            limit += 1;
        }
        if limit == cells.len() {
            segs.push((start, cells.len()));
            break;
        }
        // More cells follow; prefer breaking just after the last space that fits.
        let brk = (start..limit).rev().find(|&i| cells[i].ch == ' ').map(|i| i + 1);
        let end = brk.filter(|&e| e > start).unwrap_or(limit);
        segs.push((start, end));
        start = end;
        while start < cells.len() && cells[start].ch == ' ' {
            start += 1;
        }
    }
    segs
}

/// The first cell index lying at or past `cols` display columns — the no-wrap horizontal
/// scroll offset, snapping past a wide glyph that straddles the boundary rather than
/// splitting it.
fn skip_columns(cells: &[Cell], cols: usize) -> usize {
    let mut col = 0;
    let mut i = 0;
    while i < cells.len() && col < cols {
        col += cells[i].w;
        i += 1;
    }
    i
}

/// One display cell of a code line: a glyph, its terminal width in columns (1 for most
/// text, 2 for wide CJK/emoji, 0 for a combining mark), its syntax color, and whether it
/// falls in a word-emphasis range.
struct Cell {
    ch: char,
    w: usize,
    fg: Color,
    emph: bool,
}

/// Expand a row's spans into display cells: tabs become spaces to the next tab stop, and
/// each char carries its column width, color, and (when `emph_on`) its word-emphasis flag.
/// Width comes from `unicode-width` so wide glyphs measure as the two columns they paint.
fn code_cells(row: &Row, emph_on: bool) -> Vec<Cell> {
    let emphasis = if emph_on { row.emphasis() } else { &[] };
    let in_emph = |i: u32| emphasis.iter().any(|&(a, b)| i >= a && i < b);
    let mut cells = Vec::new();
    let mut idx = 0u32;
    let mut col = 0usize; // display column, so tab stops land right after wide glyphs too
    for s in row.spans() {
        let fg = rgb(s.color);
        for ch in s.text.chars() {
            let emph = in_emph(idx);
            if ch == '\t' {
                for _ in 0..(TAB - col % TAB) {
                    cells.push(Cell { ch: ' ', w: 1, fg, emph });
                    col += 1;
                }
            } else {
                let w = UnicodeWidthChar::width(ch).unwrap_or(0);
                cells.push(Cell { ch, w, fg, emph });
                col += w;
            }
            idx += 1;
        }
    }
    cells
}

/// Build spans from display cells, merging runs of equal color/emphasis; an emphasized
/// run takes `emph_bg` as its background.
fn cells_to_spans(cells: &[Cell], emph_bg: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<(Color, bool)> = None;
    for c in cells {
        let key = (c.fg, c.emph);
        if cur != Some(key) {
            if let Some((fg, emph)) = cur {
                spans.push(cell_span(std::mem::take(&mut buf), fg, emph, emph_bg));
            }
            cur = Some(key);
        }
        buf.push(c.ch);
    }
    if let Some((fg, emph)) = cur {
        spans.push(cell_span(buf, fg, emph, emph_bg));
    }
    spans
}

fn cell_span(text: String, fg: Color, emph: bool, emph_bg: Color) -> Span<'static> {
    let style = Style::default().fg(fg);
    Span::styled(text, if emph { style.bg(emph_bg) } else { style })
}

/// The inline comment input box, drawn at `area` (under the selection in the diff).
fn render_composer(frame: &mut Frame, app: &App, area: Rect) {
    let loc = app.pending_location().unwrap_or_else(|| "comment".to_string());
    let editing = matches!(app.mode, Mode::Composing { editing: Some(_) });
    let title = if editing { format!("edit · {loc}") } else { format!("comment · {loc}") };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(cat::PEACH))
        .title(title);
    let content_w = composer_content_width(area.width as usize);
    let body = Paragraph::new(composer_lines(app, content_w)).block(block);
    frame.render_widget(body, area);
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let left = format!(" {} file(s) · {} comment(s) ", app.files.len(), app.store.len());
    let mid = if app.status.is_empty() { String::new() } else { format!("· {} ", app.status) };
    let hints = match app.mode {
        Mode::Composing { .. } => "enter save · alt/shift+enter newline · esc cancel",
        Mode::List => "↑↓ move · s send · y copy · e edit · d delete · esc close",
        Mode::Normal => {
            "tab focus · u/b scope · v select · c comment · s send · y copy · n/N jump · l list · r refresh · q quit"
        }
    };
    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(cat::TEXT)),
        Span::styled(format!(" {mid}"), Style::default().fg(cat::PEACH)),
        Span::styled(format!("  {hints}"), Style::default().fg(cat::OVERLAY0)),
    ]);
    // Fill the whole footer with the surface bar, matching the header band.
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(cat::SURFACE0)).wrap(Wrap { trim: false }),
        area,
    );
}

fn render_comments_list(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered(area, 80, 60);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(cat::MAUVE))
        .title(format!("Comments ({})", app.store.len()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let width = inner.width as usize;
    let stale = app.stale_files();
    let items: Vec<ListItem> = app
        .store
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let loc = Span::styled(
                c.location(),
                Style::default().fg(cat::MAUVE).add_modifier(Modifier::BOLD),
            );
            let mut spans = vec![loc, Span::styled(format!("  {}", c.text), text_style())];
            // A comment whose file has left the changeset is flagged but kept.
            if stale.contains(&c.file) {
                spans.push(Span::styled("  (stale)", Style::default().fg(cat::RED)));
            }
            // The list overlay is the active modal, so its row reads at full brightness.
            selectable_row(spans, width, (i == app.list_cursor).then_some(CURSOR_BG))
        })
        .collect();
    frame.render_widget(List::new(items), inner);
}

/// The default body text color.
fn text_style() -> Style {
    Style::default().fg(cat::TEXT)
}

/// A list row, highlighted with the shared selection fill (`surface2` + bold, full
/// width) when `selected` — the same treatment the diff cursor uses, so every cursor
/// in the UI reads the same. The fill is applied per span (with a trailing pad) so it
/// spans the full width under the `List` widget, matching the diff's `Paragraph` rows.
fn selectable_row(
    mut spans: Vec<Span<'static>>,
    width: usize,
    fill: Option<Color>,
) -> ListItem<'static> {
    if let Some(bg) = fill {
        let used: usize = spans.iter().map(Span::width).sum();
        if width > used {
            spans.push(Span::raw(" ".repeat(width - used)));
        }
        for s in &mut spans {
            s.style = s.style.bg(bg).add_modifier(Modifier::BOLD);
        }
    }
    ListItem::new(Line::from(spans))
}

// --- helpers -------------------------------------------------------------------

fn bordered(title: &str, focused: bool) -> Block<'_> {
    // A focused pane gets a lavender border; an unfocused one recedes to a surface tone.
    let color = if focused { cat::LAVENDER } else { cat::SURFACE2 };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(title.to_string())
}

fn dim_paragraph(text: &str) -> Paragraph<'_> {
    Paragraph::new(text).style(Style::default().fg(cat::OVERLAY0))
}

/// The Catppuccin accent for a change marker, matched to the diff's add/remove hues.
fn kind_color(marker: char) -> Color {
    match marker {
        'A' | '?' => cat::GREEN,
        'D' => cat::RED,
        'R' => cat::MAUVE,
        _ => cat::YELLOW,
    }
}

/// Whether `(col, row)` falls inside `rect`.
fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// The content area inside a one-cell border.
fn inner_rect(outer: Rect) -> Rect {
    Rect {
        x: outer.x.saturating_add(1),
        y: outer.y.saturating_add(1),
        width: outer.width.saturating_sub(2),
        height: outer.height.saturating_sub(2),
    }
}

/// A `Rect` centered in `area` at `pct_x` × `pct_y` percent of its size.
fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}
