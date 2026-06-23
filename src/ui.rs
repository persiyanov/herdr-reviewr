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

use crate::app::{App, Focus, Mode};
use crate::git::{DiffLine, DiffLineKind};

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let rows = vrows(area);
    let (diff_area, files_area) = body_split(&rows);

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

/// The body split into `(diff, files)` outer rects.
fn body_split(rows: &[Rect]) -> (Rect, Rect) {
    let body =
        Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)]).split(rows[1]);
    (body[0], body[1])
}

/// The file index a click at `(col, row)` lands on, or `None` if outside the list.
#[must_use]
pub fn hit_file(area: Rect, col: u16, row: u16, n_files: usize) -> Option<usize> {
    let rows = vrows(area);
    let (_, files_area) = body_split(&rows);
    let inner = inner_rect(files_area);
    if !contains(inner, col, row) {
        return None;
    }
    let idx = (row - inner.y) as usize;
    (idx < n_files).then_some(idx)
}

/// The diff-line index a click at `(col, row)` lands on, or `None` if outside the
/// diff pane. `diff_scroll` fixes the window so the mapping matches the paint.
#[must_use]
pub fn hit_diff(
    area: Rect,
    col: u16,
    row: u16,
    diff_len: usize,
    diff_scroll: usize,
) -> Option<usize> {
    let rows = vrows(area);
    let (diff_area, _) = body_split(&rows);
    let inner = inner_rect(diff_area);
    if !contains(inner, col, row) {
        return None;
    }
    let start = diff_scroll.min(diff_len.saturating_sub(inner.height as usize));
    let idx = start + (row - inner.y) as usize;
    (idx < diff_len).then_some(idx)
}

/// The number of diff rows visible in the diff pane, used to clamp the scroll.
#[must_use]
pub fn diff_viewport_height(area: Rect) -> usize {
    let rows = vrows(area);
    let (diff_area, _) = body_split(&rows);
    inner_rect(diff_area).height as usize
}

