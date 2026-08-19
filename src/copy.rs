//! Character-level, pane-local copy mode.

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::export::{Clipboard, ExportTarget};
use crate::file_list::RowKind;
use crate::ui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Files,
    Diff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub pane: Pane,
    pub start: (u16, u16),
    pub end: (u16, u16),
}

/// The live copy gesture, including the small amount of state needed to recognize a
/// terminal-style double click. The selection itself stays public because the event loop
/// paints it after the normal frame.
#[derive(Default, Debug)]
pub struct State {
    pub selection: Option<Selection>,
    last_click: Option<Click>,
    source_override: Option<SourceSelection>,
    source_anchor: Option<SourcePoint>,
}

impl State {
    pub fn clear(&mut self) {
        self.selection = None;
        self.last_click = None;
        self.source_override = None;
        self.source_anchor = None;
    }
}

/// A point in the source text behind a painted pane position.
///
/// The display column is measured after tab expansion, so it can be mapped back to a source
/// character without making the rendered terminal buffer the source of truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SourcePoint {
    pub line: usize,
    pub display_column: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceSelection {
    start: SourcePoint,
    end: SourcePoint,
}

#[derive(Clone, Copy, Debug)]
struct Click {
    pane: Pane,
    point: (u16, u16),
    at: Instant,
}

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

/// Return the direction in which an active drag should move the pane when the pointer reaches
/// its edge. The caller applies the scroll and re-hits the pointer against the newly visible
/// rows, so the selection remains logical rather than being tied to painted cells.
pub(crate) fn edge_scroll_delta(area: Rect, app: &App, pane: Pane, row: u16) -> isize {
    if !app.tab.is_file_tab() || (pane == Pane::Diff && app.preview_active()) {
        return 0;
    }
    let bounds = ui::copy_pane_rect(area, app, pane);
    let bottom = bounds.y.saturating_add(bounds.height.saturating_sub(1));
    if row <= bounds.y { -1 } else { isize::from(row >= bottom) }
}