/// Rows the inline comment box occupies: one per input line plus the border.
#[must_use]
pub fn composer_height(app: &App) -> usize {
    app.input.split('\n').count() + 2
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

    let bar = Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD);
    let line = Line::from(vec![
        Span::styled(HEADER_PREFIX, bar),
        Span::styled(
            chip,
            Style::default().fg(Color::Yellow).bg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(suffix, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            button,
            Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_file_list(frame: &mut Frame, app: &App, area: Rect) {
    let block = bordered("Files", app.focus == Focus::Files);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.files.is_empty() {
        frame.render_widget(dim_paragraph("no changes"), inner);
        return;
    }

    let items: Vec<ListItem> = app
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let selected = i == app.file_cursor;
            let marker = Span::styled(
                format!("{} ", f.kind.marker()),
                Style::default().fg(kind_color(f.kind.marker())),
            );
            let name = Span::styled(f.path.clone(), name_style(selected));
            let stat = Span::styled(
                format!("  +{} -{}", f.additions, f.deletions),
                Style::default().fg(Color::DarkGray),
            );
            ListItem::new(Line::from(vec![marker, name, stat]))
        })
        .collect();
    frame.render_widget(List::new(items), inner);
}

fn render_diff_view(frame: &mut Frame, app: &App, area: Rect) {
    let title = app.diff_path.clone().unwrap_or_else(|| "Diff".to_string());
    let block = bordered(&title, app.focus == Focus::Diff);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.diff.is_empty() {
        frame.render_widget(dim_paragraph("no diff"), inner);
        return;
    }
    if app.diff.iter().any(|l| l.text.starts_with("Binary ")) {
        frame.render_widget(dim_paragraph("binary — no line comments"), inner);
        return;
    }

    let height = inner.height as usize;
    if height == 0 {
        return;
    }
    let commented = app.commented_lines();
    let (lo, hi) = app.selection_range();
    let selecting = app.focus == Focus::Diff && app.select_anchor.is_some();

    let line_at = |i: usize| -> Line {
        let dl = &app.diff[i];
        let gutter = if commented.contains(&i) {
            Span::styled("▌", Style::default().fg(Color::Yellow))
        } else {
            Span::raw(" ")
        };
        let mut style = diff_style(dl);
        if app.focus == Focus::Diff && i == app.diff_cursor {
            style = style.add_modifier(Modifier::REVERSED);
        } else if selecting && i >= lo && i <= hi {
            style = style.bg(Color::Rgb(40, 40, 60));
        }
        Line::from(vec![gutter, Span::styled(dl.text.clone(), style)])
    };

    if !app.composing() {
        let start = app.diff_scroll.min(app.diff.len().saturating_sub(height));
        let end = (start + height).min(app.diff.len());
        frame.render_widget(Paragraph::new((start..end).map(&line_at).collect::<Vec<_>>()), inner);
        return;
    }

    // Composing: splice the input box in directly under the last selected line, so the
    // diff lines below it shift down. The diff shares the pane with the box, so clamp the
    // window top against the reduced diff budget — matching the scroll reserved in the
    // event loop — rather than the full height.
    // Cap the box at height-1 so a comment taller than the viewport can't hide its anchor.
    let box_h = composer_height(app).min(height.saturating_sub(1)).max(1);
    let diff_rows = height - box_h;
    let start = app.diff_scroll.min(app.diff.len().saturating_sub(diff_rows));
    let last = app.diff.len() - 1;
    let anchor = hi.clamp(start, last);
    let above_n = (anchor + 1 - start).min(diff_rows);
    let below_start = start + above_n;
    let below_n = diff_rows - above_n;
    let slots = Layout::vertical([
        Constraint::Length(above_n as u16),
        Constraint::Length(box_h as u16),
        Constraint::Length(below_n as u16),
    ])
    .split(inner);

    if above_n > 0 {
        frame.render_widget(
            Paragraph::new((start..start + above_n).map(&line_at).collect::<Vec<_>>()),
            slots[0],
        );
    }
    render_composer(frame, app, slots[1]);
    if below_n > 0 {
        let end = (below_start + below_n).min(app.diff.len());
        frame.render_widget(
            Paragraph::new((below_start..end).map(&line_at).collect::<Vec<_>>()),
            slots[2],
        );
    }
}

/// The inline comment input box, drawn at `area` (under the selection in the diff).
fn render_composer(frame: &mut Frame, app: &App, area: Rect) {
    let loc = app.pending_location().unwrap_or_else(|| "comment".to_string());
    let editing = matches!(app.mode, Mode::Composing { editing: Some(_) });
    let title = if editing { format!("edit · {loc}") } else { format!("comment · {loc}") };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title);
    let input_lines: Vec<&str> = app.input.split('\n').collect();
    let last = input_lines.len() - 1;
    let lines: Vec<Line> = input_lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            if i == last {
                Line::from(vec![
                    Span::raw((*text).to_string()),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                ])
            } else {
                Line::from((*text).to_string())
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
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
        Span::styled(left, Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::styled(format!(" {mid}"), Style::default().fg(Color::Yellow)),
        Span::styled(format!("  {hints}"), Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), area);
}

fn render_comments_list(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered(area, 80, 60);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!("Comments ({})", app.store.len()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let stale = app.stale_files();
    let items: Vec<ListItem> = app
        .store
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let selected = i == app.list_cursor;
            let loc = Span::styled(
                c.location(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            );
            let text = Span::styled(
                format!("  {}", c.text),
                if selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                },
            );
            let mut spans = vec![loc, text];
            // A comment whose file has left the changeset is flagged but kept.
            if stale.contains(&c.file) {
                spans.push(Span::styled("  (stale)", Style::default().fg(Color::Red)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    frame.render_widget(List::new(items), inner);
}

// --- helpers -------------------------------------------------------------------

fn bordered(title: &str, focused: bool) -> Block<'_> {
    let color = if focused { Color::Cyan } else { Color::DarkGray };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(title.to_string())
}

fn dim_paragraph(text: &str) -> Paragraph<'_> {
    Paragraph::new(text).style(Style::default().fg(Color::DarkGray))
}

fn name_style(selected: bool) -> Style {
    if selected {
        Style::default().add_modifier(Modifier::REVERSED).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn kind_color(marker: char) -> Color {
    match marker {
        'A' | '?' => Color::Green,
        'D' => Color::Red,
        'R' => Color::Magenta,
        _ => Color::Yellow,
    }
}

fn diff_style(dl: &DiffLine) -> Style {
    match dl.kind {
        DiffLineKind::Added => Style::default().fg(Color::Green),
        DiffLineKind::Removed => Style::default().fg(Color::Red),
        DiffLineKind::Hunk => Style::default().fg(Color::Cyan),
        DiffLineKind::Meta => Style::default().fg(Color::DarkGray),
        DiffLineKind::Context => Style::default(),
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