pub fn mouse_event(
    app: &mut App,
    area: Rect,
    event: MouseEvent,
    state: &mut State,
) -> Result<bool> {
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let pane = [Pane::Files, Pane::Diff].into_iter().find(|&pane| {
                ui::copy_pane_rect(area, app, pane).contains((event.column, event.row).into())
            });
            if let Some(pane) = pane {
                let point =
                    clamp_point(ui::copy_pane_rect(area, app, pane), event.column, event.row);
                let double_click = state.last_click.take().is_some_and(|click| {
                    click.pane == pane
                        && click.point.1 == point.1
                        && click.point.0.abs_diff(point.0) <= 2
                        && click.at.elapsed() <= DOUBLE_CLICK_WINDOW
                });
                state.last_click = Some(Click { pane, point, at: Instant::now() });
                if double_click {
                    state.source_anchor = None;
                    if let Some((selection, source)) = token_selection(app, area, pane, point) {
                        state.selection = Some(selection);
                        state.source_override = source;
                    } else {
                        state.selection = Some(Selection { pane, start: point, end: point });
                        state.source_override = None;
                    }
                } else {
                    state.selection = Some(Selection { pane, start: point, end: point });
                    state.source_override = None;
                    state.source_anchor = copy_source_point(area, app, pane, point.0, point.1);
                }
            } else {
                state.last_click = None;
                state.source_anchor = None;
            }
            Ok(true)
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            state.last_click = None;
            state.source_override = None;
            let pane = state.selection.map(|selection| selection.pane);
            let start_y = state.selection.map(|selection| selection.start.1);
            let shifted_start = pane.zip(start_y).and_then(|(pane, start_y)| {
                scroll_selection_anchor(app, area, pane, event.row, start_y)
            });
            if let Some(current) = state.selection.as_mut() {
                if let Some(start_y) = shifted_start {
                    current.start.1 = start_y;
                }
                current.end = clamp_point(
                    ui::copy_pane_rect(area, app, current.pane),
                    event.column,
                    event.row,
                );
            }
            Ok(true)
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(current) = state.selection {
                let text =
                    selection_text(app, area, current, state.source_override, state.source_anchor)?;
                if !text.is_empty() {
                    Clipboard.export(&text)?;
                    app.status = "copied".into();
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn scroll_selection_anchor(
    app: &mut App,
    area: Rect,
    pane: Pane,
    row: u16,
    start_y: u16,
) -> Option<u16> {
    let delta = edge_scroll_delta(area, app, pane, row);
    if delta == 0 {
        return None;
    }
    let bounds = ui::copy_pane_rect(area, app, pane);
    let old_scroll = match pane {
        Pane::Files => app.file_scroll,
        Pane::Diff => app.diff_scroll,
    };
    let heights = (pane == Pane::Diff).then(|| ui::diff_row_heights(app, area));
    match pane {
        Pane::Files => {
            app.wheel_files(delta);
            app.bound_file_scroll(bounds.height as usize);
        }
        Pane::Diff => {
            app.wheel_diff(delta);
            if let Some(heights) = heights.as_deref() {
                app.bound_diff_scroll(heights, ui::diff_viewport_height(area, app));
            }
        }
    }
    let new_scroll = match pane {
        Pane::Files => app.file_scroll,
        Pane::Diff => app.diff_scroll,
    };
    if old_scroll == new_scroll {
        return None;
    }
    let shift = if let Some(heights) = heights {
        if new_scroll > old_scroll {
            heights[old_scroll..new_scroll].iter().sum()
        } else {
            heights[new_scroll..old_scroll].iter().sum()
        }
    } else {
        old_scroll.abs_diff(new_scroll)
    } as u16;
    let shifted = if new_scroll > old_scroll {
        start_y.saturating_sub(shift)
    } else {
        start_y.saturating_add(shift)
    };
    Some(shifted.clamp(bounds.y, bounds.y.saturating_add(bounds.height.saturating_sub(1))))
}

/// The source hit rectangle remains the full pane. Only code-pane painting excludes its
/// line-number/change-bar prefix; keeping hit-testing wide is what lets a drag begin in the
/// gutter and still copy the corresponding source text without copying that chrome.
pub fn visual_pane_rect(area: Rect, app: &App, pane: Pane) -> Rect {
    let bounds = ui::copy_pane_rect(area, app, pane);
    if pane == Pane::Diff && app.tab.is_file_tab() && !app.preview_active() {
        let gutter = diff_gutter_prefix(app) as u16;
        Rect::new(
            bounds.x.saturating_add(gutter),
            bounds.y,
            bounds.width.saturating_sub(gutter),
            bounds.height,
        )
    } else {
        bounds
    }
}

fn clamp_point(rect: Rect, col: u16, row: u16) -> (u16, u16) {
    (
        col.clamp(rect.x, rect.x.saturating_add(rect.width.saturating_sub(1))),
        row.clamp(rect.y, rect.y.saturating_add(rect.height.saturating_sub(1))),
    )
}

pub fn ordered(selection: Selection) -> ((u16, u16), (u16, u16)) {
    if (selection.start.1, selection.start.0) <= (selection.end.1, selection.end.0) {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    }
}

fn selection_text(
    app: &App,
    area: Rect,
    selection: Selection,
    source_override: Option<SourceSelection>,
    source_anchor: Option<SourcePoint>,
) -> Result<String> {
    if let Some(source) = source_override
        && let Some(lines) = source_lines(app, selection.pane)
        && let Some(text) = stream_source(&lines, source.start, source.end)
    {
        return Ok(text);
    }
    if let Some(source_start) = source_anchor
        && let Some(source_end) =
            copy_source_point(area, app, selection.pane, selection.end.0, selection.end.1)
    {
        let source_end = if selection.pane == Pane::Diff {
            extend_diff_end_if_overflowing(area, app, selection.end.0, selection.end.1, source_end)
        } else {
            source_end
        };
        if let Some(lines) = source_lines(app, selection.pane)
            && let Some(text) = stream_source(&lines, source_start, source_end)
        {
            return Ok(text);
        }
    }
    if let Some(text) = model_selection_text(app, area, selection) {
        return Ok(text);
    }

    visual_selection_text(app, area, selection)
}

fn model_selection_text(app: &App, area: Rect, selection: Selection) -> Option<String> {
    if let Some(text) = whole_file_selection(app, area, selection) {
        return Some(text);
    }
    let (start, screen_end) = ordered(selection);
    let start = if selection.pane == Pane::Files && start.1 < screen_end.1 {
        copy_file_selection_start_point(area, app, start.0, start.1)?
    } else {
        copy_source_point(area, app, selection.pane, start.0, start.1)?
    };
    let end = copy_source_point(area, app, selection.pane, screen_end.0, screen_end.1)?;
    let end = if selection.pane == Pane::Diff {
        extend_diff_end_if_overflowing(area, app, screen_end.0, screen_end.1, end)
    } else {
        end
    };
    let lines = source_lines(app, selection.pane)?;
    stream_source(&lines, start, end)
}

fn extend_diff_end_if_overflowing(
    area: Rect,
    app: &App,
    screen_col: u16,
    screen_row: u16,
    source_end: SourcePoint,
) -> SourcePoint {
    if app.wrap || app.preview_active() || !app.tab.is_file_tab() {
        return source_end;
    }
    let bounds = ui::copy_pane_rect(area, app, Pane::Diff);
    let content_end = bounds.x.saturating_add(bounds.width.saturating_sub(1));
    if screen_col < content_end {
        return source_end;
    }
    let Some(row) = app.visible.get(source_end.line) else {
        return source_end;
    };
    let source_width = source_cells(&row.text()).iter().map(|cell| cell.width).sum();
    SourcePoint {
        line: source_end.line,
        display_column: extend_display_end_if_overflowing(
            source_width,
            source_end.display_column,
            screen_row >= bounds.y && screen_row < bounds.y.saturating_add(bounds.height),
        ),
    }
}

fn extend_display_end_if_overflowing(
    source_width: usize,
    selected_display_column: usize,
    at_right_edge: bool,
) -> usize {
    if at_right_edge && source_width > selected_display_column.saturating_add(1) {
        usize::MAX
    } else {
        selected_display_column
    }
}

fn whole_file_selection(app: &App, area: Rect, selection: Selection) -> Option<String> {
    if selection.pane != Pane::Files || !app.tab.is_file_tab() {
        return None;
    }
    let (start, end) = ordered(selection);
    if start.1 != end.1 {
        return None;
    }
    let bounds = ui::copy_pane_rect(area, app, Pane::Files);
    let index = ui::hit_file(area, app, start.0, start.1, app.file_rows.len(), app.file_scroll)?;
    let end_index = ui::hit_file(area, app, end.0, end.1, app.file_rows.len(), app.file_scroll)?;
    if index != end_index {
        return None;
    }
    let row = app.file_rows.get(index)?;
    let (source, shown, prefix) = copy_file_name_layout(app, row, bounds.width as usize);
    let name_start = bounds.x.saturating_add(prefix as u16);
    let name_end = name_start.saturating_add(shown.width().saturating_sub(1) as u16);
    (start.0 <= name_start && end.0 >= name_end).then_some(source)
}

fn token_selection(
    app: &App,
    area: Rect,
    pane: Pane,
    point: (u16, u16),
) -> Option<(Selection, Option<SourceSelection>)> {
    match pane {
        Pane::Files if app.tab.is_file_tab() => file_token_selection(app, area, point),
        Pane::Files => visual_token_selection(app, area, Pane::Files, point),
        Pane::Diff if app.tab.is_file_tab() && !app.preview_active() => {
            diff_token_selection(app, area, point)
        }
        Pane::Diff => visual_token_selection(app, area, Pane::Diff, point),
    }
}

fn file_token_selection(
    app: &App,
    area: Rect,
    point: (u16, u16),
) -> Option<(Selection, Option<SourceSelection>)> {
    let bounds = ui::copy_pane_rect(area, app, Pane::Files);
    let index = ui::hit_file(area, app, point.0, point.1, app.file_rows.len(), app.file_scroll)?;
    let row = app.file_rows.get(index)?;
    let (source, shown, prefix) = copy_file_name_layout(app, row, bounds.width as usize);
    let name_start = bounds.x.saturating_add(prefix as u16);
    let name_end = name_start.saturating_add(shown.width().saturating_sub(1) as u16);
    if point.0 < name_start || point.0 > name_end {
        return None;
    }
    let visual =
        Selection { pane: Pane::Files, start: (name_start, point.1), end: (name_end, point.1) };
    let end = source.width().saturating_sub(1);
    Some((
        visual,
        Some(SourceSelection {
            start: SourcePoint { line: index, display_column: 0 },
            end: SourcePoint { line: index, display_column: end },
        }),
    ))
}

fn diff_token_selection(
    app: &App,
    area: Rect,
    point: (u16, u16),
) -> Option<(Selection, Option<SourceSelection>)> {
    let source_point = copy_diff_source_point(area, app, point.0, point.1)?;
    let bounds = ui::copy_pane_rect(area, app, Pane::Diff);
    let content_start = bounds.x.saturating_add(diff_gutter_prefix(app) as u16);
    if point.0 < content_start {
        return None;
    }
    let text = app.visible.get(source_point.line)?.text();
    let char_index = display_column_to_char_index(&text, source_point.display_column);
    let (start_char, end_char) = token_char_range(&text, char_index)?;
    let start_column = char_index_to_display_column(&text, start_char);
    let end_column = char_index_to_display_column(&text, end_char.saturating_sub(1));
    let start = diff_screen_point(app, area, source_point.line, start_column)?;
    let end = diff_screen_point(app, area, source_point.line, end_column)?;
    Some((
        Selection { pane: Pane::Diff, start, end },
        Some(SourceSelection {
            start: SourcePoint { line: source_point.line, display_column: start_column },
            end: SourcePoint { line: source_point.line, display_column: end_column },
        }),
    ))
}

fn visual_token_selection(
    app: &App,
    area: Rect,
    pane: Pane,
    point: (u16, u16),
) -> Option<(Selection, Option<SourceSelection>)> {
    let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).ok()?;
    terminal.draw(|frame| ui::render(frame, app)).ok()?;
    let buffer = terminal.backend().buffer();
    let bounds = ui::copy_pane_rect(area, app, pane);
    if !bounds.contains(point.into()) {
        return None;
    }
    let is_token_cell = |x: u16| {
        buffer
            .cell((x, point.1))
            .is_some_and(|cell| cell.symbol().chars().any(|ch| !ch.is_whitespace()))
    };
    if !is_token_cell(point.0) {
        return None;
    }
    let left = (bounds.x..=point.0)
        .rev()
        .find(|&x| !is_token_cell(x))
        .map_or(bounds.x, |x| x.saturating_add(1));
    let right = (point.0..=bounds.x.saturating_add(bounds.width.saturating_sub(1)))
        .find(|&x| !is_token_cell(x))
        .map_or(bounds.x.saturating_add(bounds.width.saturating_sub(1)), |x| x.saturating_sub(1));
    Some((Selection { pane, start: (left, point.1), end: (right, point.1) }, None))
}

fn token_char_range(text: &str, index: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    if index >= chars.len() || chars[index].is_whitespace() {
        return None;
    }
    let dotted = is_dotted_token_char(chars[index]);
    let accept = |ch: char| if dotted { is_dotted_token_char(ch) } else { !ch.is_whitespace() };
    let mut start = index;
    while start > 0 && accept(chars[start - 1]) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < chars.len() && accept(chars[end]) {
        end += 1;
    }
    if dotted {
        while start < end && chars[start] == '.' {
            start += 1;
        }
        while end > start && chars[end - 1] == '.' {
            end -= 1;
        }
    }
    (start < end).then_some((start, end))
}

fn is_dotted_token_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '.'
}

fn char_index_to_display_column(text: &str, char_index: usize) -> usize {
    let mut display_column = 0;
    for (index, ch) in text.chars().enumerate() {
        if index >= char_index {
            break;
        }
        display_column += if ch == '\t' {
            4 - display_column % 4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
    }
    display_column
}

fn diff_screen_point(
    app: &App,
    area: Rect,
    line: usize,
    display_column: usize,
) -> Option<(u16, u16)> {
    let bounds = ui::copy_pane_rect(area, app, Pane::Diff);
    let heights = ui::diff_row_heights(app, area);
    if line < app.diff_scroll || line >= heights.len() {
        return None;
    }
    let row_y = bounds.y.saturating_add(
        u16::try_from(
            heights.iter().skip(app.diff_scroll).take(line - app.diff_scroll).sum::<usize>(),
        )
        .unwrap_or(u16::MAX),
    );
    let gutter_prefix = diff_gutter_prefix(app);
    let code_width = (bounds.width as usize).saturating_sub(gutter_prefix).max(1);
    let text = app.visible.get(line)?.text();
    let cells = source_cells(&text);
    let segments = source_segments(&cells, code_width, app.wrap);
    let (segment_index, segment_start_column, segment_end_column) = segments
        .iter()
        .enumerate()
        .map(|(index, &(start, end))| {
            let start_column = cells[..start].iter().map(|cell| cell.width).sum::<usize>();
            let end_column = cells[..end].iter().map(|cell| cell.width).sum::<usize>();
            (index, start_column, end_column)
        })
        .find(|&(_, start, end)| display_column <= end || start == end)
        .or_else(|| {
            segments.last().map(|&(start, end)| {
                let start_column = cells[..start].iter().map(|cell| cell.width).sum::<usize>();
                let end_column = cells[..end].iter().map(|cell| cell.width).sum::<usize>();
                (segments.len().saturating_sub(1), start_column, end_column)
            })
        })?;
    let y = row_y.saturating_add(segment_index as u16);
    let content_start = bounds.x.saturating_add(gutter_prefix as u16);
    let offset = if app.wrap {
        display_column
            .saturating_sub(segment_start_column)
            .min(segment_end_column.saturating_sub(segment_start_column).saturating_sub(1))
    } else {
        display_column.saturating_sub(app.h_scroll)
    };
    let x = content_start
        .saturating_add(offset as u16)
        .clamp(content_start, bounds.x.saturating_add(bounds.width.saturating_sub(1)));
    Some((x, y.min(bounds.y.saturating_add(bounds.height.saturating_sub(1)))))
}

fn copy_source_point(area: Rect, app: &App, pane: Pane, col: u16, row: u16) -> Option<SourcePoint> {
    if !app.tab.is_file_tab() {
        return None;
    }
    match pane {
        Pane::Files => copy_file_source_point(area, app, col, row),
        Pane::Diff if !app.preview_active() => copy_diff_source_point(area, app, col, row),
        Pane::Diff => None,
    }
}

fn copy_file_source_point(area: Rect, app: &App, col: u16, row: u16) -> Option<SourcePoint> {
    let bounds = ui::copy_pane_rect(area, app, Pane::Files);
    if !bounds.contains((col, row).into()) {
        return None;
    }
    let index = ui::hit_file(area, app, col, row, app.file_rows.len(), app.file_scroll)?;
    let file_row = app.file_rows.get(index)?;
    let (source, shown, prefix) = copy_file_name_layout(app, file_row, bounds.width as usize);
    let visible_column = (col - bounds.x) as usize;
    Some(SourcePoint {
        line: index,
        display_column: map_file_column(&source, &shown, visible_column.saturating_sub(prefix)),
    })
}

fn copy_file_selection_start_point(
    area: Rect,
    app: &App,
    col: u16,
    row: u16,
) -> Option<SourcePoint> {
    let bounds = ui::copy_pane_rect(area, app, Pane::Files);
    if !bounds.contains((col, row).into()) {
        return None;
    }
    let index = ui::hit_file(area, app, col, row, app.file_rows.len(), app.file_scroll)?;
    let file_row = app.file_rows.get(index)?;
    let (source, shown, prefix) = copy_file_name_layout(app, file_row, bounds.width as usize);
    let visible_column = (col - bounds.x) as usize;
    Some(SourcePoint {
        line: index,
        display_column: file_selection_start_column(&source, &shown, prefix, visible_column),
    })
}

fn copy_file_name_layout(
    app: &App,
    row: &crate::file_list::Row,
    width: usize,
) -> (String, String, usize) {
    let source = file_row_text(app, row);
    let indent = "  ".repeat(row.depth);
    match &row.kind {
        RowKind::Dir { expanded, .. } => {
            let arrow = if *expanded { "▾ " } else { "▸ " };
            let prefix = indent.width() + arrow.width();
            // source is the complete relative directory path, while row.name is the
            // directory text actually painted at this depth. Keeping those separate makes
            // suffix mapping and double-click bounds agree with the visible row.
            let shown = format!("{}/", row.name);
            (source, shown, prefix)
        }
        RowKind::File { annotation, .. } => {
            let marker =
                annotation.as_ref().map_or(String::new(), |a| format!("{} ", a.change.marker()));
            let (additions, deletions) =
                annotation.as_ref().map_or((0, 0), |a| (a.additions, a.deletions));
            let stats = stats_str(additions, deletions);
            let gap = if stats.is_empty() { 0 } else { 2 };
            let fixed = indent.width() + marker.width() + stats.width() + gap;
            let shown = elide_head(&row.name, width.saturating_sub(fixed).max(1));
            (source, shown, indent.width() + marker.width())
        }
    }
}

fn stats_str(additions: u32, deletions: u32) -> String {
    match (additions, deletions) {
        (0, 0) => String::new(),
        (a, 0) => format!("+{a}"),
        (0, d) => format!("−{d}"),
        (a, d) => format!("+{a} −{d}"),
    }
}

fn elide_head(name: &str, max: usize) -> String {
    if name.width() <= max {
        return name.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut tail = String::new();
    let mut width = 0;
    for ch in name.chars().rev() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > budget {
            break;
        }
        tail.insert(0, ch);
        width += char_width;
    }
    if let Some(slash) = tail.find('/') {
        tail = tail[slash..].to_string();
    }
    format!("…{tail}")
}

fn copy_diff_source_point(area: Rect, app: &App, col: u16, row: u16) -> Option<SourcePoint> {
    let bounds = ui::copy_pane_rect(area, app, Pane::Diff);
    if !bounds.contains((col, row).into()) {
        return None;
    }
    let heights = ui::diff_row_heights(app, area);
    let index = ui::hit_diff(area, app, col, row, &heights, app.diff_scroll)?;
    let target = (row - bounds.y) as usize;
    let previous: usize =
        heights.iter().skip(app.diff_scroll).take(index.saturating_sub(app.diff_scroll)).sum();
    let row_offset = target.saturating_sub(previous);
    let source_row = app.visible.get(index)?;
    if matches!(source_row, crate::diff::Row::Fold { .. }) {
        return None;
    }

    let gutter_prefix = diff_gutter_prefix(app);
    let code_width = (bounds.width as usize).saturating_sub(gutter_prefix);
    let cells = source_cells(&source_row.text());
    let segments = source_segments(&cells, code_width.max(1), app.wrap);
    if row_offset >= segments.len() {
        return None;
    }
    let content_start = bounds.x.saturating_add(gutter_prefix as u16);
    let visible_column = col.saturating_sub(content_start) as usize;
    let display_column = if app.wrap {
        let (segment_start, segment_end) = segments[row_offset];
        let before = cells[..segment_start].iter().map(|cell| cell.width).sum::<usize>();
        let segment_width =
            cells[segment_start..segment_end].iter().map(|cell| cell.width).sum::<usize>();
        before + visible_column.min(segment_width)
    } else {
        app.h_scroll.saturating_add(visible_column)
    };

    Some(SourcePoint { line: index, display_column })
}

fn diff_gutter_prefix(app: &App) -> usize {
    let total_lines: usize =
        app.diff.rows.iter().map(|row| if row.is_content() { 1 } else { row.hidden() }).sum();
    1 + total_lines.to_string().len().max(3) + 1
}

#[derive(Clone, Copy)]
struct SourceCell {
    ch: char,
    width: usize,
}

fn source_cells(text: &str) -> Vec<SourceCell> {
    let mut cells = Vec::new();
    let mut column = 0;
    for ch in text.chars() {
        if ch == '\t' {
            let width = 4 - column % 4;
            cells.extend((0..width).map(|_| SourceCell { ch: ' ', width: 1 }));
            column += width;
        } else {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            cells.push(SourceCell { ch, width });
            column += width;
        }
    }
    cells
}

fn source_segments(cells: &[SourceCell], width: usize, wrap: bool) -> Vec<(usize, usize)> {
    if !wrap {
        return vec![(0, cells.len())];
    }
    if cells.is_empty() {
        return vec![(0, 0)];
    }
    let mut segments = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let mut columns = 0;
        let mut limit = start;
        while limit < cells.len() {
            let cell_width = cells[limit].width;
            if columns + cell_width > width && limit > start {
                break;
            }
            columns += cell_width;
            limit += 1;
        }
        if limit == cells.len() {
            segments.push((start, cells.len()));
            break;
        }
        let break_at = (start..limit).rev().find(|&i| cells[i].ch == ' ').map(|i| i + 1);
        let end = break_at.filter(|&end| end > start).unwrap_or(limit);
        segments.push((start, end));
        start = end;
        while start < cells.len() && cells[start].ch == ' ' {
            start += 1;
        }
    }
    segments
}

fn source_lines(app: &App, pane: Pane) -> Option<Vec<Option<String>>> {
    match pane {
        Pane::Files if app.tab.is_file_tab() => {
            Some(app.file_rows.iter().map(|row| Some(file_row_text(app, row))).collect())
        }
        Pane::Diff if app.tab.is_file_tab() && !app.preview_active() => Some(
            app.visible
                .iter()
                .map(|row| (!matches!(row, crate::diff::Row::Fold { .. })).then(|| row.text()))
                .collect(),
        ),
        _ => None,
    }
}

pub(crate) fn file_row_text(app: &App, row: &crate::file_list::Row) -> String {
    match &row.kind {
        RowKind::Dir { path, .. } => format!("{path}/"),
        RowKind::File { index, .. } => {
            app.entries.get(*index).map_or_else(|| row.name.clone(), |entry| entry.path.clone())
        }
    }
}

fn map_file_column(full: &str, shown: &str, column: usize) -> usize {
    if shown.starts_with('…') {
        return map_elided_column(full, shown, column);
    }
    let full_width = full.width();
    let shown_width = shown.width();
    if full.ends_with(shown) && full_width > shown_width {
        return (full_width - shown_width + column).min(full_width);
    }
    column.min(full_width)
}

fn file_selection_start_column(
    full: &str,
    shown: &str,
    prefix: usize,
    visible_column: usize,
) -> usize {
    if visible_column <= prefix { 0 } else { map_file_column(full, shown, visible_column - prefix) }
}

/// Map a column in a possibly head-elided name back to the full source name.
pub(crate) fn map_elided_column(full: &str, shown: &str, column: usize) -> usize {
    let full_width = full.width();
    if full_width <= shown.width() {
        return column.min(full_width);
    }
    let Some(tail) = shown.strip_prefix('…') else {
        return column.min(full_width);
    };
    if column == 0 {
        return 0;
    }
    let tail_start = full_width.saturating_sub(tail.width());
    (tail_start + column.saturating_sub(1)).min(full_width)
}

/// Convert a display column to a source character index, preserving tabs and wide glyphs.
pub(crate) fn display_column_to_char_index(text: &str, column: usize) -> usize {
    let mut display_column = 0;
    for (char_index, ch) in text.chars().enumerate() {
        let width = if ch == '\t' {
            4 - display_column % 4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if column < display_column.saturating_add(width) {
            return char_index;
        }
        display_column = display_column.saturating_add(width);
    }
    text.chars().count()
}

fn stream_source(
    lines: &[Option<String>],
    first: SourcePoint,
    second: SourcePoint,
) -> Option<String> {
    let (start, end) = if first <= second { (first, second) } else { (second, first) };
    let start_line = lines.get(start.line).and_then(Option::as_deref)?;
    let end_line = lines.get(end.line).and_then(Option::as_deref)?;
    let start_char = display_column_to_char_index(start_line, start.display_column);
    let end_char = display_column_to_char_index(end_line, end.display_column);

    if start.line == end.line {
        return Some(inclusive_chars(start_line, start_char, end_char));
    }

    let mut selected = Vec::with_capacity(end.line - start.line + 1);
    selected.push(from_char(start_line, start_char));
    selected.extend(
        lines[start.line + 1..end.line]
            .iter()
            .map(Option::as_deref)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .map(str::to_string),
    );
    selected.push(inclusive_chars(end_line, 0, end_char));
    Some(selected.join("\n"))
}

fn from_char(text: &str, start: usize) -> String {
    text.chars().skip(start).collect()
}

fn inclusive_chars(text: &str, start: usize, end: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if start >= chars.len() || start > end {
        return String::new();
    }
    let end = end.min(chars.len().saturating_sub(1));
    chars[start..=end].iter().collect()
}

fn visual_selection_text(app: &App, area: Rect, selection: Selection) -> Result<String> {
    let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))?;
    terminal.draw(|frame| ui::render(frame, app))?;
    let buffer = terminal.backend().buffer();
    let bounds = visual_pane_rect(area, app, selection.pane);
    let end_x = bounds.x.saturating_add(bounds.width.saturating_sub(1));
    let (start, end) = ordered(selection);
    let mut lines = Vec::new();
    for y in start.1..=end.1 {
        let (from, to) = if start.1 == end.1 {
            (start.0, end.0)
        } else if y == start.1 {
            (start.0, end_x)
        } else if y == end.1 {
            (bounds.x, end.0)
        } else {
            (bounds.x, end_x)
        };
        lines.push(
            (from.max(bounds.x)..=to.min(end_x))
                .filter_map(|x| buffer.cell((x, y)))
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_string(),
        );
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_columns_map_tabs_and_wide_glyphs_to_source_chars() {
        let text = "\t界x";
        assert_eq!(display_column_to_char_index(text, 0), 0);
        assert_eq!(display_column_to_char_index(text, 3), 0);
        assert_eq!(display_column_to_char_index(text, 4), 1);
        assert_eq!(display_column_to_char_index(text, 5), 1);
        assert_eq!(display_column_to_char_index(text, 6), 2);
        assert_eq!(display_column_to_char_index(text, 99), 3);
    }

    #[test]
    fn elided_names_map_the_visible_tail_and_overflow_to_the_full_name() {
        let full = "src/very-long-file.rs";
        let shown = "…/file.rs";
        let tail_start = full.width() - "/file.rs".width();
        assert_eq!(map_elided_column(full, shown, 1), tail_start);
        assert_eq!(map_elided_column(full, shown, shown.width()), full.width());
    }

    #[test]
    fn basename_columns_map_back_to_the_relative_path_suffix() {
        let full = "app/controllers/application_controller.rb";
        let shown = "application_controller.rb";
        let prefix = full.width() - shown.width();
        assert_eq!(map_file_column(full, shown, 0), prefix);
        assert_eq!(map_file_column(full, shown, shown.width() - 1), full.width() - 1);
    }

    #[test]
    fn directory_columns_map_the_visible_name_to_the_full_path_suffix() {
        let full = "scripts/bench-results/";
        let shown = "bench-results/";
        let prefix = full.width() - shown.width();
        assert_eq!(map_file_column(full, shown, 0), prefix);
        assert_eq!(map_file_column(full, shown, shown.width() - 1), full.width() - 1);
    }

    #[test]
    fn the_right_edge_expands_only_when_source_text_continues() {
        assert_eq!(extend_display_end_if_overflowing(20, 9, true), usize::MAX);
        assert_eq!(extend_display_end_if_overflowing(10, 9, true), 9);
        assert_eq!(extend_display_end_if_overflowing(20, 9, false), 9);
    }

    #[test]
    fn a_multi_file_selection_starts_at_the_full_first_path() {
        let full = "scripts/bench-results/baseline-v0.18.1-chained.json";
        let shown = "baseline-v0.18.1-chained.json";
        let prefix = 4;
        assert_eq!(file_selection_start_column(full, shown, prefix, prefix), 0);
        assert_eq!(
            file_selection_start_column(full, shown, prefix, prefix + 1),
            full.width() - shown.width() + 1
        );
    }

    #[test]
    fn dotted_identifiers_are_one_double_click_token() {
        let text = "some.model.method = x";
        let model = text.chars().position(|ch| ch == 'm').unwrap();
        assert_eq!(token_char_range(text, model), Some((0, "some.model.method".chars().count())));
        assert_eq!(
            token_char_range(text, text.chars().position(|ch| ch == '=').unwrap()),
            Some((18, 19))
        );
    }

    #[test]
    fn source_selection_keeps_the_full_line_and_crosses_source_lines() {
        let lines = vec![Some("a very long line".to_string()), Some("next line".to_string())];
        assert_eq!(
            stream_source(
                &lines,
                SourcePoint { line: 0, display_column: 0 },
                SourcePoint { line: 0, display_column: usize::MAX },
            ),
            Some("a very long line".to_string())
        );
        assert_eq!(
            stream_source(
                &lines,
                SourcePoint { line: 0, display_column: 2 },
                SourcePoint { line: 1, display_column: usize::MAX },
            ),
            Some("very long line\nnext line".to_string())
        );
    }

    #[test]
    fn a_fold_elsewhere_does_not_force_visual_copy() {
        let lines = vec![Some("kept".to_string()), None, Some("other".to_string())];
        assert_eq!(
            stream_source(
                &lines,
                SourcePoint { line: 0, display_column: 0 },
                SourcePoint { line: 0, display_column: usize::MAX },
            ),
            Some("kept".to_string())
        );
        assert_eq!(
            stream_source(
                &lines,
                SourcePoint { line: 0, display_column: 0 },
                SourcePoint { line: 2, display_column: usize::MAX },
            ),
            None
        );
    }

    #[test]
    fn wrapped_source_segments_drop_only_continuation_padding() {
        let cells = source_cells("alpha beta");
        assert_eq!(source_segments(&cells, 5, true), vec![(0, 5), (6, 10)],);
    }
}
