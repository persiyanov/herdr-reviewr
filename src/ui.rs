//! All rendering, and the hit tests that share its geometry; it only reads `App`.

use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Band, Focus, FooterAction, Mode, Tab};
use crate::config::NavigatorPosition;
use crate::diff::{FileDiff, MarkerKind, Notice, RenderedKind, Row};
use crate::file_list::RowKind;
use crate::forge;
use crate::git;
use crate::herdr::AgentChoice;
use crate::keymap::Keymap;
use crate::model::{ChangeKind, ChangedFile, Comment};
use crate::roles::Palette;
use crate::roles::{Fill, Ink};
use crate::snippet::{snippet_caption_sign, snippet_row_is_comment};
use std::fmt::Write as _;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    // Link hit-testing resolves against the painted frame; each frame repaints its own.
    app.clear_painted_frame();
    if let Some(error) = app.config_error() {
        let mut message =
            format!("{error}\n\nFix the file to continue. The config reloads automatically.");
        if app.confirming_quit {
            // The blocked screen reads the default bindings, as its key handling does.
            let key = crate::keymap::default_keymap().hint(crate::keymap::Action::QuitDiscard);
            let (n, key) = (app.unsent(), key.label());
            let _ = if n == 1 {
                write!(
                    message,
                    "\n\n1 unsent comment comes back once the config is fixed. Press {key} to quit and lose it."
                )
            } else {
                write!(
                    message,
                    "\n\n{n} unsent comments come back once the config is fixed. Press {key} to quit and lose them."
                )
            };
        }
        frame.render_widget(
            Paragraph::new(message).wrap(ratatui::widgets::Wrap { trim: false }),
            area,
        );
        return;
    }
    let p = panes(area, app);

    // The search screen replaces the body; the header and footer chrome stay
    if app.mode == Mode::Search {
        if app.tab == Tab::Pr {
            render_pr_header(frame, app, p.tab);
        } else {
            render_tab_bar(frame, app, p.tab);
        }
        render_search(frame, app, p.body);
        render_footer(frame, app, p.status);
        return;
    }

    if app.tab == Tab::Pr {
        render_pr_header(frame, app, p.tab);
        let (read, composer) = pr_compose_split(app, p.diff);
        render_pr_read(frame, app, read);
        if let Some(area) = composer {
            render_composer(frame, app, area);
        }
        // `PR` never hides its navigator, so no hidden gate here.
        render_pr_nav(frame, app, p.files);
    } else {
        render_tab_bar(frame, app, p.tab);
        render_diff_view(frame, app, p.diff);
        if !app.navigator_hidden_here() {
            render_file_list(frame, app, p.files);
        }
    }
    // The drag highlight paints over the finished body.
    render_text_selection(frame, app, area);
    // One footer band on every tab, drawn after the per-tab base so it sits on both layouts.
    render_footer(frame, app, p.status);

    // One match decides scrim and popup, so a scrim never comes without its popup.
    let popup: Option<fn(&mut Frame, &App, Rect)> = match app.mode {
        Mode::List => Some(render_comments_list),
        Mode::Picker => Some(render_agent_picker),
        Mode::BasePick => Some(render_base_picker),
        Mode::CommitPick => Some(render_commit_picker),
        Mode::Normal | Mode::Composing { .. } | Mode::Search | Mode::Find => None,
    };
    if let Some(render_popup) = popup {
        scrim_behind(frame, app, area);
        render_popup(frame, app, area);
    }
}

/// Recede the page behind a modal, all but the footer, which is the modal's key bar.
fn scrim_behind(frame: &mut Frame, app: &App, area: Rect) {
    let p = *app.palette();
    let bands = panes(area, app);
    let buf = frame.buffer_mut();
    for band in [bands.tab, bands.body] {
        for y in band.y..band.y + band.height {
            for x in band.x..band.x + band.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.fg = p.scrim(cell.fg);
                    cell.bg = p.scrim(cell.bg);
                }
            }
        }
    }
}

/// The vertical bands: tab bar, body, footer, which `?` grows while the body keeps 3 rows.
fn vrows(area: Rect, app: &App) -> Rc<[Rect]> {
    let footer = footer_height(app, area);
    Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(footer)])
        .split(area)
}

/// The frame's layout rects, computed once so paint and hit tests always agree.
struct Panes {
    tab: Rect,
    diff: Rect,
    files: Rect,
    body: Rect,
    status: Rect,
}

fn panes(area: Rect, app: &App) -> Panes {
    let rows = vrows(area, app);
    let body = rows[1];
    // A hidden navigator's zero-sized rect misses every hit test.
    let (diff, files) = if app.navigator_hidden_here() {
        (body, Rect::new(body.x, body.y, 0, 0))
    } else {
        split_body(body, app.navigator_position, app.navigator_share())
    };
    Panes { tab: rows[0], diff, files, body, status: rows[2] }
}

/// Split `axis_len` by `pct`, each side at least 3 cells once 6 exist, else evenly.
pub(crate) fn split_axis(axis_len: u16, pct: u16) -> u16 {
    let mut len = (u32::from(axis_len) * u32::from(pct) / 100) as u16;
    if axis_len >= 6 {
        len = len.clamp(3, axis_len - 3);
    } else {
        len = axis_len / 2;
    }
    len
}

fn split_body(body: Rect, position: NavigatorPosition, share: u16) -> (Rect, Rect) {
    let axis_len = if position.stacked() { body.height } else { body.width };
    let navigator_len = split_axis(axis_len, share);
    let read_len = axis_len - navigator_len;
    match position {
        NavigatorPosition::Right => (
            Rect::new(body.x, body.y, read_len, body.height),
            Rect::new(body.x + read_len, body.y, navigator_len, body.height),
        ),
        NavigatorPosition::Left => (
            Rect::new(body.x + navigator_len, body.y, read_len, body.height),
            Rect::new(body.x, body.y, navigator_len, body.height),
        ),
        NavigatorPosition::Bottom => (
            Rect::new(body.x, body.y, body.width, read_len),
            Rect::new(body.x, body.y + read_len, body.width, navigator_len),
        ),
        NavigatorPosition::Top => (
            Rect::new(body.x, body.y + navigator_len, body.width, read_len),
            Rect::new(body.x, body.y, body.width, navigator_len),
        ),
    }
}

/// The whole body band (between the tab bar and status bar), for divider hit-testing.
#[must_use]
pub fn body_rect(area: Rect, app: &App) -> Rect {
    vrows(area, app)[1]
}

/// Whether `(col, row)` lands on the draggable divider between the two panes.
#[must_use]
pub fn hit_divider(area: Rect, app: &App, col: u16, row: u16) -> bool {
    // A hidden navigator has no divider.
    if app.navigator_hidden_here() {
        return false;
    }
    let p = panes(area, app);
    match app.navigator_position {
        NavigatorPosition::Left => {
            contains(p.body, col, row) && at_seam(col, p.files.x + p.files.width)
        }
        NavigatorPosition::Right => contains(p.body, col, row) && at_seam(col, p.files.x),
        NavigatorPosition::Top => {
            contains(p.body, col, row) && at_seam(row, p.files.y + p.files.height)
        }
        NavigatorPosition::Bottom => contains(p.body, col, row) && at_seam(row, p.files.y),
    }
}

/// The two adjacent pane-border cells around a split boundary.
fn at_seam(coordinate: u16, boundary: u16) -> bool {
    coordinate == boundary || coordinate.checked_add(1) == Some(boundary)
}

/// The file row a click at `(col, row)` lands on.
#[must_use]
pub fn hit_file(
    area: Rect,
    app: &App,
    col: u16,
    row: u16,
    n_files: usize,
    file_scroll: usize,
) -> Option<usize> {
    let inner = inner_rect(panes(area, app).files);
    if !contains(inner, col, row) {
        return None;
    }
    let idx = (row - inner.y) as usize + file_scroll;
    (idx < n_files).then_some(idx)
}

/// The number of file rows visible in the file pane, used to clamp the file-list scroll.
#[must_use]
pub fn file_viewport_height(area: Rect, app: &App) -> usize {
    inner_rect(panes(area, app).files).height as usize
}

/// Whether `(col, row)` falls in the file pane, so the wheel scrolls the list it is over.
#[must_use]
pub fn in_files_pane(area: Rect, app: &App, col: u16, row: u16) -> bool {
    contains(panes(area, app).files, col, row)
}

/// Whether `(col, row)` falls in the read pane.
#[must_use]
pub fn in_diff_pane(area: Rect, app: &App, col: u16, row: u16) -> bool {
    contains(panes(area, app).diff, col, row)
}

/// The read pane's inner content rect, for the drag edge-scroll.
#[must_use]
pub fn read_inner_rect(area: Rect, app: &App) -> Rect {
    inner_rect(panes(area, app).diff)
}

/// The file navigator's inner content rect, for the drag edge-scroll.
#[must_use]
pub fn files_inner_rect(area: Rect, app: &App) -> Rect {
    inner_rect(panes(area, app).files)
}

/// The diff row a click lands on, any display line of a wrapped row included.
#[must_use]
pub fn hit_diff(
    area: Rect,
    app: &App,
    col: u16,
    row: u16,
    heights: &[usize],
    diff_scroll: usize,
) -> Option<usize> {
    let inner = inner_rect(panes(area, app).diff);
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

/// The comment whose card the painted frame shows at `(col, row)`, if any.
#[must_use]
pub fn card_at(area: Rect, app: &App, col: u16, row: u16) -> Option<usize> {
    card_slot_at(&read_pane(area, app), app, col, row).map(|(comment, _)| comment)
}

/// The painted card line under `(col, row)` in `pane`: its comment and body line.
fn card_slot_at(pane: &ReadPane, app: &App, col: u16, row: u16) -> Option<(usize, usize)> {
    if !contains(pane.inner, col, row) {
        return None;
    }
    match *app.painted_slots().get((row - pane.inner.y) as usize)? {
        Slot::Card { comment, line } => Some((comment, line)),
        _ => None,
    }
}

/// The number of diff rows visible in the diff pane, used to clamp the scroll.
#[must_use]
pub fn diff_viewport_height(area: Rect, app: &App) -> usize {
    let h = inner_rect(panes(area, app).diff).height as usize;
    // The find band takes the pane's bottom row, so the cursor reveals above it
    if app.mode == crate::app::Mode::Find { h.saturating_sub(1) } else { h }
}

/// The display height (rows on screen) of each visible logical diff row, honoring wrap.
#[must_use]
pub fn diff_row_heights(app: &App, area: Rect) -> Vec<usize> {
    let width = inner_rect(panes(area, app).diff).width as usize;
    let gutter_w = gutter_for(&app.diff);
    let p = app.palette();
    // Wrapped lines plus cards, minus the one being edited, exactly as painted.
    let cards = app.card_rows();
    let editing = editing_comment(app);
    app.visible
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let base = row_height(r, gutter_w, width, app.wrap);
            let card: usize = cards
                .iter()
                .filter(|&&(row, ci)| row == i && Some(ci) != editing)
                .filter_map(|&(_, ci)| app.store.get(ci))
                .map(|c| comment_card_lines(c, width, p).len())
                .sum();
            base + card
        })
        .collect()
}

/// One painted display line of the read pane, recorded for every hit test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// A code display line: the logical row and its wrap-segment index.
    Code { row: usize, seg: usize },
    /// A spliced comment card's display line: the store index and the line within the card.
    Card { comment: usize, line: usize },
    /// A composer display line, inert for selection.
    Composer,
}

/// The composer splice: anchor row, box height, and diff-line budget.
fn composing_split(app: &App, height: usize, width: usize) -> (usize, usize, usize) {
    // Cap the box at height-1 so a comment taller than the viewport can't hide its anchor.
    let box_h = composer_height(app, width).min(height.saturating_sub(1)).max(1);
    let diff_budget = height - box_h;
    let hi = app.compose_row();
    // Bound by hand: an edge scroll can pass the last row, and `clamp` would panic.
    let anchor = hi.max(app.diff_scroll).min(app.visible.len().saturating_sub(1));
    (anchor, box_h, diff_budget)
}

/// The last `cap` items of `v`.
fn tail<T: Clone>(v: Vec<T>, cap: usize) -> Vec<T> {
    if v.len() > cap { v[v.len() - cap..].to_vec() } else { v }
}

/// The read pane's display lines top to bottom, the one layout walk; empty on a notice.
fn read_layout(app: &App, inner: Rect, cards: &[(usize, usize)]) -> Vec<Slot> {
    if app.visible.is_empty() || inner.height == 0 {
        return Vec::new();
    }
    let height = inner.height as usize;
    let width = inner.width as usize;
    let gutter_w = gutter_for(&app.diff);
    let p = app.palette();
    let editing = editing_comment(app);
    let rows = app.visible.len();
    // Code lines, then card lines, in paint order.
    let row_slots = |i: usize| -> Vec<Slot> {
        let segs = row_height(&app.visible[i], gutter_w, width, app.wrap);
        let mut out: Vec<Slot> = (0..segs).map(|seg| Slot::Code { row: i, seg }).collect();
        for &(_, ci) in cards.iter().filter(|&&(row, _)| row == i) {
            if Some(ci) != editing
                && let Some(c) = app.store.get(ci)
            {
                out.extend(
                    (0..comment_card_lines(c, width, p).len())
                        .map(|line| Slot::Card { comment: ci, line }),
                );
            }
        }
        out
    };

    if !app.composing() {
        let body_h = if app.mode == Mode::Find { height.saturating_sub(1) } else { height };
        let mut out = Vec::new();
        for i in app.diff_scroll..rows {
            out.extend(row_slots(i));
            if out.len() >= body_h {
                break;
            }
        }
        out.truncate(body_h);
        return out;
    }

    // Composing: the box splices under the anchor's last display line (`composing_split`).
    let (anchor, box_h, diff_budget) = composing_split(app, height, width);
    let above = tail((app.diff_scroll..=anchor).flat_map(&row_slots).collect(), diff_budget);
    let remaining = diff_budget - above.len();
    let mut out = above;
    out.extend(std::iter::repeat_n(Slot::Composer, box_h));
    let mut below = Vec::new();
    for i in anchor + 1..rows {
        below.extend(row_slots(i));
        if below.len() >= remaining {
            break;
        }
    }
    below.truncate(remaining);
    out.extend(below);
    out
}

/// Re-record the layout after a mid-gesture scroll, so the same event hit-tests fresh.
pub fn refresh_read_layout(app: &App, area: Rect) {
    let pane = read_pane(area, app);
    app.note_painted_slots(read_layout(app, pane.inner, &app.card_rows()));
}

/// The cells a code display line paints: its wrap segment, or the `h_scroll`-skipped tail.
fn seg_cell_range(
    app: &App,
    row: &Row,
    cells: &[Cell],
    seg: usize,
    code_width: usize,
) -> (usize, usize) {
    if matches!(row, Row::Rendered { .. }) {
        (0, cells.len())
    } else if app.wrap {
        let segs = wrap_segments(cells, code_width.max(1), ContinuationSpaces::Trim);
        segs.get(seg).copied().unwrap_or((0, 0))
    } else {
        (skip_columns(cells, app.h_scroll), cells.len())
    }
}

/// The source char at a display column, past the end clamping to the last char.
fn seg_char_at(app: &App, row: &Row, seg: usize, code_width: usize, col_in_code: usize) -> usize {
    let cells = plain_cells(row);
    let (s, e) = seg_cell_range(app, row, &cells, seg, code_width);
    if s >= e {
        // Scrolled entirely off: the last char, as past the text below.
        return cells.last().map_or(0, |c| c.src);
    }
    let mut col = 0;
    for cell in &cells[s..e] {
        col += cell.w;
        if col > col_in_code {
            return cell.src;
        }
    }
    cells[e - 1].src
}

/// The widest visible row's width, the cap for a drag's horizontal edge scroll.
#[must_use]
pub fn widest_visible_row(app: &App, area: Rect) -> usize {
    let content = read_content_rect(area, app);
    app.visible
        .iter()
        .skip(app.diff_scroll)
        .take(content.height as usize)
        .map(|r| plain_cells(r).iter().map(|c| c.w).sum())
        .max()
        .unwrap_or(0)
}

/// The read-pane geometry shared by every selection map below.
struct ReadPane {
    inner: Rect,
    prefix_w: usize,
}

fn read_pane(area: Rect, app: &App) -> ReadPane {
    let inner = inner_rect(panes(area, app).diff);
    ReadPane { inner, prefix_w: gutter_prefix_width(gutter_for(&app.diff)) }
}

/// The read pane's content rows, minus the find band; a drag scrolls only past them.
#[must_use]
pub fn read_content_rect(area: Rect, app: &App) -> Rect {
    let mut inner = inner_rect(panes(area, app).diff);
    if app.mode == Mode::Find {
        inner.height = inner.height.saturating_sub(1);
    }
    inner
}

/// The selection point under `(col, row)` on a code line of the read pane.
#[must_use]
pub fn read_point_at(area: Rect, app: &App, col: u16, row: u16) -> Option<crate::selection::Point> {
    let pane = read_pane(area, app);
    if !contains(pane.inner, col, row) {
        return None;
    }
    // The gutter is chrome with its own gestures.
    if (col as usize) < pane.inner.x as usize + pane.prefix_w {
        return None;
    }
    let slots = app.painted_slots();
    let Slot::Code { row: li, seg } = *slots.get((row - pane.inner.y) as usize)? else {
        return None;
    };
    let r = &app.visible[li];
    if !r.is_content() {
        return None;
    }
    let code_width = (pane.inner.width as usize).saturating_sub(pane.prefix_w).max(1);
    let col_in_code = (col as usize).saturating_sub(pane.inner.x as usize + pane.prefix_w);
    Some(crate::selection::Point {
        row: li,
        chr: seg_char_at(app, r, seg, code_width, col_in_code),
    })
}

/// `read_point_at` for a drag's moving end, clamped to the nearest code line (`TS-ONE-SURFACE`).
#[must_use]
pub fn read_point_clamped(
    area: Rect,
    app: &App,
    col: u16,
    row: u16,
) -> Option<crate::selection::Point> {
    let pane = read_pane(area, app);
    if pane.inner.width == 0 || pane.inner.height == 0 {
        return None;
    }
    let col = col.clamp(pane.inner.x, pane.inner.x + pane.inner.width - 1);
    let row = row.clamp(pane.inner.y, pane.inner.y + pane.inner.height - 1);
    let slots = app.painted_slots();
    let at = ((row - pane.inner.y) as usize).min(slots.len().saturating_sub(1));
    let code = (0..=at)
        .rev()
        .chain(at + 1..slots.len())
        .find(|&i| matches!(slots.get(i), Some(Slot::Code { .. })))?;
    let Slot::Code { row: li, seg } = slots[code] else { return None };
    let r = &app.visible[li];
    if !r.is_content() {
        // A fold paints as one Code slot; snap its endpoint to the row's start.
        return Some(crate::selection::Point { row: li, chr: 0 });
    }
    let code_width = (pane.inner.width as usize).saturating_sub(pane.prefix_w).max(1);
    let col_in_code = (col as usize).saturating_sub(pane.inner.x as usize + pane.prefix_w);
    Some(crate::selection::Point {
        row: li,
        chr: seg_char_at(app, r, seg, code_width, col_in_code),
    })
}

/// The commentable row whose gutter `(col, row)` lands on, continuation lines included.
#[must_use]
pub fn gutter_row_at(area: Rect, app: &App, col: u16, row: u16) -> Option<usize> {
    if app.tab == Tab::Pr {
        return None;
    }
    let pane = read_pane(area, app);
    if !contains(pane.inner, col, row) || (col as usize) >= pane.inner.x as usize + pane.prefix_w {
        return None;
    }
    let slots = app.painted_slots();
    match *slots.get((row - pane.inner.y) as usize)? {
        Slot::Code { row: li, .. } if app.visible[li].is_content() => Some(li),
        _ => None,
    }
}

/// Paint one selected cell on the selection fill; text on a match or caret becomes body text.
fn paint_selected(cell: &mut ratatui::buffer::Cell, p: &Palette) {
    let on_solid = [Fill::Highlight, Fill::Caret].iter().any(|&f| cell.bg == p.fill(f));
    let fg = match cell.fg {
        Color::Rgb(..) if !on_solid => p.legible(cell.fg, Fill::Selection),
        _ => p.ink(Ink::Text, Fill::Selection),
    };
    cell.set_bg(p.fill(Fill::Selection));
    cell.set_fg(fg);
}

/// Paint the live drag's highlight, else the settled one a copy left.
fn render_text_selection(frame: &mut Frame, app: &App, area: Rect) {
    use crate::selection::Surface;
    let (drag, is_live) = match app.text_drag() {
        Some(d) => (d, true),
        None => match app.settled_selection() {
            Some(d) => (d, false),
            None => return,
        },
    };
    // An unmoved press is a pending click; a settled span always paints.
    if is_live && drag.anchor == drag.extent {
        return;
    }
    let p = app.palette();
    let (lo, hi) = drag.ordered();
    match drag.surface {
        Surface::Files => {
            let inner = inner_rect(panes(area, app).files);
            for y in inner.y..inner.y + inner.height {
                let i = (y - inner.y) as usize + app.file_scroll;
                if i >= lo.row && i <= hi.row && i < app.file_rows.len() {
                    for x in inner.x..inner.x + inner.width {
                        if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                            paint_selected(cell, p);
                        }
                    }
                }
            }
        }
        Surface::Read => {
            let pane = read_pane(area, app);
            let slots = app.painted_slots();
            let code_width = (pane.inner.width as usize).saturating_sub(pane.prefix_w).max(1);
            for (off, slot) in slots.iter().enumerate() {
                let Slot::Code { row: li, seg } = *slot else { continue };
                if li < lo.row || li > hi.row {
                    continue;
                }
                let row_ref = &app.visible[li];
                if !row_ref.is_content() {
                    continue;
                }
                let cells = plain_cells(row_ref);
                let (seg_s, seg_e) = seg_cell_range(app, row_ref, &cells, seg, code_width);
                let y = pane.inner.y + off as u16;
                let max_x = (pane.inner.x + pane.inner.width) as usize;
                let mut x = pane.inner.x as usize + pane.prefix_w;
                for painted in &cells[seg_s..seg_e] {
                    let sel = (li > lo.row || painted.src >= lo.chr)
                        && (li < hi.row || painted.src <= hi.chr);
                    if sel {
                        for dx in 0..painted.w {
                            // A straddling wide char never tints the border.
                            if x + dx >= max_x {
                                break;
                            }
                            if let Some(cell) = frame.buffer_mut().cell_mut(((x + dx) as u16, y)) {
                                paint_selected(cell, p);
                            }
                        }
                    }
                    x += painted.w;
                    if x >= max_x {
                        break;
                    }
                }
            }
        }
        Surface::Painted => {
            let Some(sel) = painted_sel(app, area) else { return };
            for off in 0..sel.rect.height as usize {
                let line = sel.scroll + off;
                if line < lo.row || line > hi.row || line >= sel.texts.len() {
                    continue;
                }
                let from = if line == lo.row { lo.chr } else { 0 };
                let to = if line == hi.row { Some(hi.chr) } else { None };
                paint_text_span(
                    frame,
                    sel.rect.x as usize + sel.offsets[line],
                    sel.rect.y + off as u16,
                    (sel.rect.x + sel.rect.width) as usize,
                    &sel.texts[line],
                    from,
                    to,
                    p,
                );
            }
        }
        Surface::Card { comment } => {
            let pane = read_pane(area, app);
            let slots = app.painted_slots();
            let Some(c) = app.store.get(comment) else { return };
            let texts = card_body_lines(c, pane.inner.width as usize);
            for (off, slot) in slots.iter().enumerate() {
                let Slot::Card { comment: ci, line } = *slot else { continue };
                if ci != comment || line == 0 {
                    continue; // the borders are chrome, never selected
                }
                let body = line - 1;
                if body >= texts.len() || body < lo.row || body > hi.row {
                    continue;
                }
                let from = if body == lo.row { lo.chr } else { 0 };
                let to = if body == hi.row { Some(hi.chr) } else { None };
                paint_text_span(
                    frame,
                    pane.inner.x as usize + CARD_TEXT_X,
                    pane.inner.y + off as u16,
                    (pane.inner.x + pane.inner.width) as usize,
                    &texts[body],
                    from,
                    to,
                    p,
                );
            }
        }
        Surface::PrNav => {
            let inner = inner_rect(panes(area, app).files);
            let scroll = app.pr_nav_scroll();
            for off in 0..inner.height as usize {
                let i = scroll + off;
                if i >= lo.row && i <= hi.row {
                    let y = inner.y + off as u16;
                    for x in inner.x..inner.x + inner.width {
                        if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                            paint_selected(cell, p);
                        }
                    }
                }
            }
        }
    }
}

/// Style the cells of `text`'s chars `from..=to`, clipped at `max_x`.
#[allow(clippy::too_many_arguments)]
fn paint_text_span(
    frame: &mut Frame,
    x0: usize,
    y: u16,
    max_x: usize,
    text: &str,
    from: usize,
    to: Option<usize>,
    p: &Palette,
) {
    let mut x = x0;
    for (i, ch) in text.chars().enumerate() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        let sel = i >= from && to.is_none_or(|t| i <= t);
        if sel {
            for dx in 0..w {
                if x + dx >= max_x {
                    break;
                }
                if let Some(cell) = frame.buffer_mut().cell_mut(((x + dx) as u16, y)) {
                    paint_selected(cell, p);
                }
            }
        }
        x += w;
        if x >= max_x {
            break;
        }
    }
}

/// A `Line`'s plain text, spans joined.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The char index at display column `col` of `text`, past-the-end clamping to the last char.
fn char_at_col(text: &str, col: usize) -> usize {
    let mut acc = 0usize;
    let mut last = 0usize;
    for (i, ch) in text.chars().enumerate() {
        last = i;
        acc += UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc > col {
            return i;
        }
    }
    last
}

/// `text` without its first `cols` display columns.
fn skip_display_cols(text: &str, cols: usize) -> String {
    let mut acc = 0usize;
    text.chars()
        .skip_while(|ch| {
            if acc >= cols {
                return false;
            }
            acc += UnicodeWidthChar::width(*ch).unwrap_or(0);
            true
        })
        .collect()
}

/// A spliced comment card's left indent, in columns (`comment_card_lines`).
const CARD_INDENT: usize = 2;

/// Where a card's body text starts, from the pane's left edge: the indent plus `│ `.
const CARD_TEXT_X: usize = CARD_INDENT + 2;

/// A comment card's wrapped body lines, without its box glyphs.
pub(crate) fn card_body_lines(c: &Comment, width: usize) -> Vec<String> {
    let box_w = width.saturating_sub(CARD_INDENT).max(10);
    let text_w = box_w.saturating_sub(4).max(1); // inside "│ " … " │"
    c.text.split('\n').flat_map(|l| wrap_text(l, text_w)).collect()
}

/// The card selection point under `(col, row)`.
#[must_use]
pub fn card_point_at(
    area: Rect,
    app: &App,
    col: u16,
    row: u16,
) -> Option<(usize, crate::selection::Point)> {
    let pane = read_pane(area, app);
    let (comment, line) = card_slot_at(&pane, app, col, row)?;
    let point = card_point(app, &pane, comment, line, col)?;
    Some((comment, point))
}

/// `card_point_at` for a drag's moving end, clamped to its card.
#[must_use]
pub fn card_point_clamped(
    area: Rect,
    app: &App,
    comment: usize,
    col: u16,
    row: u16,
) -> Option<crate::selection::Point> {
    let pane = read_pane(area, app);
    if pane.inner.height == 0 {
        return None;
    }
    let slots = app.painted_slots();
    let mine = |s: &Slot| matches!(s, Slot::Card { comment: c, .. } if *c == comment);
    let first = slots.iter().position(mine)?;
    let last = slots.iter().rposition(mine)?;
    let at = ((row.max(pane.inner.y) - pane.inner.y) as usize).clamp(first, last);
    let Slot::Card { line, .. } = slots[at] else { return None };
    card_point(app, &pane, comment, line, col)
}

/// The (body line, char) under a card cell; borders snap to the nearest body line.
fn card_point(
    app: &App,
    pane: &ReadPane,
    comment: usize,
    line: usize,
    col: u16,
) -> Option<crate::selection::Point> {
    let texts = card_body_lines(app.store.get(comment)?, pane.inner.width as usize);
    if texts.is_empty() {
        return None;
    }
    let body = line.saturating_sub(1).min(texts.len() - 1);
    let text_col = (col as usize).saturating_sub(pane.inner.x as usize + CARD_TEXT_X);
    Some(crate::selection::Point { row: body, chr: char_at_col(&texts[body], text_col) })
}

/// A painted line's selectable text: no trailing pad, and a `─` rule is nothing.
fn painted_text(text: &str) -> String {
    let t = text.trim_end();
    if !t.is_empty() && t.chars().all(|c| c == '─') { String::new() } else { t.to_string() }
}

/// A painted surface's selectable geometry.
pub(crate) struct PaintedSel {
    pub rect: Rect,
    pub scroll: usize,
    pub texts: Vec<String>,
    pub offsets: Vec<usize>,
}

/// The open painted surface: the `PR` read pane, on the `PR` tab only.
pub(crate) fn painted_sel(app: &App, area: Rect) -> Option<PaintedSel> {
    let inner = inner_rect(panes(area, app).diff);
    if app.tab == Tab::Pr {
        let content = pr_read_content(app, inner);
        let notice_h = content.notice.len() as u16;
        let rect = Rect::new(
            inner.x,
            inner.y.saturating_add(notice_h),
            inner.width,
            inner.height.saturating_sub(notice_h),
        );
        let max = content.lines.len().saturating_sub(rect.height as usize);
        let scroll = app.pr_read_scroll.min(max);
        let offsets: Vec<usize> = (0..content.lines.len())
            .map(|i| match &content.snippet {
                Some((r, w)) if r.contains(&i) => *w,
                _ => 0,
            })
            .collect();
        let texts = content
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| painted_text(&skip_display_cols(&line_text(l), offsets[i])))
            .collect();
        return Some(PaintedSel { rect, scroll, texts, offsets });
    }
    None
}

/// The painted-surface point under `(col, row)`; `clamp` for a drag's moving end.
#[must_use]
pub fn painted_point(
    area: Rect,
    app: &App,
    col: u16,
    row: u16,
    clamp: bool,
) -> Option<crate::selection::Point> {
    let sel = painted_sel(app, area)?;
    if sel.texts.is_empty() || sel.rect.height == 0 || sel.rect.width == 0 {
        return None;
    }
    let (col, row) = if clamp {
        (
            col.clamp(sel.rect.x, sel.rect.x + sel.rect.width - 1),
            row.clamp(sel.rect.y, sel.rect.y + sel.rect.height - 1),
        )
    } else {
        if !contains(sel.rect, col, row) {
            return None;
        }
        (col, row)
    };
    // Only a moving end clamps from blank space onto the last line.
    let line = sel.scroll + (row - sel.rect.y) as usize;
    let line = if clamp {
        line.min(sel.texts.len() - 1)
    } else if line < sel.texts.len() {
        line
    } else {
        return None;
    };
    let text_col = (col as usize).saturating_sub(sel.rect.x as usize + sel.offsets[line]);
    Some(crate::selection::Point { row: line, chr: char_at_col(&sel.texts[line], text_col) })
}

/// The painted surface's line texts, for extraction at a drag's release.
pub(crate) fn painted_texts(app: &App, area: Rect) -> Vec<String> {
    painted_sel(app, area).map(|s| s.texts).unwrap_or_default()
}

/// The PR navigator's row texts, unelided, for copying.
pub(crate) fn pr_nav_texts(app: &App) -> Vec<String> {
    pr_nav_rows(app, usize::MAX, std::time::SystemTime::now())
        .iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim().to_string())
        .collect()
}

/// The PR navigator display row under `(col, row)`; `clamp` snaps into the pane.
#[must_use]
pub fn pr_nav_display_row(area: Rect, app: &App, col: u16, row: u16, clamp: bool) -> Option<usize> {
    let inner = inner_rect(panes(area, app).files);
    if inner.height == 0 {
        return None;
    }
    let n = pr_nav_texts(app).len();
    if n == 0 {
        return None;
    }
    let row = if clamp {
        row.clamp(inner.y, inner.y + inner.height - 1)
    } else {
        if !contains(inner, col, row) {
            return None;
        }
        row
    };
    // Only a moving end clamps from blank space onto the last row.
    let i = (row - inner.y) as usize + app.pr_nav_scroll();
    if clamp {
        Some(i.min(n - 1))
    } else if i < n {
        Some(i)
    } else {
        None
    }
}

/// The comment being edited, whose card hides behind its edit box.
fn editing_comment(app: &App) -> Option<usize> {
    match app.mode {
        Mode::Composing { editing } => editing,
        _ => None,
    }
}

/// The comment box's rows at `width`: wrapped body plus borders.
#[must_use]
pub fn composer_height(app: &App, width: usize) -> usize {
    box_rows(&app.input, composer_content_width(width)).len() + 2
}

/// The text width inside the comment box: the diff pane width minus its two borders.
#[must_use]
pub fn composer_content_width(width: usize) -> usize {
    width.saturating_sub(2).max(1)
}

/// The rendered markdown's wrap width: the read pane's code column.
#[must_use]
pub fn rendered_width(area: Rect, app: &App) -> usize {
    let inner = inner_rect(panes(area, app).diff).width as usize;
    inner.saturating_sub(gutter_prefix_width(gutter_for(&app.diff)))
}

/// The read pane's inner width, without a `Frame`.
#[must_use]
pub fn diff_inner_width(area: Rect, app: &App) -> usize {
    inner_rect(panes(area, app).diff).width as usize
}

/// The comment box's lines, the caret a block over its character, else a placeholder.
fn composer_lines(
    app: &App,
    content_w: usize,
    rows: &[(usize, String)],
    (caret_row, caret_col): (usize, usize),
) -> Vec<Line<'static>> {
    let p = app.palette();
    if app.input.is_empty() {
        return vec![Line::from(input_line("", 0, content_w, "Leave a comment…", p).0)];
    }
    rows.iter()
        .enumerate()
        .map(|(i, (_, text))| {
            if i == caret_row {
                row_with_caret(text, caret_col, p)
            } else {
                Line::from(text.clone())
            }
        })
        .collect()
}

/// The block-cursor style: the character under the caret on a solid accent block.
fn caret_style(p: &Palette) -> Style {
    Style::default().fg(p.ink(Ink::Text, Fill::Caret)).bg(p.fill(Fill::Caret))
}

/// One box row with the caret block over the character at `col`.
fn row_with_caret(text: &str, col: usize, p: &Palette) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let col = col.min(chars.len());
    let left: String = chars[..col].iter().collect();
    let mut spans = vec![Span::raw(left)];
    if col < chars.len() {
        spans.push(Span::styled(chars[col].to_string(), caret_style(p)));
        spans.push(Span::raw(chars[col + 1..].iter().collect::<String>()));
    }
    Line::from(spans)
}

/// The box's wrapped rows as `(start char, text)`.
fn box_rows(input: &str, width: usize) -> Vec<(usize, String)> {
    let chars: Vec<char> = input.chars().collect();
    let mut rows = Vec::new();
    let mut i = 0;
    loop {
        let line_end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
        let cells: Vec<Cell> = chars[i..line_end].iter().copied().map(plain_cell).collect();
        let segments = wrap_segments(&cells, width, ContinuationSpaces::Keep);
        for &(a, b) in &segments {
            rows.push((i + a, chars[i + a..i + b].iter().collect::<String>()));
        }
        // Input exactly filling its last row gets an empty row for the caret.
        if line_end == chars.len()
            && let Some(&(a, b)) = segments.last()
            && cells[a..b].iter().map(|c| c.w).sum::<usize>() == width
        {
            rows.push((line_end, String::new()));
        }
        match chars[line_end..].first() {
            Some('\n') => {
                i = line_end + 1;
                if i == chars.len() {
                    rows.push((i, String::new())); // a trailing newline opens an empty row
                    break;
                }
            }
            _ => break,
        }
    }
    if rows.is_empty() {
        rows.push((0, String::new()));
    }
    rows
}

/// A caret's `(row, col)` in the box rows.
fn caret_rowcol(rows: &[(usize, String)], caret: usize) -> (usize, usize) {
    let row = rows.iter().rposition(|(start, _)| *start <= caret).unwrap_or(0);
    let (start, text) = &rows[row];
    (row, (caret - start).min(text.chars().count()))
}

/// A caret's terminal cell; past an exactly-full row it sits on the next row's first cell.
fn composer_caret_cell_position(
    rows: &[(usize, String)],
    (row, char_col): (usize, usize),
    content_w: usize,
) -> (usize, usize) {
    let cell_col: usize = rows[row].1.chars().take(char_col).map(|c| plain_cell(c).w).sum();
    if cell_col < content_w {
        (row, cell_col)
    } else if row + 1 < rows.len() {
        (row + 1, 0)
    } else {
        (row, content_w.saturating_sub(1))
    }
}

/// A single-line input's visible tail and caret columns, the caret's cells always in view.
fn single_line_caret_view(input: &str, caret: usize, width: usize) -> (String, usize, usize) {
    let chars: Vec<char> = input.chars().collect();
    let caret = caret.min(chars.len());
    let caret_w = chars.get(caret).map_or(1, |&c| plain_cell(c).w.max(1));
    let mut start = caret;
    let mut caret_cell_col = 0;
    let before_limit = width.saturating_sub(caret_w);
    while start > 0 {
        let cell_w = plain_cell(chars[start - 1]).w;
        if caret_cell_col + cell_w > before_limit {
            break;
        }
        caret_cell_col += cell_w;
        start -= 1;
    }

    let mut end = start;
    let mut visible_w = 0;
    while end < chars.len() {
        let cell_w = plain_cell(chars[end]).w;
        if visible_w + cell_w > width {
            break;
        }
        visible_w += cell_w;
        end += 1;
    }
    (chars[start..end].iter().collect(), caret - start, caret_cell_col)
}

/// Put the terminal cursor on the caret, for IME, when it lies inside `area`.
fn anchor_input_cursor(frame: &mut Frame, area: Rect, cell_x: usize, cell_y: usize) {
    let x = area.x.saturating_add(u16::try_from(cell_x).unwrap_or(u16::MAX));
    let y = area.y.saturating_add(u16::try_from(cell_y).unwrap_or(u16::MAX));
    if area.contains(Position::new(x, y)) {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// A single-line input's spans and its caret's cell column, scrolled to keep the caret in view.
fn input_line(
    text: &str,
    caret: usize,
    width: usize,
    placeholder: &str,
    p: &Palette,
) -> (Vec<Span<'static>>, usize) {
    if text.is_empty() {
        let dim = Style::default().fg(p.ink(Ink::TextMuted, Fill::Base));
        return (vec![Span::raw(" "), Span::styled(placeholder.to_string(), dim)], 0);
    }
    // The floor keeps a squeezed input showing its caret's character instead of nothing.
    let (visible, caret_char_col, caret_cell_col) =
        single_line_caret_view(text, caret, width.max(1));
    (row_with_caret(&visible, caret_char_col, p).spans, caret_cell_col)
}

/// The caret after moving one wrapped row up or down, keeping its column where it can.
#[must_use]
pub fn caret_vertical(input: &str, caret: usize, content_w: usize, down: bool) -> usize {
    let rows = box_rows(input, content_w);
    let (mut row, mut col) = caret_rowcol(&rows, caret);
    // Step from the row the cursor shows.
    if composer_caret_cell_position(&rows, (row, col), content_w).0 > row {
        row += 1;
        col = 0;
    }
    let target = if down { (row + 1).min(rows.len() - 1) } else { row.saturating_sub(1) };
    let (start, text) = &rows[target];
    start + col.min(text.chars().count())
}

/// Word-wrap a plain string by the diff's own [`wrap_segments`] rule.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let cells: Vec<Cell> = s.chars().map(plain_cell).collect();
    wrap_segments(&cells, width, ContinuationSpaces::Trim)
        .into_iter()
        .map(|(a, b)| cells[a..b].iter().map(|c| c.ch).collect())
        .collect()
}

/// A clickable region in the header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderHit {
    Tab(Tab),
    Scope,
    /// The `branch` scope's base label; the click opens the base picker.
    Base,
    /// The `commits` scope's pick name; the click opens the commit picker.
    Pick,
}

/// The header control a click lands on, by the painted frame's `keymap`.
#[must_use]
pub fn hit_header(area: Rect, app: &App, keymap: &Keymap, col: u16, row: u16) -> Option<HeaderHit> {
    if row != area.y {
        return None;
    }
    let spans = tab_spans(keymap, app.pr_forge);
    for &(tab, start, end) in &spans {
        if (start as u16..end as u16).contains(&col) {
            return Some(HeaderHit::Tab(tab));
        }
    }
    let prefix = header_prefix_len(&spans);
    let scope_start = prefix as u16;
    let scope_end = scope_start + scope_chip(app).len() as u16;
    if (scope_start..scope_end).contains(&col) {
        return Some(HeaderHit::Scope);
    }
    if let Some((lead, name, tail)) = base_parts(app, keymap, area.width) {
        let base_start = scope_end + BASE_GAP.len() as u16;
        let base_end = base_start + (lead.width() + name.width() + tail.width()) as u16;
        if (base_start..base_end).contains(&col) {
            return Some(if app.scope == crate::model::Scope::Commits {
                HeaderHit::Pick
            } else {
                HeaderHit::Base
            });
        }
    }
    None
}

/// The three tab labels, each led by its hint key; the third is `PR` or `MR`.
fn tab_labels(keymap: &Keymap, forge: crate::git::Forge) -> [(Tab, String); 3] {
    use crate::keymap::Action as K;
    [
        (Tab::Changes, format!("{} Changes", keymap.hint(K::TabChanges).label())),
        (Tab::AllFiles, format!("{} Files", keymap.hint(K::TabAllFiles).label())),
        (Tab::Pr, format!("{} {}", keymap.hint(K::TabPr).label(), forge.abbr())),
    ]
}
const HEADER_LEAD: &str = " ";
const TAB_GAP: &str = "  ";
const HEADER_GAP: &str = "  ";
/// The gap between the scope chip and the base label.
const BASE_GAP: &str = " ";
/// The tab strip's always-reserved indicator cell, so nothing shifts when it lights.
const INDICATOR_CELL: usize = 2;

/// The indicator cell: the refresh glyph, or blank.
fn indicator_glyph(app: &App) -> &'static str {
    if app.refresh_indicator { "⟳" } else { " " }
}

/// Each tab's header columns, for paint and hit test alike.
fn tab_spans(keymap: &Keymap, forge: crate::git::Forge) -> Vec<(Tab, usize, usize)> {
    let mut col = HEADER_LEAD.len();
    let mut out = Vec::new();
    for (i, (tab, label)) in tab_labels(keymap, forge).iter().enumerate() {
        if i > 0 {
            col += TAB_GAP.len();
        }
        out.push((*tab, col, col + label.width()));
        col += label.width();
    }
    out
}

/// The column where the scope chip starts.
fn header_prefix_len(spans: &[(Tab, usize, usize)]) -> usize {
    spans.last().map_or(HEADER_LEAD.len(), |&(_, _, end)| end) + INDICATOR_CELL + HEADER_GAP.len()
}

fn scope_chip(app: &App) -> String {
    format!("[{}]", app.scope.label())
}

/// The base label as `(lead, shown, marker, tail)`; `marker` is ` (sha)` for a named rev.
fn base_label(app: &App) -> Option<(String, String, String, String)> {
    if app.scope == crate::model::Scope::Commits {
        return pick_label(app);
    }
    if app.scope != crate::model::Scope::Branch {
        return None;
    }
    let tail = match &app.branch_base.skipped {
        Some(missing) => format!(" · {missing} missing"),
        None => String::new(),
    };
    Some(match &app.branch_base.winner {
        Some(git::ResolvedBase::Branch { name, .. }) => {
            ("vs ".to_string(), name.clone(), String::new(), tail)
        }
        Some(git::ResolvedBase::Rev { spelling, oid }) => {
            let (shown, mark) = git::rev_paint(spelling, oid);
            ("vs ".to_string(), shown, mark.map(|m| format!(" ({m})")).unwrap_or_default(), tail)
        }
        None => (String::new(), "no base".to_string(), String::new(), tail),
    })
}

/// The pick label: `1a2b3c4 <subject>` or `896626a..a49ed7b (N)`, plus any verdict.
fn pick_label(app: &App) -> Option<(String, String, String, String)> {
    use crate::world::PickVerdict;
    let pick = app.commit_pick.as_ref()?;
    let status = app.pick_status.as_ref();
    // Only a `commits` build carries a current verdict.
    let verdict = status.map(|s| &s.verdict).filter(|_| app.scope == crate::model::Scope::Commits);
    let gone = app.pick_gone();
    let tail = match verdict {
        Some(PickVerdict::OffBranch) => " · off branch".to_string(),
        Some(PickVerdict::Gone(_)) => " · gone".to_string(),
        Some(PickVerdict::Live) | None => String::new(),
    };
    let shown = if pick.is_single() {
        git::abbreviate_oid(&pick.newest)
    } else {
        format!("{}..{}", git::abbreviate_oid(&pick.oldest), git::abbreviate_oid(&pick.newest))
    };
    let marker = match status {
        Some(s) if pick.is_single() && !s.subject.is_empty() => format!(" {}", s.subject),
        _ if pick.is_single() || gone => String::new(),
        _ => format!(" ({})", status.map_or(0, |s| s.count)),
    };
    Some((String::new(), shown, marker, tail))
}

/// The base label truncated to fit, the name before the skipped tail and `(sha)` kept.
fn base_parts(app: &App, keymap: &Keymap, width: u16) -> Option<(String, String, String)> {
    let (lead, shown, marker, tail) = base_label(app)?;
    // Everything else on the line plus the base's own gap and the suffix's minimum gap.
    let fixed = header_prefix_len(&tab_spans(keymap, app.pr_forge))
        + scope_chip(app).len()
        + BASE_GAP.len()
        + lead.width()
        + header_suffix(app).width()
        + HEADER_LEAD.len()
        + 1;
    let budget = (width as usize).saturating_sub(fixed);
    let marker_w = marker.width();
    let name = if app.scope == crate::model::Scope::Commits {
        // The subject clips first; the sha and the verdict stay whole.
        let tail_w = tail.width();
        if budget > shown.width() + tail_w {
            format!("{shown}{}", truncate_width(&marker, budget - shown.width() - tail_w))
        } else {
            truncate_width(&shown, budget.saturating_sub(tail_w))
        }
    } else if marker_w > 0 && budget > marker_w {
        format!("{}{marker}", truncate_width(&shown, budget - marker_w))
    } else {
        truncate_width(&format!("{shown}{marker}"), budget)
    };
    if name.is_empty() {
        // No room for a name: no nameless `vs`.
        return None;
    }
    let tail = truncate_width(&tail, budget.saturating_sub(name.width()));
    Some((lead, name, tail))
}

/// The header suffix: the changed-file count and line totals, measured by display width.
fn header_suffix(app: &App) -> String {
    let (added, removed) = app.changed_totals();
    let stats = stats_str(added, removed);
    let gap = if stats.is_empty() { "" } else { "  " };
    format!("{} changed{gap}{stats}", app.changed_count())
}

/// The header's left side both tab bars share: the tabs, the active one underlined.
fn tab_bar_spans(app: &App) -> Vec<Span<'static>> {
    let p = app.palette();
    let bar = Style::default().bg(p.fill(Fill::Bar));
    let mut spans = vec![Span::styled(HEADER_LEAD, bar)];
    for (i, (tab, label)) in tab_labels(app.keymap(), app.pr_forge).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(TAB_GAP, bar));
        }
        let style = if tab == app.tab {
            bar.fg(p.ink(Ink::Accent, Fill::Bar))
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            bar.fg(p.ink(Ink::TextSecondary, Fill::Bar))
        };
        spans.push(Span::styled(label, style));
    }
    // The reserved indicator cell: blank when idle, so nothing shifts.
    spans.push(Span::styled(" ", bar));
    // Quiet like the header's secondary text — status, not an alert.
    spans.push(Span::styled(indicator_glyph(app), bar.fg(p.ink(Ink::TextMuted, Fill::Bar))));
    spans.push(Span::styled(HEADER_GAP, bar));
    spans
}

fn render_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let chip = scope_chip(app);
    let base = base_parts(app, app.keymap(), area.width);
    let base_width = base.as_ref().map_or(0, |(lead, name, tail)| {
        BASE_GAP.len() + lead.width() + name.width() + tail.width()
    });
    let suffix = header_suffix(app);
    let prefix = header_prefix_len(&tab_spans(app.keymap(), app.pr_forge));
    // The suffix keeps the same edge pad as the tab strip's lead.
    let used = prefix + chip.len() + base_width + suffix.width() + HEADER_LEAD.len();
    // Right-align the suffix; at least one gap column when the bar overflows.
    let pad = (area.width as usize).saturating_sub(used).max(1);

    // The clickable scope control is accented to read as a button.
    let p = app.palette();
    let bar = Style::default().bg(p.fill(Fill::Bar));
    let mut spans = tab_bar_spans(app);
    spans.push(Span::styled(
        chip,
        bar.fg(p.ink(Ink::Accent, Fill::Bar)).add_modifier(Modifier::BOLD),
    ));
    if let Some((lead, name, tail)) = base {
        // An empty lead warns `no base`, except in `commits`, whose lead is always empty.
        let warn = lead.is_empty() && app.scope != crate::model::Scope::Commits;
        spans.push(Span::styled(BASE_GAP, bar));
        spans.push(Span::styled(lead, bar.fg(p.ink(Ink::TextMuted, Fill::Bar))));
        spans.push(Span::styled(
            name,
            bar.fg(if warn {
                p.ink(Ink::Warning, Fill::Bar)
            } else {
                p.ink(Ink::Accent, Fill::Bar)
            }),
        ));
        if !tail.is_empty() {
            spans.push(Span::styled(tail, bar.fg(p.ink(Ink::Warning, Fill::Bar))));
        }
    }
    spans.push(Span::styled(" ".repeat(pad), bar));
    // `header_suffix` in colored parts.
    let (added, removed) = app.changed_totals();
    spans.push(Span::styled(
        format!("{} changed", app.changed_count()),
        bar.fg(p.ink(Ink::TextMuted, Fill::Bar)),
    ));
    let stats = stats_spans(added, removed, p, Fill::Bar);
    if !stats.is_empty() {
        spans.push(Span::styled("  ", bar));
        spans.extend(
            stats.into_iter().map(|s| Span::styled(s.content, s.style.bg(p.fill(Fill::Bar)))),
        );
    }
    spans.push(Span::styled(HEADER_LEAD, bar));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The mark on a collapsed `All files` folder holding a change; a folder mixes kinds.
const DIR_DOT: &str = "•";
/// The columns every `All files` folder keeps for the dot, so names elide alike.
const DIR_DOT_RESERVE: usize = 2;

fn render_file_list(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let block = bordered("Files", app.focus == Focus::Files, p);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.file_rows.is_empty() {
        let gone = app.commits_gone_message();
        let msg = match app.tab {
            Tab::AllFiles => "no files",
            Tab::Changes if app.awaiting_turn() => app.turn_wait_message(),
            Tab::Changes if app.commits_gone() => gone.as_str(),
            _ => "no changes",
        };
        frame.render_widget(dim_paragraph(msg, p), inner);
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
            let on = match (i == app.file_cursor, app.focus == Focus::Files) {
                (false, _) => Fill::Base,
                (true, true) => Fill::Cursor,
                (true, false) => Fill::CursorInactive,
            };
            let nest = "  ".repeat(row.depth);
            match &row.kind {
                RowKind::Dir { expanded, has_change, .. } => {
                    let arrow = if *expanded { "▾ " } else { "▸ " };
                    // A git-ignored directory recedes into a dim, unbolded row.
                    let name_style = if row.ignored {
                        Style::default().fg(p.ink(Ink::TextMuted, on))
                    } else {
                        Style::default()
                            .fg(p.ink(Ink::TextSecondary, on))
                            .add_modifier(Modifier::BOLD)
                    };
                    // Elide the bare name, then add `/`: eliding `name/` would leave `…/`.
                    let reserve = if app.tab == Tab::AllFiles { DIR_DOT_RESERVE } else { 0 };
                    let lead = format!("{nest}{arrow}");
                    let budget = width.saturating_sub(lead.width() + reserve + 1).max(1);
                    let name = format!("{}/", elide_head(&row.name, budget));
                    let mut spans = vec![
                        Span::styled(lead, Style::default().fg(p.ink(Ink::TextMuted, on))),
                        Span::styled(name, name_style),
                    ];
                    // Only collapsed `All files` folders need the dot.
                    if app.tab == Tab::AllFiles && !expanded && *has_change {
                        let used: usize = spans.iter().map(Span::width).sum();
                        spans.push(Span::raw(" ".repeat(width.saturating_sub(used + 1))));
                        // Modified's hue, the neutral one.
                        let hue = kind_color(p, ChangeKind::Modified, on);
                        spans.push(Span::styled(DIR_DOT, Style::default().fg(hue)));
                    }
                    selectable_row(p, spans, width, on)
                }
                RowKind::File { index } => {
                    let annotation = app.entries[*index].annotation.as_ref();
                    // No marker: two spaces align the name with sibling folders.
                    let indent = if annotation.is_some() { nest } else { format!("{nest}  ") };
                    file_row_item(
                        &FileRowSpec {
                            indent: &indent,
                            annotation,
                            name: &row.name,
                            ignored: row.ignored,
                            emphasis: &[],
                        },
                        width,
                        on,
                        p,
                    )
                }
            }
        })
        .collect();
    frame.render_widget(List::new(items), inner);
}

/// The fields [`file_row_item`] renders; `emphasis` is byte ranges of search matches.
struct FileRowSpec<'a> {
    indent: &'a str,
    annotation: Option<&'a ChangedFile>,
    name: &'a str,
    ignored: bool,
    emphasis: &'a [(u32, u32)],
}

/// A file row: `<indent><marker> <name> <stats>`, a long name eliding its head.
fn file_row_item(row: &FileRowSpec<'_>, width: usize, on: Fill, p: &Palette) -> ListItem<'static> {
    let FileRowSpec { indent, annotation, name, ignored, emphasis } = *row;
    let marker = annotation.map_or(String::new(), |a| format!("{} ", a.kind.marker()));
    let (additions, deletions) = annotation.map_or((0, 0), |a| (a.additions, a.deletions));
    let stats = stats_str(additions, deletions);
    let gap = if stats.is_empty() { 0 } else { 2 };
    let fixed = indent.width() + marker.width() + stats.width() + gap;
    let shown = elide_head(name, width.saturating_sub(fixed).max(1));

    let mut spans = vec![Span::styled(indent.to_string(), text_style(p, on))];
    if let Some(a) = annotation {
        spans.push(Span::styled(marker, Style::default().fg(kind_color(p, a.kind, on))));
    }
    // An ignored file dims its name, never its change marker.
    let muted = Style::default().fg(p.ink(Ink::TextMuted, on));
    let base_style = if ignored { muted } else { text_style(p, on) };
    let shown_spans = remap_emphasis(emphasis, name, &shown);
    if shown_spans.is_empty() {
        // Dim the parent directories, keep the basename bright.
        let (dim, base) = match shown.rfind('/') {
            Some(s) => (&shown[..=s], &shown[s + 1..]),
            None => ("", shown.as_str()),
        };
        if !dim.is_empty() {
            spans.push(Span::styled(dim.to_string(), muted));
        }
        spans.push(Span::styled(base.to_string(), base_style));
    } else {
        // The same, under the match highlight.
        let basename_at = shown.rfind('/').map_or(0, |i| i + 1);
        spans.extend(emphasized_spans(&shown, &shown_spans, search_hl(p), |byte| {
            if byte < basename_at { muted } else { base_style }
        }));
    }
    if !stats.is_empty() {
        let used: usize = spans.iter().map(Span::width).sum();
        let pad = width.saturating_sub(used + stats.width());
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(stats_spans(additions, deletions, p, on));
    }
    selectable_row(p, spans, width, on)
}

/// The `+a −d` stats text, a zero side dropped.
fn stats_str(additions: u32, deletions: u32) -> String {
    match (additions, deletions) {
        (0, 0) => String::new(),
        (a, 0) => format!("+{a}"),
        (0, d) => format!("−{d}"),
        (a, d) => format!("+{a} −{d}"),
    }
}

/// [`stats_str`] in the diff's green and red.
fn stats_spans(additions: u32, deletions: u32, p: &Palette, on: Fill) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if additions > 0 {
        spans.push(Span::styled(
            format!("+{additions}"),
            Style::default().fg(p.ink(Ink::Added, on)),
        ));
    }
    if additions > 0 && deletions > 0 {
        spans.push(Span::raw(" "));
    }
    if deletions > 0 {
        spans.push(Span::styled(
            format!("−{deletions}"),
            Style::default().fg(p.ink(Ink::Removed, on)),
        ));
    }
    spans
}

/// Remap match spans onto the head-elided `shown`; one wholly in the head is lost.
fn remap_emphasis(spans: &[(u32, u32)], name: &str, shown: &str) -> Vec<(u32, u32)> {
    if spans.is_empty() {
        return Vec::new();
    }
    // `shown` is `name` itself, or `…` plus a suffix of it.
    let Some(tail) = shown.strip_prefix('…') else { return spans.to_vec() };
    let prefix = '…'.len_utf8() as u32;
    let tail_start = (name.len() - tail.len()) as u32;
    spans
        .iter()
        .filter(|&&(_, e)| e > tail_start)
        .map(|&(s, e)| (prefix + s.saturating_sub(tail_start), prefix + (e - tail_start)))
        .collect()
}

/// Elide `name`'s head to `max` columns, cutting at a `/` where it can.
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

/// A saved comment's inline card: a box titled with its location, holding its text.
fn comment_card_lines(c: &Comment, width: usize, p: &Palette) -> Vec<Line<'static>> {
    const INDENT: usize = CARD_INDENT;
    let box_w = width.saturating_sub(INDENT).max(10);
    let text_w = box_w.saturating_sub(4).max(1); // inside "│ " … " │"
    let border = Style::default().fg(p.mark(Ink::Border, Fill::Base));
    let title = Style::default().fg(p.ink(Ink::Comment, Fill::Base)).add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(p.ink(Ink::Text, Fill::Base));
    let pad = || Span::raw(" ".repeat(INDENT));

    let label = truncate_width(&format!(" comment · {} ", c.location()), box_w.saturating_sub(3));
    let fill = box_w.saturating_sub(3 + label.width());
    let mut lines = vec![Line::from(vec![
        pad(),
        Span::styled("╭─", border),
        Span::styled(label, title),
        Span::styled(format!("{}╮", "─".repeat(fill)), border),
    ])];

    // The selection model's own wrap, so paint and copy agree.
    for piece in card_body_lines(c, width) {
        let gap = " ".repeat(text_w.saturating_sub(piece.width()));
        lines.push(Line::from(vec![
            pad(),
            Span::styled("│ ", border),
            Span::styled(piece, body_style),
            Span::styled(format!("{gap} │"), border),
        ]));
    }

    lines.push(Line::from(vec![
        pad(),
        Span::styled(format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))), border),
    ]));
    lines
}

/// Truncate `s` to `max` columns with a trailing `…`; zero fits nothing.
fn truncate_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
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

/// The visible stand-in for a line-ending CR: its caret notation, as `less` and vim show it.
pub const CR_MARKER: &str = "^M";

fn render_diff_view(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let mut title = match (&app.diff_path, &app.diff.previous_path) {
        (Some(new), Some(old)) => format!("{old} → {new}"),
        (Some(new), None) => new.clone(),
        (None, _) => match app.tab {
            Tab::AllFiles => "File",
            _ => "Diff",
        }
        .to_string(),
    };
    if app.rendered_active() {
        title.push_str(" · rendered");
    }
    let block = bordered(&title, app.focus == Focus::Diff, p);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.visible.is_empty() {
        // `All files` is no diff, so its copy avoids diff words.
        let gone = app.commits_gone_message();
        let msg = match app.tab {
            Tab::AllFiles => match app.diff.notice {
                Some(notice) => notice.message(),
                None if app.diff_path.is_some() => "empty file",
                None => "select a file to read",
            },
            Tab::Changes if app.awaiting_turn() => app.turn_wait_message(),
            Tab::Changes if app.commits_gone() => gone.as_str(),
            _ => app.diff.notice.map_or("no diff", Notice::message),
        };
        frame.render_widget(dim_paragraph(msg, p), inner);
        return;
    }

    let height = inner.height as usize;
    if height == 0 {
        return;
    }
    let width = inner.width as usize;

    let gutter_w = gutter_for(&app.diff);
    let expand_hint = app.keymap().hint(crate::keymap::Action::Expand).label();
    let see = app.keymap().hint(crate::keymap::Action::Rendered).label();
    let layout = RowLayout {
        gutter_w,
        width,
        h_scroll: app.h_scroll,
        wrap: app.wrap,
        focused: app.focus == Focus::Diff,
        pal: p,
        find: app.find_query().map(|q| (q, crate::app::find_case_sensitive(q))),
        expand_hint: &expand_hint,
        rendered: app.rendered_lines(),
        see: &see,
    };
    // One comment→row walk feeds both the marks and the card splice.
    let (cards, commented) = app.comment_marks();
    let (lo, hi) = app.selection_range();
    let selecting = app.focus == Focus::Diff && app.select_anchor.is_some();

    // Painted and recorded from one walk, so hit tests match the screen.
    let slots = read_layout(app, inner, &cards);
    app.note_painted_slots(slots.clone());
    note_rendered_regions(app, &slots, inner, gutter_prefix_width(gutter_w));

    // The hovered row; a modal hides the gutter `+`.
    let hovered_row = app.hover.filter(|_| !app.mode.is_modal()).and_then(|(c, r)| {
        if !contains(inner, c, r) {
            return None;
        }
        match slots.get((r - inner.y) as usize) {
            Some(&Slot::Code { row, .. }) if app.visible[row].is_content() => Some(row),
            _ => None,
        }
    });

    // A slot's line, caching the current row's or card's lines across its run.
    let mut row_cache: Option<(usize, Vec<Line>)> = None;
    let mut card_cache: Option<(usize, Vec<Line>)> = None;
    let mut line_for = |slot: &Slot| -> Line<'static> {
        match *slot {
            Slot::Code { row, seg } => {
                if row_cache.as_ref().is_none_or(|(r, _)| *r != row) {
                    let state = RowState {
                        commented: commented.contains(&row),
                        cursor: row == app.diff_cursor,
                        selected: selecting && row >= lo && row <= hi,
                        hovered: hovered_row == Some(row),
                        lead: app.is_rendered_lead(row),
                    };
                    row_cache = Some((row, render_row(&app.visible[row], layout, state)));
                }
                row_cache.as_ref().and_then(|(_, l)| l.get(seg).cloned()).unwrap_or_default()
            }
            Slot::Card { comment, line } => {
                if card_cache.as_ref().is_none_or(|(c, _)| *c != comment) {
                    let lines = app
                        .store
                        .get(comment)
                        .map(|c| comment_card_lines(c, width, p))
                        .unwrap_or_default();
                    card_cache = Some((comment, lines));
                }
                card_cache.as_ref().and_then(|(_, l)| l.get(line).cloned()).unwrap_or_default()
            }
            Slot::Composer => Line::default(),
        }
    };

    let composer_from = slots.iter().position(|s| matches!(s, Slot::Composer));
    if let Some(from) = composer_from {
        // Composing: paint above, the box, and below.
        let box_h = slots[from..].iter().take_while(|s| matches!(s, Slot::Composer)).count();
        let above: Vec<Line> = slots[..from].iter().map(&mut line_for).collect();
        let below: Vec<Line> = slots[from + box_h..].iter().map(&mut line_for).collect();
        let bands = Layout::vertical([
            Constraint::Length(above.len() as u16),
            Constraint::Length(box_h as u16),
            Constraint::Length(below.len() as u16),
        ])
        .split(inner);
        if !above.is_empty() {
            frame.render_widget(Paragraph::new(above), bands[0]);
        }
        render_composer(frame, app, bands[1]);
        if !below.is_empty() {
            frame.render_widget(Paragraph::new(below), bands[2]);
        }
        return;
    }

    // The find band takes the bottom row.
    let finding = app.mode == Mode::Find;
    let body_h = if finding { height.saturating_sub(1) } else { height };
    let out: Vec<Line> = slots.iter().map(&mut line_for).collect();
    frame.render_widget(Paragraph::new(out), Rect { height: body_h as u16, ..inner });
    if finding {
        let band = Rect { y: inner.y + body_h as u16, height: 1, ..inner };
        render_find_band(frame, app, band);
    }
}

/// The line-number column width for a diff of `rows` lines.
fn gutter_width(rows: usize) -> usize {
    rows.to_string().len().max(3)
}

/// The gutter width for a whole `FileDiff`, so a fold toggle never resizes it.
fn gutter_for(diff: &FileDiff) -> usize {
    let total_lines: usize =
        diff.rows.iter().map(|r| if r.is_content() { 1 } else { r.hidden() }).sum();
    gutter_width(total_lines)
}

/// The gutter prefix width: the change bar plus the right-aligned line number and a space.
fn gutter_prefix_width(gutter_w: usize) -> usize {
    1 + gutter_w + 1
}

/// A row's display height: its wrap segments, or 1.
fn row_height(row: &Row, gutter_w: usize, width: usize, wrap: bool) -> usize {
    // A rendered row is one line the renderer already wrapped to the code column.
    if !wrap || matches!(row, Row::Fold { .. } | Row::Rendered { .. }) {
        return 1;
    }
    let code_width = width.saturating_sub(gutter_prefix_width(gutter_w)).max(1);
    // The find highlight never changes wrapping, so height ignores it.
    wrap_segments(&plain_cells(row), code_width, ContinuationSpaces::Trim).len()
}

/// The diff-pane layout: constant for a frame.
#[derive(Clone, Copy)]
struct RowLayout<'a> {
    gutter_w: usize,
    width: usize,
    h_scroll: usize,
    wrap: bool,
    /// Whether the diff pane is focused — dims the cursor row when it is not.
    focused: bool,
    /// The active palette for the change bars, row tints, and fills.
    pal: &'a Palette,
    /// The open find query and its smart-case flag.
    find: Option<(&'a str, bool)>,
    /// The `expand` hint the cursor's fold row advertises, following a rebind.
    expand_hint: &'a str,
    /// The styled lines a rendered block's line paints, indexed by its `line`.
    rendered: &'a [Line<'static>],
    /// The `rendered` key's label a don't-render marker names, following a rebind.
    see: &'a str,
}

/// A row's per-row highlight state.
#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
struct RowState {
    commented: bool,
    cursor: bool,
    selected: bool,
    /// Whether the pointer hovers this row, showing the gutter `+`.
    hovered: bool,
    /// Whether a rendered row leads its block and so shows its line number.
    lead: bool,
}

/// A diff row's display lines: bar, number, tinted code, wrapped or h-scrolled.
fn render_row(row: &Row, layout: RowLayout<'_>, state: RowState) -> Vec<Line<'static>> {
    let RowLayout {
        gutter_w,
        width,
        h_scroll,
        wrap,
        focused,
        pal,
        find,
        expand_hint,
        rendered,
        see,
    } = layout;
    let RowState { commented, cursor, selected, hovered, lead } = state;
    // A commented line's number wears your comment color; others are muted.
    let num_ink = if commented { Ink::Comment } else { Ink::TextMuted };
    let highlight = match_style(pal);
    if let Row::Rendered { src, kind, .. } = row {
        let on = row_fill(cursor, selected, focused, Fill::Base);
        let (num_color, plus) = (pal.ink(num_ink, on), pal.ink(Ink::Comment, on));
        // Only the lead line is numbered; the bar cell shows the change mark.
        let num = if lead { src.to_string() } else { String::new() };
        let (bar, bar_color) = match kind {
            RenderedKind::Block { bar: None, .. } => (" ", pal.mark(Ink::Border, on)),
            RenderedKind::Block { bar: Some(b), .. } => ("▌", pal.mark(bar_ink(*b), on)),
            RenderedKind::Marker { kind, .. } => ("▌", pal.mark(marker_ink(*kind), on)),
        };
        let mut spans = gutter_spans(bar, bar_color, &num, num_color, hovered, gutter_w, plus);
        let code_width = width.saturating_sub(gutter_prefix_width(gutter_w));
        match kind {
            RenderedKind::Block { line, hides, bar, .. } => {
                let body: Vec<Span<'static>> = rendered
                    .get(*line as usize)
                    .map(|l| l.spans.clone())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|mut sp| {
                        if let Some(fg) = sp.style.fg {
                            // Syntax and markdown colors keep their legibility on the row's fill.
                            sp.style = sp.style.fg(pal.legible(fg, on));
                        }
                        // The cursor and selection stack above a code chip.
                        if on != Fill::Base {
                            sp.style.bg = None;
                        }
                        sp
                    })
                    .collect();
                let hits = find
                    .map(|(q, cs)| crate::app::find_match_ranges(&row.text(), q, cs))
                    .unwrap_or_default();
                let body = light_ranges(body, &hits, highlight);
                let used: usize = body.iter().map(Span::width).sum();
                spans.extend(body);
                // A collapsed summary names the changed lines its body hides.
                if let (Some(n), Some(b)) = (hides, bar) {
                    let note = format!("  · {n} changed {}", plural(*n, "line"));
                    let note = truncate_width(&note, code_width.saturating_sub(used));
                    spans.push(Span::styled(note, Style::default().fg(pal.ink(bar_ink(*b), on))));
                }
            }
            RenderedKind::Marker { kind, lines, .. } => {
                let text = truncate_width(&marker_text(*kind, *lines, see), code_width);
                spans.push(Span::styled(text, Style::default().fg(pal.ink(marker_ink(*kind), on))));
            }
        }
        let mut out = Line::from(spans);
        if let Some(pad) = width.checked_sub(out.width()).filter(|p| *p > 0) {
            out.push_span(Span::raw(" ".repeat(pad)));
        }
        return vec![fill(out, pal.bg(on))];
    }
    if let Row::Fold { .. } = row {
        let label = if cursor {
            format!("  ⋯  {} unmodified lines · {expand_hint} to expand", row.hidden())
        } else {
            format!("  ⋯  {} unmodified lines", row.hidden())
        };
        let on = row_fill(cursor, false, focused, Fill::Bar);
        let mut line =
            Line::from(Span::styled(label, Style::default().fg(pal.ink(Ink::TextSecondary, on))));
        if let Some(pad) = width.checked_sub(line.width()).filter(|p| *p > 0) {
            line.push_span(Span::raw(" ".repeat(pad)));
        }
        return vec![line.style(Style::default().bg(pal.fill(on)).add_modifier(Modifier::BOLD))];
    }
    // `0` is an unnumbered PR snippet row; file diffs are 1-based.
    let num = row
        .new_no()
        .or_else(|| row.old_no())
        .filter(|&n| n > 0)
        .map_or(String::new(), |n| n.to_string());
    let (bar, bar_ink, tint) = match row.marker() {
        '-' => ("▌", Ink::Removed, Fill::Removed),
        '+' => ("▌", Ink::Added, Fill::Added),
        _ => (" ", Ink::Border, Fill::Base),
    };
    let on = row_fill(cursor, selected, focused, tint);
    let (bar_color, num_color) = (pal.mark(bar_ink, on), pal.ink(num_ink, on));
    let plus = pal.ink(Ink::Comment, on);
    let row_bg = pal.bg(on);

    // A cursor or selection fill wins over word emphasis.
    let emph_on = !cursor && !selected;
    let emph = match row.marker() {
        '-' => Fill::RemovedEmph,
        _ => Fill::AddedEmph,
    };
    let emph_bg = pal.fill(emph);
    let hl_ranges =
        find.map(|(q, cs)| crate::app::find_match_ranges(&row.text(), q, cs)).unwrap_or_default();
    let mut cells = code_cells(row, emph_on, &hl_ranges, pal.ink(Ink::Text, on));
    // Dim syntax colors move back to their plain legibility on the row's fill or emphasis.
    for cell in cells.iter_mut().filter(|c| !c.hl) {
        cell.fg = pal.legible(cell.fg, if cell.emph { emph } else { on });
    }

    let prefix_w = gutter_prefix_width(gutter_w);
    let code_width = width.saturating_sub(prefix_w).max(1);
    let chunks: Vec<&[Cell]> = if wrap {
        wrap_segments(&cells, code_width, ContinuationSpaces::Trim)
            .into_iter()
            .map(|(s, e)| &cells[s..e])
            .collect()
    } else {
        vec![cells.get(skip_columns(&cells, h_scroll)..).unwrap_or(&[])]
    };

    chunks
        .into_iter()
        .enumerate()
        .map(|(k, chunk)| {
            let gutter = if k == 0 {
                gutter_spans(bar, bar_color, &num, num_color, hovered, gutter_w, plus)
            } else {
                // A continuation row keeps the change bar but blanks the number column.
                vec![
                    Span::styled(bar, Style::default().fg(bar_color)),
                    Span::raw(" ".repeat(prefix_w - 1)),
                ]
            };
            let mut spans = gutter;
            spans.extend(cells_to_spans(chunk, emph_bg, highlight));
            let mut line = Line::from(spans);
            if let Some(pad) = width.checked_sub(line.width()).filter(|p| *p > 0) {
                line.push_span(Span::raw(" ".repeat(pad)));
            }
            fill(line, row_bg)
        })
        .collect()
}

/// A row's gutter: bar and number, or under the pointer a `[+]` in your comment color.
fn gutter_spans(
    bar: &'static str,
    bar_color: Color,
    num: &str,
    num_color: Color,
    hovered: bool,
    gutter_w: usize,
    plus: Color,
) -> Vec<Span<'static>> {
    let bar = Span::styled(bar, Style::default().fg(bar_color));
    if hovered {
        vec![
            bar,
            Span::raw(" ".repeat(gutter_w - 3)),
            Span::styled("[+]", Style::default().fg(plus).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]
    } else {
        vec![bar, Span::styled(format!("{num:>gutter_w$} "), Style::default().fg(num_color))]
    }
}

/// A rendered block's change bar, by what changed.
fn bar_ink(bar: crate::diff::Bar) -> Ink {
    match bar {
        crate::diff::Bar::Added => Ink::Added,
        crate::diff::Bar::Modified => Ink::Modified,
    }
}

/// A rendered marker row's color: removed, or a change that renders nothing.
fn marker_ink(kind: crate::diff::MarkerKind) -> Ink {
    match kind {
        crate::diff::MarkerKind::Removed => Ink::Removed,
        crate::diff::MarkerKind::Unrendered => Ink::Modified,
    }
}

/// The fill a list row sits on: the cursor's on the cursor row, else the background.
fn cursor_fill(on_cursor: bool) -> Fill {
    if on_cursor { Fill::Cursor } else { Fill::Base }
}

/// A row's fill: cursor, else selection, else its own `tint`.
fn row_fill(cursor: bool, selected: bool, focused: bool, tint: Fill) -> Fill {
    match (cursor, selected, focused) {
        (true, _, true) => Fill::Cursor,
        (true, _, false) => Fill::CursorInactive,
        (false, true, _) => Fill::Selection,
        _ => tint,
    }
}

/// `line` under the fill `bg`, when there is one.
fn fill(line: Line<'static>, bg: Option<Color>) -> Line<'static> {
    match bg {
        Some(bg) => line.style(Style::default().bg(bg)),
        None => line,
    }
}

/// A marker's words; `see` names the key that flips to source.
#[must_use]
pub fn marker_text(kind: MarkerKind, lines: u32, see: &str) -> String {
    match kind {
        MarkerKind::Removed => format!("− {lines} {} removed", plural(lines, "line")),
        MarkerKind::Unrendered if lines == 1 => {
            format!("⚠ 1 changed line doesn't render · {see} to see")
        }
        MarkerKind::Unrendered => format!("⚠ {lines} changed lines don't render · {see} to see"),
    }
}

/// `n` `word`s, plural past one.
fn plural(n: u32, word: &str) -> String {
    if n == 1 { word.to_string() } else { format!("{word}s") }
}

/// `spans` with the chars in `ranges` restyled by `hl`.
fn light_ranges(
    spans: Vec<Span<'static>>,
    ranges: &[crate::diff::CharRange],
    hl: Style,
) -> Vec<Span<'static>> {
    if ranges.is_empty() {
        return spans;
    }
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let byte_at: Vec<usize> = text.char_indices().map(|(b, _)| b).chain([text.len()]).collect();
    let at = |c: u32| byte_at.get(c as usize).copied().unwrap_or(text.len()) as u32;
    let bytes: Vec<(u32, u32)> = ranges.iter().map(|&(a, b)| (at(a), at(b))).collect();
    let ends: Vec<(usize, Style)> = spans
        .iter()
        .scan(0, |end, s| {
            *end += s.content.len();
            Some((*end, s.style))
        })
        .collect();
    let mut k = 0;
    emphasized_spans(
        &text,
        &bytes,
        |style| style.patch(hl),
        |byte| {
            while ends.get(k).is_some_and(|&(end, _)| end <= byte) {
                k += 1;
            }
            ends.get(k).map_or_else(Style::default, |&(_, style)| style)
        },
    )
}

pub(crate) fn rgb(c: crate::diff::Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Tabs expand to this many columns.
const TAB: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContinuationSpaces {
    Keep,
    Trim,
}

fn plain_cell(ch: char) -> Cell {
    Cell {
        ch,
        w: UnicodeWidthChar::width(ch).unwrap_or(0),
        fg: Color::Reset,
        emph: false,
        hl: false,
        src: 0,
    }
}

/// Greedy word wrap into one cell range per row, hard-breaking an over-wide word.
fn wrap_segments(
    cells: &[Cell],
    width: usize,
    continuation_spaces: ContinuationSpaces,
) -> Vec<(usize, usize)> {
    if cells.is_empty() {
        return vec![(0, 0)];
    }
    let mut segs = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        // At least one cell, so an over-wide glyph never stalls.
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
        if continuation_spaces == ContinuationSpaces::Trim {
            while start < cells.len() && cells[start].ch == ' ' {
                start += 1;
            }
        }
    }
    segs
}

/// The first cell at or past `cols` columns, never splitting a wide glyph.
fn skip_columns(cells: &[Cell], cols: usize) -> usize {
    let mut col = 0;
    let mut i = 0;
    while i < cells.len() && col < cols {
        col += cells[i].w;
        i += 1;
    }
    i
}

/// One display cell of a code line.
struct Cell {
    ch: char,
    w: usize,
    fg: Color,
    emph: bool,
    hl: bool,
    /// The source char this cell paints; a tab's cells share one.
    src: usize,
}

/// A row's display cells: tab-expanded, width-measured, emphasis and find flags, plus any CR marker.
fn code_cells(row: &Row, emph_on: bool, hl_ranges: &[(u32, u32)], marker_fg: Color) -> Vec<Cell> {
    let emphasis = if emph_on { row.emphasis() } else { &[] };
    let in_emph = |i: u32| emphasis.iter().any(|&(a, b)| i >= a && i < b);
    let in_hl = |i: u32| hl_ranges.iter().any(|&(a, b)| i >= a && i < b);
    let mut cells = Vec::new();
    let mut idx = 0u32;
    let mut col = 0usize; // display column, so tab stops land right after wide glyphs too
    // A rendered row's text carries no color of its own: its paint styles it from the render.
    let runs: Vec<(Color, &str)> = match row {
        Row::Rendered { text, .. } => vec![(Color::Reset, text.as_str())],
        _ => row.spans().iter().map(|s| (rgb(s.color), s.text.as_str())).collect(),
    };
    for (fg, text) in runs {
        for ch in text.chars() {
            let emph = in_emph(idx);
            let hl = in_hl(idx);
            let src = idx as usize;
            if ch == '\t' {
                for _ in 0..(TAB - col % TAB) {
                    cells.push(Cell { ch: ' ', w: 1, fg, emph, hl, src });
                    col += 1;
                }
            } else {
                let w = UnicodeWidthChar::width(ch).unwrap_or(0);
                cells.push(Cell { ch, w, fg, emph, hl, src });
                col += w;
            }
            idx += 1;
        }
    }
    if row.cr_marker() {
        // The marker stands at the text's last char, so past the end still selects that char.
        let src = (idx as usize).saturating_sub(1);
        cells.extend(CR_MARKER.chars().map(|ch| Cell {
            ch,
            w: 1,
            fg: marker_fg,
            emph: emph_on,
            hl: false,
            src,
        }));
    }
    cells
}

/// A row's cells for layout: widths and source indices, no paint.
fn plain_cells(row: &Row) -> Vec<Cell> {
    code_cells(row, false, &[], Color::Reset)
}

/// Spans from cells, merging equal runs; a find match takes `hl`, emphasis `emph_bg`.
fn cells_to_spans(cells: &[Cell], emph_bg: Color, hl: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<(Color, bool, bool)> = None;
    for c in cells {
        let key = (c.fg, c.emph, c.hl);
        if cur != Some(key) {
            if let Some((fg, emph, is_hl)) = cur {
                spans.push(cell_span(std::mem::take(&mut buf), fg, emph, is_hl, emph_bg, hl));
            }
            cur = Some(key);
        }
        buf.push(c.ch);
    }
    if let Some((fg, emph, is_hl)) = cur {
        spans.push(cell_span(buf, fg, emph, is_hl, emph_bg, hl));
    }
    spans
}

/// A find or search match: bold text on the solid highlight, legible over any row tint.
fn match_style(p: &Palette) -> Style {
    Style::default()
        .bg(p.fill(Fill::Highlight))
        .fg(p.ink(Ink::Text, Fill::Highlight))
        .add_modifier(Modifier::BOLD)
}

/// A run's span: find match, else word emphasis, else plain.
fn cell_span(
    text: String,
    fg: Color,
    emph: bool,
    is_hl: bool,
    emph_bg: Color,
    hl: Style,
) -> Span<'static> {
    let style = if is_hl {
        hl
    } else if emph {
        Style::default().fg(fg).bg(emph_bg)
    } else {
        Style::default().fg(fg)
    };
    Span::styled(text, style)
}

/// The foot band: label (`find` or `line`), query, and the match or line count.
fn render_find_band(frame: &mut Frame, app: &App, area: Rect) {
    let Some(f) = app.find.as_ref() else { return };
    let p = app.palette();
    let line_field = app.line_open();
    let dim = Style::default().fg(p.ink(Ink::TextMuted, Fill::Base));
    let width = area.width as usize;
    let (label, placeholder) =
        if line_field { ("line ", "Go to line…") } else { ("find ", "Find in file…") };
    let count = if line_field {
        format!("of {}", app.line_count())
    } else {
        match app.find_count() {
            None => String::new(),
            Some((_, 0)) => "no matches".to_string(),
            Some((Some(k), total)) => format!("{k}/{total}"),
            Some((None, total)) => total.to_string(),
        }
    };

    let count_w = count.width();
    // Bounded, so a long query never pushes the count off.
    let query_w = width.saturating_sub(label.width() + count_w + 1).max(1);
    let (query_spans, caret_cell_col) = input_line(&f.query, f.caret, query_w, placeholder, p);

    let mut spans =
        vec![Span::styled(label, Style::default().fg(p.ink(Ink::TextSecondary, Fill::Base)))];
    spans.extend(query_spans);

    let mut line = Line::from(spans);
    if let Some(pad) = width.checked_sub(line.width() + count_w).filter(|pad| *pad > 0) {
        line.push_span(Span::raw(" ".repeat(pad)));
    }
    if !count.is_empty() {
        line.push_span(Span::styled(count, dim));
    }
    frame.render_widget(Paragraph::new(line), area);
    anchor_input_cursor(frame, area, label.width() + caret_cell_col, 0);
}

/// The `PR` read pane and, while a rework note is composed, the composer box under it.
fn pr_compose_split(app: &App, area: Rect) -> (Rect, Option<Rect>) {
    if !matches!(app.mode, Mode::Composing { .. }) {
        return (area, None);
    }
    let rows = box_rows(&app.input, composer_content_width(area.width as usize)).len();
    let h = (rows as u16 + 2).clamp(3, (area.height / 2).max(3)).min(area.height);
    let [read, composer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(h)]).areas(area);
    (read, Some(composer))
}

/// The inline comment input box, drawn at `area` (under the selection in the diff).
fn render_composer(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let loc = app.pending_location().unwrap_or_else(|| "comment".to_string());
    let editing = matches!(app.mode, Mode::Composing { editing: Some(_) });
    let title = if editing { format!("edit · {loc}") } else { format!("comment · {loc}") };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.mark(Ink::Comment, Fill::Base)))
        .title(framed_title(&title));
    let content_w = composer_content_width(area.width as usize);
    let rows = box_rows(&app.input, content_w);
    let inner = inner_rect(area);
    let rowcol = caret_rowcol(&rows, app.caret);
    let (cursor_row, cursor_col) = composer_caret_cell_position(&rows, rowcol, content_w);
    // A box too short for its rows scrolls to keep the caret row visible.
    let scroll = cursor_row.saturating_sub((inner.height as usize).saturating_sub(1));
    let body = Paragraph::new(composer_lines(app, content_w, &rows, rowcol))
        .block(block)
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));
    frame.render_widget(body, area);
    anchor_input_cursor(frame, inner, cursor_col, cursor_row - scroll);
}

/// A timestamp's age against `now`, as [`age_label`] spells it.
#[must_use]
pub fn relative_age(created_at: &str, now: SystemTime) -> String {
    let Some(then) = forge::parse_iso(created_at) else {
        return String::new();
    };
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    age_label((now - then).max(0) as u64)
}

/// A compact age: `30s`, `5m`, `2h`, `3d`, `6w`, `2y`.
pub fn age_label(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 604_800 => format!("{}d", s / 86_400),
        s if s < 365 * 86_400 => format!("{}w", s / 604_800),
        s => format!("{}y", s / (365 * 86_400)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cr_marker_points_at_its_lines_last_char() {
        let span = crate::diff::Span { text: "ab".into(), color: Default::default() };
        let row = Row::Insertion { new_no: 1, spans: vec![span], emphasis: vec![], cr: true };
        let srcs: Vec<usize> = plain_cells(&row).iter().map(|c| c.src).collect();
        assert_eq!(srcs, [0, 1, 1, 1]);
    }

    #[test]
    fn relative_age_buckets_by_magnitude() {
        // now = 2026-06-27T12:00:00Z
        let now = UNIX_EPOCH
            + std::time::Duration::from_secs(
                crate::forge::parse_iso("2026-06-27T12:00:00Z").unwrap() as u64,
            );
        assert_eq!(relative_age("2026-06-27T11:55:00Z", now), "5m");
        assert_eq!(relative_age("2026-06-27T10:00:00Z", now), "2h");
        assert_eq!(relative_age("2026-06-24T12:00:00Z", now), "3d");
        assert_eq!(relative_age("2026-06-13T12:00:00Z", now), "2w");
        assert_eq!(age_label(364 * 86_400), "52w");
        assert_eq!(age_label(365 * 86_400), "1y");
        assert_eq!(relative_age("garbage", now), "");
    }

    use super::{box_rows, caret_rowcol, composer_caret_cell_position, single_line_caret_view};

    /// The production pairing: box rows built at the same width the caret maps against.
    fn caret_cell(input: &str, caret: usize, content_w: usize) -> (usize, usize) {
        let rows = box_rows(input, content_w);
        composer_caret_cell_position(&rows, caret_rowcol(&rows, caret), content_w)
    }

    #[test]
    fn comment_caret_uses_display_cells() {
        assert_eq!(caret_cell("abc", 3, 20), (0, 3));
        assert_eq!(caret_cell("日本", 2, 20), (0, 4));
        assert_eq!(caret_cell("a日本b", 3, 20), (0, 5));
    }

    #[test]
    fn comment_caret_follows_the_existing_wrap_rows() {
        // `a日` fills three cells, so `本b` starts the next row at cell zero.
        assert_eq!(caret_cell("a日本b", 3, 3), (1, 2));
    }

    #[test]
    fn a_full_rows_end_maps_to_the_next_rows_first_cell() {
        // Past a full row the cursor waits on the next row's first cell, never the border.
        assert_eq!(caret_cell("abc", 3, 3), (1, 0));
        assert_eq!(caret_cell("日本", 2, 4), (1, 0));
        assert_eq!(caret_cell("abc\ndef", 3, 3), (1, 0));
        assert_eq!(caret_cell("abc\ndef", 4, 3), (1, 0));
    }

    #[test]
    fn an_over_wide_glyph_clamps_to_the_last_cell() {
        // In a one-cell box the caret clamps rather than leave it.
        assert_eq!(caret_cell("日", 1, 1), (0, 0));
    }

    #[test]
    fn only_input_ending_on_a_full_row_grows_a_continuation_row() {
        assert_eq!(box_rows("abc", 3).len(), 2);
        assert_eq!(box_rows("ab", 3).len(), 1);
        // A full line before a newline adds no phantom blank row between the lines.
        assert_eq!(box_rows("abc\ndef", 3).len(), 3);
        assert_eq!(box_rows("abc\n", 3).len(), 2);
    }

    #[test]
    fn single_line_caret_view_uses_display_cells() {
        assert_eq!(single_line_caret_view("abcdef", 6, 4), ("def".to_string(), 3, 3));
        assert_eq!(single_line_caret_view("a日本b", 3, 4), ("本b".to_string(), 1, 2));
    }

    #[test]
    fn a_scrolled_view_keeps_the_wide_character_under_the_caret() {
        // A wide char under the caret keeps both its cells in view.
        assert_eq!(single_line_caret_view("abcdef日x", 6, 4), ("ef日".to_string(), 2, 2));
    }
}

/// A footer action's key and label; an empty label shows the key alone.
fn action_key_label(app: &App, action: FooterAction) -> (String, String) {
    use crate::keymap::Action as K;
    use FooterAction as A;
    // A rebindable action's hint is its first bound key.
    let hint = |action: K| app.keymap().hint(action).label();
    let (k, l): (String, &str) = match action {
        A::Comment => (hint(K::Comment), "comment"),
        A::Rework => (hint(K::Comment), "rework"),
        // One word for one gesture: `v` marks a range end in the diff and the commit picker alike.
        A::Select | A::CommitAnchor => (hint(K::Select), "select"),
        A::ClearSelection => ("esc".into(), "clear"),
        A::EditComment => (hint(K::Edit), "edit"),
        A::EditFile => (hint(K::Edit), "edit file"),
        A::DeleteComment => (hint(K::Delete), "delete"),
        A::JumpComment => (format!("{}/{}", hint(K::NextComment), hint(K::PrevComment)), "jump"),
        A::ExpandFold => (hint(K::Expand), "expand fold"),
        A::CrossFile { forward: true } => (hint(K::NextHunk), "next file"),
        A::CrossFile { forward: false } => (hint(K::PrevHunk), "prev file"),
        // The `move` band's pairs render as their two keys.
        A::MoveLine => (format!("{} {}", hint(K::Down), hint(K::Up)), ""),
        A::MoveHunk => (format!("{} {}", hint(K::NextHunk), hint(K::PrevHunk)), "hunk"),
        A::MoveFile => (format!("{} {}", hint(K::NextFile), hint(K::PrevFile)), "file"),
        A::MovePage => (format!("{} {}", hint(K::PageUp), hint(K::PageDown)), ""),
        A::ExpandDir => (hint(K::Expand), "expand"),
        A::CollapseDir => (hint(K::Collapse), "collapse"),
        A::TogglePane => {
            return ("tab".into(), if app.focus == Focus::Files { "diff" } else { "files" }.into());
        }
        A::Rendered => {
            (hint(K::Rendered), if app.rendered_active() { "source" } else { "rendered" })
        }
        A::NavigatorPosition => (hint(K::NavigatorPosition), "layout"),
        A::NavigatorHide => {
            (hint(K::NavigatorHide), if app.navigator_hidden_here() { "show" } else { "hide" })
        }
        A::Scope => (
            format!(
                "{}/{}/{}/{}",
                hint(K::ScopeUncommitted),
                hint(K::ScopeBranch),
                hint(K::ScopeLastTurn),
                hint(K::ScopeCommits)
            ),
            "scope",
        ),
        A::Send => return (hint(K::Send), format!("send {}", app.store.len())),
        A::List => (hint(K::Comments), "comments"),
        A::Copy => (hint(K::Copy), "copy"),
        A::QuitDiscard => {
            // Short, so `esc cancel` fits a 40-column row.
            return (hint(K::QuitDiscard), format!("quit ({} pending)", app.unsent()));
        }
        A::Save => ("enter".into(), "save"),
        A::Newline => ("shift+enter".into(), "newline"),
        A::Cancel | A::ClosePicker => ("esc".into(), "cancel"),
        A::CloseList | A::CloseSearch | A::CloseFind => ("esc".into(), "close"),
        A::PickAgent => ("enter".into(), "send"),
        // The digits are literal; the movement keys are bound.
        A::MovePickerRow => (format!("1-9 {} {}", hint(K::Down), hint(K::Up)), "move"),
        A::BasePick => (hint(K::BasePick), "base"),
        A::CommitPick => (hint(K::CommitPick), "commits"),
        A::PickCommitRun => {
            let n = app.commit_picker.as_ref().map_or(0, crate::app::CommitPicker::run_len);
            return ("enter".into(), if n > 1 { format!("open {n}") } else { "open".into() });
        }
        A::MoveCommitRow => (format!("{} {}", hint(K::Down), hint(K::Up)), "move"),
        A::CloseCommitPicker => {
            let anchored = app.commit_picker.as_ref().is_some_and(|cp| cp.anchor.is_some());
            ("esc".into(), if anchored { "clear" } else { "cancel" })
        }
        A::ScopeOther => {
            use crate::model::Scope;
            let others: Vec<String> = [
                (Scope::Uncommitted, K::ScopeUncommitted),
                (Scope::Branch, K::ScopeBranch),
                (Scope::LastTurn, K::ScopeLastTurn),
                (Scope::Commits, K::ScopeCommits),
            ]
            .into_iter()
            .filter(|(scope, _)| app.scope != *scope)
            .map(|(_, action)| hint(action))
            .collect();
            (others.join("/"), "scope")
        }
        A::Search => (hint(K::Search), "search"),
        A::Find => (hint(K::Find), "find"),
        A::GotoLine => (hint(K::GotoLine), "line"),
        A::LineGo => ("enter".into(), "go"),
        A::Wrap => (hint(K::Wrap), if app.wrap { "unwrap" } else { "wrap" }),
        // Arrows, since every printable is query text here.
        A::FindStep | A::MoveBaseRow | A::PickResult => ("↑↓".into(), "move"),
        A::FlipSearchMode => {
            // The label names the destination mode: `code` from Files, `files` from Code.
            let to_code =
                app.search.as_ref().is_none_or(|s| s.search_mode == crate::app::SearchMode::Files);
            return ("tab".into(), if to_code { "code" } else { "files" }.into());
        }
        // `enter` opens the highlight in every list: a search result, a base, a commit run.
        A::OpenResult | A::PickBaseRow => ("enter".into(), "open"),
        A::OpenPr => (hint(K::OpenPr), "open ↗"),
        A::Refresh => (hint(K::Refresh), "refresh"),
        A::Tabs => {
            (format!("{}·{}·{}", hint(K::TabChanges), hint(K::TabAllFiles), hint(K::TabPr)), "tabs")
        }
        A::Quit => (hint(K::Quit), "quit"),
    };
    (k, l.into())
}

/// A band's `(key, label)` styles: the primary bright and bold, the rest readable.
fn band_styles(band: Band, p: &Palette) -> (Style, Style) {
    match band {
        Band::Primary => (
            Style::default().fg(p.ink(Ink::Accent, Fill::Bar)).add_modifier(Modifier::BOLD),
            text_style(p, Fill::Bar),
        ),
        Band::Send | Band::Do | Band::Go | Band::Move => (
            Style::default().fg(p.ink(Ink::Accent, Fill::Bar)),
            Style::default().fg(p.ink(Ink::TextSecondary, Fill::Bar)),
        ),
    }
}

/// One action's `key label` spans, without the leading separator.
fn action_entry(app: &App, action: FooterAction, band: Band) -> Vec<Span<'static>> {
    let p = app.palette();
    let (key, label) = action_key_label(app, action);
    let (key_style, label_style) = band_styles(band, p);
    let mut spans = vec![Span::styled(key, key_style)];
    if !label.is_empty() {
        spans.push(Span::styled(format!(" {label}"), label_style));
    }
    spans
}

/// The rendered width of one action entry: its `key label` (a space joins them).
fn entry_body_width(app: &App, action: FooterAction) -> usize {
    let (key, label) = action_key_label(app, action);
    if label.is_empty() {
        key.chars().count()
    } else {
        key.chars().count() + 1 + label.chars().count()
    }
}

/// The rendered width of one action entry, plus its leading ` · ` separator.
fn entry_width(app: &App, action: FooterAction) -> usize {
    SEP.chars().count() + entry_body_width(app, action)
}

/// The ` · ` that joins footer entries, and the dim-label indent of a wrapped `?`-band row.
const SEP: &str = " · ";
const BAND_INDENT: usize = 6;

/// The footer status's frame: the two-space gap, the `·`, and the trailing space around it.
const STATUS_FRAME: usize = 5;
/// The narrowest status row 1 paints; below it the status drops.
const STATUS_MIN: usize = 8;
/// The ` …` a trimmed modal footer ends with, in place of `?`.
const MORE_ELLIPSIS: usize = 2;

/// The footer: row 1, plus the `?` bands when open.
fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let mut lines = footer_lines(app, area.width as usize);
    lines.truncate((area.height as usize).max(1));
    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(p.fill(Fill::Bar))), area);
}

/// The footer's height, capped so the body keeps 3 rows.
fn footer_height(app: &App, area: Rect) -> u16 {
    if !(app.keys_expanded && app.footer_open_ended()) {
        return 1;
    }
    let want = footer_lines(app, area.width as usize).len() as u16;
    let cap = area.height.saturating_sub(1 + 3).max(1); // tab bar + body minimum
    want.clamp(1, cap)
}

/// The footer's lines, for height and paint alike.
fn footer_lines(app: &App, w: usize) -> Vec<Line<'static>> {
    let (row1, overflow) = footer_row1(app, w);
    let mut lines = vec![Line::from(row1)];
    if app.keys_expanded && app.footer_open_ended() {
        let bands = app.footer_bands();
        let of_band = |band: Band| -> Vec<FooterAction> {
            bands.iter().filter(|&&(_, b)| b == band).map(|&(a, _)| a).collect()
        };
        // Row 1 holds the `do` label, so its overflow takes a blank one.
        lines.extend(render_band(app, w, "", Band::Do, &overflow));
        lines.extend(render_band(app, w, "go", Band::Go, &of_band(Band::Go)));
        lines.extend(render_band(app, w, "move", Band::Move, &of_band(Band::Move)));
    }
    lines
}

/// Row 1, and the actions trimmed off it for the `do` band.
fn footer_row1(app: &App, w: usize) -> (Vec<Span<'static>>, Vec<FooterAction>) {
    let p = app.palette();
    let bands = app.footer_bands();
    let primary = bands.iter().find(|&&(_, b)| b == Band::Primary).map(|&(a, _)| a);
    let do_acts: Vec<FooterAction> =
        bands.iter().filter(|&&(_, b)| b == Band::Do).map(|&(a, _)| a).collect();
    let send = bands.iter().find(|&&(_, b)| b == Band::Send).map(|&(a, _)| a);
    let show_more = app.footer_open_ended();
    let reserve = if show_more { 2 } else { 0 }; // a gap plus the `?`

    // `send` and `?` never drop, so their width is reserved first.
    let send_w = send.map_or(0, |a| entry_width(app, a));
    let tail = send_w + reserve;

    // Open, row 1 joins the labeled grid, if the `do` gutter leaves room for what never drops.
    let primary_key_w = primary.map_or(0, |a| action_key_label(app, a).0.chars().count());
    let labeled = app.keys_expanded
        && show_more
        && (primary.is_some() || !do_acts.is_empty())
        && 1 + BAND_INDENT + primary_key_w + tail <= w;
    let (mut spans, mut used): (Vec<Span<'static>>, usize) = if labeled {
        let label = Span::styled(
            format!("{:<BAND_INDENT$}", "do"),
            Style::default().fg(p.ink(Ink::TextMuted, Fill::Bar)),
        );
        (vec![Span::raw(" "), label], 1 + BAND_INDENT)
    } else {
        (vec![Span::raw(" ")], 1)
    };

    // The `PR` tab leads with its state, unless the quit question owns the row.
    let pr_state =
        (app.tab == Tab::Pr && !app.confirming_quit).then(|| app.pr_snapshot()).flatten();
    if let Some(s) = pr_state {
        let primary_w = primary.map_or(0, |a| entry_body_width(app, a));
        let budget = w.saturating_sub(used + primary_w + reserve + 4).max(8);
        let text = truncate_width(&format!("{}   ", pr_state_line(app, s)), budget);
        used += text.chars().count();
        spans.push(Span::styled(text, Style::default().fg(p.ink(Ink::TextSecondary, Fill::Bar))));
    }

    // The primary sheds its label, then truncates its key, but never drops.
    if let Some(a) = primary {
        let (key, label) = action_key_label(app, a);
        let (key_style, label_style) = band_styles(Band::Primary, p);
        let full =
            key.chars().count() + if label.is_empty() { 0 } else { 1 + label.chars().count() };
        if used + full + tail <= w || !show_more {
            spans.push(Span::styled(key, key_style));
            if !label.is_empty() {
                spans.push(Span::styled(format!(" {label}"), label_style));
            }
            used += full;
        } else if used + key.chars().count() + tail <= w {
            used += key.chars().count();
            spans.push(Span::styled(key, key_style));
        } else {
            let room = w.saturating_sub(used + tail);
            let key = truncate_width(&key, room);
            used += key.chars().count();
            spans.push(Span::styled(key, key_style));
        }
    }

    // The status outranks the actions, which `?` repeats; it reserves only the room it can use.
    let status_tail =
        send_w + if do_acts.is_empty() { reserve } else { reserve.max(MORE_ELLIPSIS) };
    let free = w.saturating_sub(used + status_tail);
    let status_w =
        if app.status.is_empty() || app.confirming_quit || free < STATUS_FRAME + STATUS_MIN {
            0
        } else {
            (STATUS_FRAME + app.status.width()).min(free)
        };

    // Pack the actions; the rest spill to the `do` band.
    let mut overflow = Vec::new();
    let mut trimming = false;
    let widths: Vec<usize> = do_acts.iter().map(|&a| entry_width(app, a)).collect();
    for (i, a) in do_acts.into_iter().enumerate() {
        let ew = widths[i];
        // A trimming entry leaves room for the modal's `…`.
        let rest: usize = widths[i + 1..].iter().sum();
        let fits_all = used + ew + rest + send_w + status_w + reserve <= w;
        let ellipsis = if show_more || fits_all { 0 } else { MORE_ELLIPSIS };
        if trimming || used + ew + send_w + status_w + reserve + ellipsis > w {
            trimming = true;
            overflow.push(a);
            continue;
        }
        used += ew;
        spans.push(Span::styled(SEP, Style::default().fg(p.mark(Ink::Border, Fill::Bar))));
        spans.extend(action_entry(app, a, Band::Do));
    }

    // `send` closes the actions and never drops.
    if let Some(a) = send {
        used += send_w;
        spans.push(Span::styled(SEP, Style::default().fg(p.mark(Ink::Border, Fill::Bar))));
        spans.extend(action_entry(app, a, Band::Send));
    }

    // The status, truncated into its reserved room.
    if status_w > 0 {
        let more = if overflow.is_empty() { reserve } else { reserve.max(MORE_ELLIPSIS) };
        let room = w.saturating_sub(used + STATUS_FRAME + more);
        if room >= STATUS_MIN {
            let text = format!("  · {} ", truncate_width(&app.status, room));
            used += text.width();
            spans.push(Span::styled(text, Style::default().fg(p.ink(Ink::Text, Fill::Bar))));
        }
    }

    // `?` at the right, or a modal's `…` when it trimmed.
    if show_more {
        let pad = w.saturating_sub(used + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled("?", Style::default().fg(p.ink(Ink::TextSecondary, Fill::Bar))));
    } else if !overflow.is_empty() {
        spans.push(Span::styled(" …", Style::default().fg(p.ink(Ink::TextMuted, Fill::Bar))));
    }
    (spans, overflow)
}

/// One `?` band: a dim label, then its keys wrapped beneath.
fn render_band(
    app: &App,
    w: usize,
    label: &str,
    band: Band,
    actions: &[FooterAction],
) -> Vec<Line<'static>> {
    if actions.is_empty() {
        return Vec::new();
    }
    let p = app.palette();
    let label_style = Style::default().fg(p.ink(Ink::TextMuted, Fill::Bar));
    let avail = w.saturating_sub(1 + BAND_INDENT);
    let start = |first: bool| -> Vec<Span<'static>> {
        if first {
            vec![Span::raw(" "), Span::styled(format!("{label:<BAND_INDENT$}"), label_style)]
        } else {
            vec![Span::raw(" ".repeat(1 + BAND_INDENT))]
        }
    };

    let mut lines = Vec::new();
    let mut row = start(true);
    let mut row_w = 0usize;
    let mut first_in_row = true;
    for &a in actions {
        let entry = action_entry(app, a, band);
        let ew: usize = entry.iter().map(Span::width).sum();
        if !first_in_row && row_w + SEP.chars().count() + ew > avail {
            lines.push(Line::from(std::mem::replace(&mut row, start(false))));
            row_w = 0;
            first_in_row = true;
        }
        if first_in_row {
            row_w += ew;
            first_in_row = false;
        } else {
            row.push(Span::styled(SEP, label_style));
            row_w += SEP.chars().count() + ew;
        }
        row.extend(entry);
    }
    lines.push(Line::from(row));
    lines
}

/// The comments list's box, a fixed share of the body so it holds still.
const LIST_POPUP_W_PCT: u16 = 80;
const LIST_POPUP_H_PCT: u16 = 70;

fn render_comments_list(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let body = panes(area, app).body;
    let w = body.width * LIST_POPUP_W_PCT / 100;
    let h = body.height * LIST_POPUP_H_PCT / 100;
    let popup = body_popup(area, app, w, h);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.mark(Ink::Accent, Fill::Base)))
        .title(framed_title(&format!("Comments ({})", app.store.len())));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let width = inner.width as usize;
    let items: Vec<ListItem> = app
        .store
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let on = cursor_fill(i == app.list_cursor);
            let loc = Span::styled(
                format!(" {}", c.location()),
                Style::default().fg(p.ink(Ink::Comment, on)).add_modifier(Modifier::BOLD),
            );
            let mut spans = vec![loc, Span::styled(format!("  {}", c.text), text_style(p, on))];
            // A comment whose anchor may have moved is flagged, never dropped.
            if app.is_stale(c) {
                spans.push(Span::styled("  (stale)", Style::default().fg(p.ink(Ink::Warning, on))));
            }
            // The list overlay is the active modal, so its row reads at full brightness.
            selectable_row(p, spans, width, on)
        })
        .collect();
    frame.render_widget(List::new(items), inner);
}

/// A `w` × `h` popup centered in the body, never over the footer.
fn body_popup(area: Rect, app: &App, w: u16, h: u16) -> Rect {
    let body = panes(area, app).body;
    let w = w.min(body.width);
    let h = h.min(body.height);
    Rect {
        x: body.x + body.width.saturating_sub(w) / 2,
        y: body.y + body.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// The narrowest a menu popup gets.
const PICKER_MIN_WIDTH: usize = 34;

/// A menu's box: sized to its rows, plus borders and one column of air.
fn menu_popup(area: Rect, app: &App, widest: usize, title: &str, lines: usize) -> Rect {
    let body = panes(area, app).body;
    let title = framed_title(title).width() + 2;
    let w = (widest + 3).max(title).max(PICKER_MIN_WIDTH).min(body.width as usize) as u16;
    let h = lines.min(body.height as usize) as u16;
    body_popup(area, app, w, h)
}

/// The first visible row, so the highlight stays on screen in a menu taller than the pane
fn menu_scroll(cursor: usize, total: usize, rows: usize) -> usize {
    if rows == 0 || cursor < rows {
        return 0;
    }
    (cursor + 1).saturating_sub(rows).min(total.saturating_sub(rows))
}

/// The menu row under the pointer; `None` off the list.
fn menu_hit(
    inner: Rect,
    top: u16,
    first: usize,
    total: usize,
    col: u16,
    row: u16,
) -> Option<usize> {
    let list_y = inner.y + top;
    if col < inner.x
        || col >= inner.x + inner.width
        || row < list_y
        || row >= inner.y + inner.height
    {
        return None;
    }
    let index = first + (row - list_y) as usize;
    (index < total).then_some(index)
}

fn picker_popup(area: Rect, app: &App) -> Rect {
    let name_width = picker_name_width(app);
    // " N  " + name + "  " + trail; the highlight is a fill.
    let widest = app
        .picker_rows
        .iter()
        .map(|row| 4 + name_width + 2 + picker_trail(app, row).width())
        .max()
        .unwrap_or(0);
    menu_popup(area, app, widest, &picker_title(app), app.picker_rows.len() + 2)
}

/// The popup's row region, from the `Block` the renderer draws.
fn picker_inner(popup: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(popup)
}

/// The names pad to the widest, so the dim tails start in one column.
fn picker_name_width(app: &App) -> usize {
    app.picker_rows.iter().map(|row| row.name.width()).max().unwrap_or(0)
}

/// A row's dim trail: state, tab label, and ` · last used`.
fn picker_trail(app: &App, row: &AgentChoice) -> String {
    let tab = if row.tab.is_empty() { String::new() } else { format!(" · {}", row.tab) };
    let last =
        if app.last_sent_pane.as_deref() == Some(&row.pane_id) { " · last used" } else { "" };
    format!("{}{tab}{last}", row.state)
}

fn picker_title(app: &App) -> String {
    let n = app.store.len();
    let noun = if n == 1 { "comment" } else { "comments" };
    format!("send {n} {noun} to")
}

fn picker_scroll(app: &App, rows: usize) -> usize {
    menu_scroll(app.picker_cursor, app.picker_rows.len(), rows)
}

fn render_agent_picker(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let popup = picker_popup(area, app);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.mark(Ink::Accent, Fill::Base)))
        .title(framed_title(&picker_title(app)));
    let inner = picker_inner(popup);
    frame.render_widget(block, popup);

    let name_width = picker_name_width(app);
    let first = picker_scroll(app, inner.height as usize);
    let items: Vec<ListItem> = app
        .picker_rows
        .iter()
        .enumerate()
        .skip(first)
        .take(inner.height as usize)
        .map(|(i, row)| {
            // Only the first nine rows carry a number, since no digit key reaches further.
            let on = cursor_fill(i == app.picker_cursor);
            let lead = if i < 9 { format!(" {}  ", i + 1) } else { "    ".to_string() };
            let pad = name_width.saturating_sub(row.name.width());
            let spans = vec![
                Span::styled(lead, Style::default().fg(p.ink(Ink::TextMuted, on))),
                // Only the name is bright: it is what the reviewer scans for.
                Span::styled(row.name.clone(), text_style(p, on)),
                Span::styled(
                    format!("{}  {}", " ".repeat(pad), picker_trail(app, row)),
                    Style::default().fg(p.ink(Ink::TextMuted, on)),
                ),
            ];
            selectable_row(p, spans, inner.width as usize, on)
        })
        .collect();
    frame.render_widget(List::new(items), inner);
}

/// The picker row under the pointer, for click-to-highlight.
pub fn hit_picker_row(area: Rect, app: &App, col: u16, row: u16) -> Option<usize> {
    let inner = picker_inner(picker_popup(area, app));
    let first = picker_scroll(app, inner.height as usize);
    menu_hit(inner, 0, first, app.picker_rows.len(), col, row)
}

// --- Base picker ----------------------------------------------

/// The name as painted: a rev's SHA-once abbrev, else the branch name.
fn row_shown(row: &crate::app::BaseChoice) -> String {
    match row {
        crate::app::BaseChoice::Rev { name, oid } => git::rev_paint(name, oid).0,
        crate::app::BaseChoice::Branch { name, .. } => name.clone(),
    }
}

/// A base row's trail words in paint order, or `(sha)` on a named rev.
fn base_trail_words(row: &crate::app::BaseChoice, now: u64) -> Vec<String> {
    if let crate::app::BaseChoice::Rev { name, oid } = row {
        return git::rev_paint(name, oid).1.map(|a| format!("({a})")).into_iter().collect();
    }
    let mut words: Vec<String> = Vec::new();
    if row.pr_base() {
        words.push("pr base".into());
    }
    if row.is_default() {
        words.push("default".into());
    }
    if row.current() {
        words.push("current".into());
    }
    if row.tip_secs() > 0 {
        words.push(age_label(now.saturating_sub(row.tip_secs())));
    }
    words
}

const BASE_ROW_LEAD: &str = " ";
/// Two cells between the name and the trail, so `feat/x  3d` never reads as one token.
const BASE_TRAIL_GAP: usize = 2;

/// A base row's name and trail for `width`: the trail sheds words before the name clips.
fn base_row_parts(row: &crate::app::BaseChoice, width: usize, now: u64) -> (String, String) {
    let name = row_shown(row);
    let mut words = base_trail_words(row, now);
    let avail = width.saturating_sub(BASE_ROW_LEAD.width());
    loop {
        let trail = words.join(" · ");
        let trail_w = if trail.is_empty() { 0 } else { BASE_TRAIL_GAP + trail.width() };
        if name.width() + trail_w <= avail {
            return (name, trail);
        }
        if words.pop().is_none() {
            return (truncate_width(&name, avail), String::new());
        }
    }
}

/// Content width of one base-picker row at full length: lead, name, gap, and trail.
fn base_row_width(row: &crate::app::BaseChoice, now: u64) -> usize {
    let (name, trail) = base_row_parts(row, usize::MAX, now);
    let trail_w = if trail.is_empty() { 0 } else { BASE_TRAIL_GAP + trail.width() };
    BASE_ROW_LEAD.width() + name.width() + trail_w
}

/// The base picker's box, held at its full-list size while filtering, plus a probe hit's row.
fn base_picker_popup(area: Rect, app: &App, now: u64) -> Rect {
    let Some(bp) = &app.base_picker else { return Rect::default() };
    // `visible` decides whether the hit is its own row; the box follows that one decision.
    let visible = bp.visible();
    let added = visible.len() > bp.filtered().len();
    let hit = visible.last().filter(|_| added).copied();
    let widest = bp.rows.iter().chain(hit).map(|r| base_row_width(r, now)).max().unwrap_or(0);
    let lines = bp.rows.len().max(1) + 3 + usize::from(added);
    menu_popup(area, app, widest, &base_picker_title(bp), lines)
}

/// The base picker's title: the branch count, or `matched/total` while filtering.
fn base_picker_title(bp: &crate::app::BasePicker) -> String {
    let is_branch = |r: &crate::app::BaseChoice| matches!(r, crate::app::BaseChoice::Branch { .. });
    let total = bp.rows.iter().filter(|r| is_branch(r)).count();
    if !bp.query.is_empty() {
        let shown = bp.filtered().into_iter().filter(|&i| is_branch(&bp.rows[i])).count();
        return format!("base · {shown}/{total}");
    }
    let noun = if total == 1 { "branch" } else { "branches" };
    format!("base · {total} {noun}")
}

fn base_picker_scroll(bp: &crate::app::BasePicker, rows: usize) -> usize {
    menu_scroll(bp.cursor, bp.visible().len(), rows)
}

fn render_base_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(bp) = &app.base_picker else { return };
    let p = app.palette();
    let now = now_unix();
    let popup = base_picker_popup(area, app, now);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.mark(Ink::Accent, Fill::Base)))
        .title(framed_title(&base_picker_title(bp)));
    let inner = picker_inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }

    // The filter line, scrolled to keep the caret in view.
    let prefix = " ";
    // One cell of right margin keeps the scrolled query off the popup's border.
    let avail = (inner.width as usize).saturating_sub(prefix.width() + 1);
    let (filter_spans, caret_cell_col) =
        input_line(&bp.query, bp.caret, avail, "Filter or type a revision…", p);
    let mut filter = Line::from(filter_spans);
    filter.spans.insert(0, Span::styled(prefix, text_style(p, Fill::Base)));
    frame.render_widget(Paragraph::new(filter), Rect { height: 1, ..inner });
    anchor_input_cursor(frame, inner, prefix.width() + caret_cell_col, 0);

    let list_area = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
    let visible = bp.visible();
    if visible.is_empty() {
        let msg = if bp.query.is_empty() {
            " type a revision"
        } else if matches!(bp.probe, crate::app::BaseProbe::Miss) {
            " no branches match"
        } else {
            return;
        };
        let none =
            Line::from(Span::styled(msg, Style::default().fg(p.ink(Ink::TextMuted, Fill::Base))));
        frame.render_widget(Paragraph::new(none), list_area);
        return;
    }
    let width = list_area.width as usize;
    let first = base_picker_scroll(bp, list_area.height as usize);
    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .skip(first)
        .take(list_area.height as usize)
        .map(|(vi, row)| {
            // A bright name and a right-aligned dim trail.
            let on = cursor_fill(vi == bp.cursor);
            let lead = BASE_ROW_LEAD;
            let (label, trail) = base_row_parts(row, width, now);
            let gap = if trail.is_empty() { 0 } else { BASE_TRAIL_GAP };
            let pad = width.saturating_sub(lead.width() + label.width() + gap + trail.width());
            let mut spans = vec![
                Span::styled(lead.to_string(), text_style(p, on)),
                Span::styled(label, text_style(p, on)),
            ];
            if !trail.is_empty() {
                spans.push(Span::styled(
                    format!("{}{trail}", " ".repeat(pad + gap)),
                    Style::default().fg(p.ink(Ink::TextMuted, on)),
                ));
            }
            selectable_row(p, spans, width, on)
        })
        .collect();
    frame.render_widget(List::new(items), list_area);
}

/// The filtered base-picker row under the pointer, the filter line skipped
pub fn hit_base_picker_row(area: Rect, app: &App, col: u16, row: u16) -> Option<usize> {
    let bp = app.base_picker.as_ref()?;
    let inner = picker_inner(base_picker_popup(area, app, now_unix()));
    let first = base_picker_scroll(bp, inner.height.saturating_sub(1) as usize);
    menu_hit(inner, 1, first, bp.visible().len(), col, row)
}

// --- Commit picker -------------------------------------------

/// The bar column, the sha, two spaces, the subject, two spaces, the age.
const COMMIT_SHA_W: usize = 7;
const COMMIT_AGE_W: usize = 3;

/// One commit row's parts; the trail joins `✎ N`, `merge`, and one ref.
struct CommitRowParts {
    sha: String,
    subject: String,
    trail: String,
    author: String,
    age: String,
}

/// Every visible commit row's parts, built once per paint.
fn commit_row_parts(app: &App, cp: &crate::app::CommitPicker) -> Vec<CommitRowParts> {
    let mut comments: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for c in app.store.iter() {
        if let crate::model::Rev::Commit(pick) = &c.rev {
            *comments.entry(pick.newest.as_str()).or_default() += 1;
        }
    }
    let now = now_unix();
    (0..cp.len())
        .map(|i| {
            if cp.is_pick_row(i) {
                let (_, shown, marker, tail) = pick_label(app).unwrap_or_default();
                return CommitRowParts {
                    sha: shown,
                    subject: format!("{}{tail}", marker.trim_start()),
                    trail: String::new(),
                    author: String::new(),
                    age: String::new(),
                };
            }
            let row = cp.list_row(i).expect("a visible index names a row");
            let mut trail: Vec<String> = Vec::new();
            if let Some(n) = comments.get(row.sha.as_str()) {
                trail.push(format!("✎ {n}"));
            }
            if row.merge {
                trail.push("merge".to_string());
            }
            trail.extend(commit_ref(app, row));
            CommitRowParts {
                sha: git::abbreviate_oid(&row.sha),
                subject: row.subject.clone(),
                trail: trail.join(" · "),
                author: row.author.clone(),
                age: age_label(now.saturating_sub(row.time)),
            }
        })
        .collect()
}

/// The one ref a row shows: `pr`, else a remote tip, else a tag, else a local branch.
fn commit_ref(app: &App, row: &git::CommitRow) -> Option<String> {
    use git::CommitRef as R;
    if app
        .pr_snapshot()
        .is_some_and(|s| s.state == crate::forge::PrState::Open && s.head_oid == row.sha)
    {
        return Some("pr".to_string());
    }
    let rank = |r: &R| match r {
        R::Remote(_) => 0,
        R::Tag(_) => 1,
        R::Branch(_) => 2,
    };
    row.refs.iter().min_by_key(|r| rank(r)).map(R::label)
}

/// The author column's width: the widest author, capped.
fn commit_author_width(parts: &[CommitRowParts]) -> usize {
    const CAP: usize = 20;
    parts.iter().map(|p| p.author.width()).max().unwrap_or(0).min(CAP)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn commit_picker_popup(area: Rect, app: &App, parts: &[CommitRowParts]) -> Rect {
    let Some(cp) = &app.commit_picker else { return Rect::default() };
    let author_w = commit_author_width(parts);
    let widest = parts
        .iter()
        .map(|p| {
            let trail_w = if p.trail.is_empty() { 0 } else { 2 + p.trail.width() };
            // bar + sha + gap + subject + trail + gap + author + gap + age
            2 + p.sha.width().max(COMMIT_SHA_W)
                + 2
                + p.subject.width()
                + trail_w
                + 2
                + author_w
                + 2
                + COMMIT_AGE_W
        })
        .max()
        .unwrap_or(0);
    menu_popup(area, app, widest, &cp.title, cp.len().max(1) + 2)
}

/// The rows the list shows, leaving one for `… N more` when clipped.
fn commit_picker_rows(cp: &crate::app::CommitPicker, height: usize) -> usize {
    if cp.len() > height { height.saturating_sub(1) } else { height }
}

fn commit_picker_scroll(cp: &crate::app::CommitPicker, height: usize) -> usize {
    menu_scroll(cp.cursor, cp.len(), commit_picker_rows(cp, height))
}

fn render_commit_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(cp) = &app.commit_picker else { return };
    let p = app.palette();
    let parts = commit_row_parts(app, cp);
    let popup = commit_picker_popup(area, app, &parts);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.mark(Ink::Accent, Fill::Base)))
        .title(framed_title(&cp.title));
    let inner = picker_inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }
    if cp.is_empty() {
        let msg = format!(" {}", cp.empty);
        frame.render_widget(
            Paragraph::new(Span::styled(
                msg,
                Style::default().fg(p.ink(Ink::TextMuted, Fill::Base)),
            )),
            inner,
        );
        return;
    }
    let width = inner.width as usize;
    let rows = commit_picker_rows(cp, inner.height as usize);
    let first = commit_picker_scroll(cp, inner.height as usize);
    let last = cp.len().min(first + rows);
    let author_w = commit_author_width(&parts);
    let mut items: Vec<ListItem> = (first..last)
        .map(|i| {
            let on = cursor_fill(i == cp.cursor);
            let CommitRowParts { sha, subject, trail, author, age } = &parts[i];
            // The bar marks the run, the way the diff's selection bar marks a line range.
            let bar = if cp.in_run(i) { "▎" } else { " " };
            let fixed = 2 + COMMIT_SHA_W + 2 + 2 + author_w + 2 + COMMIT_AGE_W;
            // The subject clips first; the trail takes only what it leaves.
            let subject = truncate_width(subject, width.saturating_sub(fixed));
            let room = width.saturating_sub(fixed + subject.width());
            let trail = if trail.is_empty() || room < 4 {
                String::new()
            } else {
                format!("  {}", truncate_width(trail, room - 2))
            };
            let author = truncate_width(author, author_w);
            // Padding counts cells, not chars: a wide-glyph author takes its width.
            let pad = width.saturating_sub(fixed + subject.width() + trail.width())
                + 2
                + author_w.saturating_sub(author.width());
            let spans = vec![
                Span::styled(format!("{bar} "), Style::default().fg(p.mark(Ink::Accent, on))),
                Span::styled(
                    format!("{sha:<COMMIT_SHA_W$}  "),
                    Style::default().fg(p.ink(Ink::TextSecondary, on)),
                ),
                Span::styled(subject, text_style(p, on)),
                Span::styled(trail, Style::default().fg(p.ink(Ink::TextMuted, on))),
                Span::styled(
                    format!("{}{author}  {age:>COMMIT_AGE_W$}", " ".repeat(pad)),
                    Style::default().fg(p.ink(Ink::TextMuted, on)),
                ),
            ];
            selectable_row(p, spans, width, on)
        })
        .collect();
    // A clipped list says so, like the search screen's results.
    if last < cp.len() {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  … {} more", cp.len() - last),
            Style::default().fg(p.ink(Ink::TextMuted, Fill::Base)),
        ))));
    }
    frame.render_widget(List::new(items), inner);
}

/// The commit-picker row under the pointer.
pub fn hit_commit_picker_row(area: Rect, app: &App, col: u16, row: u16) -> Option<usize> {
    let cp = app.commit_picker.as_ref()?;
    let inner = picker_inner(commit_picker_popup(area, app, &commit_row_parts(app, cp)));
    let first = commit_picker_scroll(cp, inner.height as usize);
    // The `… more` line is not a row: a click on it is inert.
    let shown = cp.len().min(first + commit_picker_rows(cp, inner.height as usize));
    menu_hit(inner, 0, first, shown, col, row)
}

// --- Search screen -------------------------------------------------------

/// The search list's rows; the pick indexes only `File` and `Code` rows.
enum SearchRow {
    /// A `Code` file header, by its first hit's index.
    Header(usize),
    File(usize),
    Code(usize),
    /// The clip marker `… more`; the full count lives in the chip.
    More,
}

fn search_rows(s: &crate::app::SearchOverlay) -> Vec<SearchRow> {
    let mut rows = Vec::new();
    match s.search_mode {
        crate::app::SearchMode::Files => {
            rows.extend((0..s.results.files.len()).map(SearchRow::File));
            let more = s.results.file_total.saturating_sub(s.results.files.len());
            if more > 0 {
                rows.push(SearchRow::More);
            }
        }
        crate::app::SearchMode::Code => {
            // The engine groups by file already; headers only show it.
            let mut last: Option<&str> = None;
            for (i, hit) in s.results.code.iter().enumerate() {
                if last != Some(hit.path.as_str()) {
                    rows.push(SearchRow::Header(i));
                    last = Some(hit.path.as_str());
                }
                rows.push(SearchRow::Code(i));
            }
            if s.results.code_more {
                rows.push(SearchRow::More);
            }
        }
    }
    rows
}

/// The pick a display row maps to, if it is a result row.
fn search_row_pick(row: &SearchRow) -> Option<usize> {
    match row {
        SearchRow::File(i) | SearchRow::Code(i) => Some(*i),
        _ => None,
    }
}

/// A pane's titled rule `─ label ─────`, brighter than the chrome.
fn search_pane_rule(label: &str, width: usize, p: &Palette) -> Line<'static> {
    let rule = Style::default().fg(p.mark(Ink::Border, Fill::Base));
    let label = format!(" {label} ");
    let mut line = Line::from(vec![
        Span::styled("─", rule),
        Span::styled(label.clone(), Style::default().fg(p.ink(Ink::TextSecondary, Fill::Base))),
    ]);
    if let Some(pad) = width.checked_sub(label.width() + 1).filter(|w| *w > 0) {
        line.push_span(Span::styled("─".repeat(pad), rule));
    }
    line
}

/// The results pane's list area, below its title rule.
fn search_results_list(results: Rect) -> Rect {
    let title = u16::from(results.height > 1);
    Rect::new(results.x, results.y + title, results.width, results.height - title)
}

/// The search screen's bands: input, results, divider, preview.
pub(crate) struct SearchLayout {
    pub band: Rect,
    pub results: Rect,
    pub divider: Rect,
    pub preview: Rect,
}

pub(crate) fn search_layout(body: Rect, app: &App) -> SearchLayout {
    let band = Rect::new(body.x, body.y, body.width, body.height.min(1));
    let rest_y = body.y + band.height;
    let rest_h = body.height - band.height;
    let divider_h = rest_h.min(1);
    let avail = rest_h - divider_h;
    // The share splits the panes through the same minimum-pane rule as the review split.
    let results_h = split_axis(avail, app.search_pct);
    let results = Rect::new(body.x, rest_y, body.width, results_h);
    let divider = Rect::new(body.x, rest_y + results_h, body.width, divider_h);
    let preview = Rect::new(body.x, divider.y + divider.height, body.width, avail - results_h);
    SearchLayout { band, results, divider, preview }
}

/// The mode chips' texts, with counts once the engine is warm.
fn search_chip_texts(s: &crate::app::SearchOverlay) -> (String, String) {
    if s.phase != crate::app::SearchPhase::Ready {
        return ("files".to_string(), "code".to_string());
    }
    // An empty query lists no code, so its count is `0`.
    let files = format!("files {}", s.results.file_total);
    let plus = if s.results.code_more { "+" } else { "" };
    let code = format!("code {}{plus}", s.results.code.len());
    (files, code)
}

/// The painted width of `files N │ code M`.
fn chips_width(files: &str, code: &str) -> u16 {
    (files.width() + 3 + code.width()) as u16
}

fn render_search(frame: &mut Frame, app: &App, body: Rect) {
    let Some(s) = app.search.as_ref() else { return };
    let p = app.palette();
    let l = search_layout(body, app);

    // The query, then the chips, the active one lit like the active tab.
    let (files_chip, code_chip) = search_chip_texts(s);
    let chips_w = chips_width(&files_chip, &code_chip);
    let active = Style::default()
        .fg(p.ink(Ink::Accent, Fill::Base))
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let inactive = Style::default().fg(p.ink(Ink::TextSecondary, Fill::Base));
    let dim = Style::default().fg(p.mark(Ink::Border, Fill::Base));
    let files_mode = s.search_mode == crate::app::SearchMode::Files;
    let chips = Line::from(vec![
        Span::styled(files_chip, if files_mode { active } else { inactive }),
        Span::styled(" │ ", dim),
        Span::styled(code_chip, if files_mode { inactive } else { active }),
    ]);
    let query_w = l.band.width.saturating_sub(chips_w + 1);
    let prompt = "> ";
    let avail = (query_w as usize).saturating_sub(prompt.width());
    let (query_spans, caret_cell_col) =
        input_line(&s.query, s.caret, avail, "Search files and code…", p);
    let mut input = Line::from(query_spans);
    input
        .spans
        .insert(0, Span::styled(prompt, Style::default().fg(p.ink(Ink::Accent, Fill::Base))));
    let input_area = Rect::new(l.band.x, l.band.y, query_w, l.band.height);
    frame.render_widget(Paragraph::new(input), input_area);
    anchor_input_cursor(frame, input_area, prompt.width() + caret_cell_col, 0);
    if l.band.width > chips_w {
        frame.render_widget(
            Paragraph::new(chips),
            Rect::new(l.band.x + l.band.width - chips_w, l.band.y, chips_w, l.band.height),
        );
    }

    render_search_results(frame, app, s, l.results, p);
    render_search_divider(frame, s, l.divider, p);
    render_search_preview(frame, s, l.preview, p);
}

fn render_search_results(
    frame: &mut Frame,
    app: &App,
    s: &crate::app::SearchOverlay,
    region: Rect,
    p: &Palette,
) {
    if region.height == 0 {
        return;
    }
    if region.height > 1 {
        frame.render_widget(
            Paragraph::new(search_pane_rule("results", region.width as usize, p)),
            Rect::new(region.x, region.y, region.width, 1),
        );
    }
    let region = search_results_list(region);
    if region.height == 0 {
        return;
    }
    match &s.phase {
        crate::app::SearchPhase::Indexing => {
            frame.render_widget(dim_paragraph("indexing…", p), region);
            return;
        }
        crate::app::SearchPhase::Error(e) => {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    e.clone(),
                    Style::default().fg(p.ink(Ink::Danger, Fill::Base)),
                )),
                region,
            );
            return;
        }
        crate::app::SearchPhase::Ready => {}
    }

    let rows = search_rows(s);
    if rows.is_empty() {
        // An empty `Code` query looked for nothing, so no `no matches`.
        if !(s.search_mode == crate::app::SearchMode::Code && s.query.trim().is_empty()) {
            frame.render_widget(dim_paragraph("no matches", p), region);
        }
        return;
    }
    // Scroll to keep the pick visible.
    let viewport = region.height as usize;
    let picked_disp = rows.iter().position(|r| search_row_pick(r) == Some(s.pick)).unwrap_or(0);
    let mut scroll = s.scroll.get().min(rows.len().saturating_sub(viewport));
    if picked_disp < scroll {
        scroll = picked_disp;
    } else if picked_disp >= scroll + viewport {
        scroll = picked_disp + 1 - viewport;
    }
    s.scroll.set(scroll);

    let width = region.width as usize;
    let items: Vec<ListItem> = rows
        .iter()
        .skip(scroll)
        .take(viewport)
        .map(|row| match row {
            SearchRow::Header(i) => {
                let path = &s.results.code[*i].path;
                file_row_item(
                    &FileRowSpec {
                        indent: "",
                        annotation: app.changed_annotation(path),
                        name: path,
                        ignored: false,
                        emphasis: &[],
                    },
                    width,
                    Fill::Base,
                    p,
                )
            }
            SearchRow::More => ListItem::new(Line::from(Span::styled(
                "… more",
                Style::default().fg(p.ink(Ink::TextMuted, Fill::Base)),
            ))),
            SearchRow::File(i) => {
                let hit = &s.results.files[*i];
                let on = cursor_fill(s.pick == *i);
                file_row_item(
                    &FileRowSpec {
                        indent: "",
                        annotation: app.changed_annotation(&hit.path),
                        name: &hit.path,
                        ignored: false,
                        emphasis: &hit.spans,
                    },
                    width,
                    on,
                    p,
                )
            }
            SearchRow::Code(i) => {
                let hit = &s.results.code[*i];
                let on = cursor_fill(s.pick == *i);
                search_code_row(hit, width, on, p)
            }
        })
        .collect();
    frame.render_widget(List::new(items), region);
}

/// The divider: a rule titled with the previewed file, and the drag target.
fn render_search_divider(
    frame: &mut Frame,
    s: &crate::app::SearchOverlay,
    region: Rect,
    p: &Palette,
) {
    if region.height == 0 {
        return;
    }
    let label = match s.preview.as_ref() {
        Some(pv) => format!("preview · {}", pv.path),
        None => "preview".to_string(),
    };
    frame.render_widget(Paragraph::new(search_pane_rule(&label, region.width as usize, p)), region);
}

/// The preview: the picked file, its hit line centered and banded.
fn render_search_preview(
    frame: &mut Frame,
    s: &crate::app::SearchOverlay,
    region: Rect,
    p: &Palette,
) {
    if region.height == 0 {
        return;
    }
    // Nothing to preview: a notice, never a blank pane.
    let Some(pv) = s.preview.as_ref() else {
        frame.render_widget(dim_paragraph("no preview", p), region);
        return;
    };
    let notice = match pv.diff.notice {
        Some(notice) => Some(notice.message()),
        None if pv.diff.rows.is_empty() => Some("no preview"),
        None => None,
    };
    if let Some(notice) = notice {
        frame.render_widget(dim_paragraph(notice, p), region);
        return;
    }
    let rows = &pv.diff.rows;
    let h = region.height as usize;
    let max_scroll = rows.len().saturating_sub(h);
    // Center the hit once per build; `PageUp`/`PageDown` then move the pane freely.
    if pv.center.get() {
        let target = pv.hit.as_ref().map_or(0, |(l, _)| (*l as usize).saturating_sub(1));
        pv.scroll.set(target.saturating_sub(h / 2));
        pv.center.set(false);
    }
    let scroll = pv.scroll.get().min(max_scroll);
    pv.scroll.set(scroll);
    let gw = gutter_for(&pv.diff);
    let width = region.width as usize;
    let lines: Vec<Line> = rows
        .iter()
        .skip(scroll)
        .take(h)
        .map(|row| {
            let hit = pv
                .hit
                .as_ref()
                .filter(|(l, _)| row.new_no() == Some(*l as u32))
                .map(|(_, spans)| spans.as_slice());
            search_preview_line(row, gw, width, hit, p)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), region);
}

/// One preview row; the hit row takes the band and match emphasis.
fn search_preview_line(
    row: &Row,
    gw: usize,
    width: usize,
    hit: Option<&[(u32, u32)]>,
    p: &Palette,
) -> Line<'static> {
    let num = row.new_no().map_or(String::new(), |n| n.to_string());
    // The hit line sits on the cursor fill, so its colors resolve there.
    let on = cursor_fill(hit.is_some());
    let syntax = |c| p.legible(c, on);
    let mut spans =
        vec![Span::styled(format!("{num:>gw$} "), Style::default().fg(p.ink(Ink::TextMuted, on)))];
    match hit {
        None => {
            for sp in row.spans() {
                spans.push(Span::styled(
                    sp.text.replace('\t', "    "),
                    Style::default().fg(rgb(sp.color)),
                ));
            }
            Line::from(spans)
        }
        Some(ranges) => {
            let text = row.text();
            // The engine's offsets skip indentation the preview keeps.
            let indent = (text.len() - text.trim_start().len()) as u32;
            let ranges: Vec<(u32, u32)> =
                ranges.iter().map(|&(s, e)| (s + indent, e + indent)).collect();
            // Each byte's syntax color, recovered in one forward pass.
            let mut colors: Vec<(usize, Color)> = Vec::new();
            let mut at = 0usize;
            for sp in row.spans() {
                colors.push((at, syntax(rgb(sp.color))));
                at += sp.text.len();
            }
            let mut ci = 0usize;
            let base = |byte: usize| {
                while ci + 1 < colors.len() && colors[ci + 1].0 <= byte {
                    ci += 1;
                }
                Style::default().fg(colors.get(ci).map_or(p.ink(Ink::Text, on), |&(_, c)| c))
            };
            let emphasized = emphasized_spans(&text, &ranges, search_hl(p), base);
            spans.extend(
                emphasized
                    .into_iter()
                    .map(|sp| Span::styled(sp.content.replace('\t', "    "), sp.style)),
            );
            let mut line = Line::from(spans);
            let pad = width.saturating_sub(line.width());
            if pad > 0 {
                line.push_span(Span::raw(" ".repeat(pad)));
            }
            line.style(Style::default().bg(p.fill(Fill::Cursor)))
        }
    }
}

/// A code match row, clipped around its first match.
fn search_code_row(
    hit: &crate::search::CodeHit,
    width: usize,
    on: Fill,
    p: &Palette,
) -> ListItem<'static> {
    let locator = format!("{:>5}: ", hit.line);
    let avail = width.saturating_sub(locator.width());
    // Expand tabs before the width and clip math run on the line (see `expand_tabs`).
    let (text, match_spans) = expand_tabs(hit.text.trim_end(), &hit.spans);
    let text = text.as_str();
    // Cut the head to show the match, at a char boundary or the slice panics.
    let mut first = match_spans.first().map_or(0, |&(s, _)| s as usize).min(text.len());
    while first > 0 && !text.is_char_boundary(first) {
        first -= 1;
    }
    let head_cols: usize = text[..first].width();
    let (skip_bytes, prefix) = if head_cols + 8 > avail && avail > 8 {
        // Walk from the front until the remaining head fits in a third of the row.
        let keep = avail / 3;
        let mut cut = 0;
        let mut remaining = head_cols;
        for (i, c) in text[..first].char_indices() {
            if remaining <= keep {
                cut = i;
                break;
            }
            remaining -= unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            cut = i + c.len_utf8();
        }
        // `…` only for a real cut.
        (cut, if cut > 0 { "…" } else { "" })
    } else {
        (0, "")
    };
    let shown = &text[skip_bytes..];
    let offset = skip_bytes as u32;
    let shifted: Vec<(u32, u32)> = match_spans
        .iter()
        .filter(|&&(_, e)| e > offset)
        .map(|&(st, e)| (st.saturating_sub(offset), e - offset))
        .collect();
    let mut spans = vec![Span::styled(locator, Style::default().fg(p.ink(Ink::TextMuted, on)))];
    if !prefix.is_empty() {
        spans
            .push(Span::styled(prefix.to_string(), Style::default().fg(p.ink(Ink::TextMuted, on))));
    }
    spans.extend(emphasized_spans(shown, &shifted, search_hl(p), |_| text_style(p, on)));
    selectable_row(p, spans, width, on)
}

/// Expand tabs to four spaces, shifting the match spans; width math counts a tab as zero.
fn expand_tabs(text: &str, spans: &[(u32, u32)]) -> (String, Vec<(u32, u32)>) {
    if !text.contains('\t') {
        return (text.to_string(), spans.to_vec());
    }
    let mut out = String::with_capacity(text.len());
    let mut tabs = Vec::new();
    for (i, c) in text.char_indices() {
        if c == '\t' {
            out.push_str("    ");
            tabs.push(i);
        } else {
            out.push(c);
        }
    }
    // Each tab strictly before an offset added three bytes; the tab positions are sorted.
    let shift = |at: usize| -> u32 { (tabs.partition_point(|&t| t < at) * 3) as u32 };
    let spans =
        spans.iter().map(|&(s, e)| (s + shift(s as usize), e + shift(e as usize))).collect();
    (out, spans)
}

/// The search screen's match highlight: the same block find paints.
fn search_hl(p: &Palette) -> impl Fn(Style) -> Style {
    let hl = match_style(p);
    move |style| style.patch(hl)
}

/// Split `text` into spans, restyling the matched byte ranges by `hl` over `base`.
/// `base` sees byte indices in increasing order, so a caller may walk a forward cursor.
fn emphasized_spans(
    text: &str,
    ranges: &[(u32, u32)],
    hl: impl Fn(Style) -> Style,
    mut base: impl FnMut(usize) -> Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    for (i, c) in text.char_indices() {
        let mut style = base(i);
        if ranges.iter().any(|&(s, e)| (s as usize) <= i && i < (e as usize)) {
            style = hl(style);
        }
        if run.is_empty() {
            run_style = style;
        } else if style != run_style {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
            run_style = style;
        }
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    spans
}

/// What a mouse position lands on in the search screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchTarget {
    /// The mode chips — a click flips the mode.
    Chips,
    /// A result row's pick index.
    Row(usize),
    /// The divider row — mouse-down starts the share drag.
    Divider,
    /// Elsewhere in the results pane — the wheel moves the pick here.
    Results,
    /// The preview pane — the wheel scrolls it here.
    Preview,
}

pub fn search_target(app: &App, area: Rect, col: u16, row: u16) -> Option<SearchTarget> {
    let s = app.search.as_ref()?;
    let l = search_layout(body_rect(area, app), app);
    let within = |r: Rect| {
        col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height && r.height > 0
    };
    if within(l.band) {
        let (files_chip, code_chip) = search_chip_texts(s);
        let chips_w = chips_width(&files_chip, &code_chip);
        if l.band.width > chips_w && col >= l.band.x + l.band.width - chips_w {
            return Some(SearchTarget::Chips);
        }
        return None;
    }
    if within(l.divider) {
        return Some(SearchTarget::Divider);
    }
    if within(l.preview) {
        return Some(SearchTarget::Preview);
    }
    if !within(l.results) {
        return None;
    }
    // Off `Ready` a message is painted, not rows.
    if s.phase != crate::app::SearchPhase::Ready {
        return Some(SearchTarget::Results);
    }
    // The title rule is the pane, not a row.
    let list = search_results_list(l.results);
    if !within(list) {
        return Some(SearchTarget::Results);
    }
    let disp = s.scroll.get() + (row - list.y) as usize;
    match search_rows(s).get(disp).and_then(search_row_pick) {
        Some(pick) => Some(SearchTarget::Row(pick)),
        None => Some(SearchTarget::Results),
    }
}

/// The default body text color.
fn text_style(p: &Palette, on: Fill) -> Style {
    Style::default().fg(p.ink(Ink::Text, on))
}

/// A list row on the fill `on` its colors were resolved for, a cursor row bold full width.
fn selectable_row(
    p: &Palette,
    mut spans: Vec<Span<'static>>,
    width: usize,
    on: Fill,
) -> ListItem<'static> {
    if let Some(bg) = p.bg(on) {
        let used: usize = spans.iter().map(Span::width).sum();
        if width > used {
            spans.push(Span::raw(" ".repeat(width - used)));
        }
        for s in &mut spans {
            // A span's own background, like a match highlight, wins.
            if s.style.bg.is_none() {
                s.style = s.style.bg(bg);
            }
            s.style = s.style.add_modifier(Modifier::BOLD);
        }
    }
    ListItem::new(Line::from(spans))
}

// --- PR tab --------------------------------

/// The `PR` tab's header: tabs, then the title and a clickable `status #number ↗` chip.
fn render_pr_header(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let bar = Style::default().bg(p.fill(Fill::Bar));
    let mut spans = tab_bar_spans(app);
    let lead_tabs: usize = spans.iter().map(Span::width).sum();
    let w = area.width as usize;

    // No PR: the read pane alone says why.
    if let forge::PrView::Pr(s) = &app.pr {
        let number = format!("{}{}", app.pr_forge.sigil(), s.number);
        let (status, color) = pr_status_chip(p, s);
        let chip_w = pr_chip_width(app, s);
        // The head branch, `⑂` for a fork, dropped first when narrow.
        let head = match (s.head_ref.is_empty(), s.head_is_fork) {
            (true, _) => String::new(),
            (false, true) => format!("⑂ {}", s.head_ref),
            (false, false) => s.head_ref.clone(),
        };
        let head_w = if head.is_empty() { 0 } else { head.width() + 2 };
        // Keep the branch only while the title still gets a readable minimum beside it.
        let head_w =
            if w.saturating_sub(lead_tabs + chip_w + 2 + head_w) >= 8 { head_w } else { 0 };
        // The title fills the gap left of the branch + chip, right-aligned (a leading pad).
        let name =
            truncate_width(&s.title, w.saturating_sub(lead_tabs + chip_w + 2 + head_w).max(4));
        let pad = w.saturating_sub(lead_tabs + name.width() + head_w + 2 + chip_w);
        spans.push(Span::styled(" ".repeat(pad), bar));
        spans.push(Span::styled(name, bar.fg(p.ink(Ink::TextSecondary, Fill::Bar))));
        if head_w > 0 {
            spans.push(Span::styled("  ", bar));
            spans.push(Span::styled(head, bar.fg(p.ink(Ink::TextMuted, Fill::Bar))));
        }
        spans.push(Span::styled("  ", bar));
        spans.push(Span::styled(status, bar.fg(color).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(" ", bar));
        spans.push(Span::styled(
            number,
            bar.fg(p.ink(Ink::Accent, Fill::Bar)).add_modifier(Modifier::BOLD),
        ));
        // The arrow shares the PR number's colour, reading as part of the clickable chip.
        spans.push(Span::styled(" ↗", bar.fg(p.ink(Ink::Accent, Fill::Bar))));
    }

    // Fill the rest of the bar (the Pr arm already reaches the right edge).
    let used: usize = spans.iter().map(Span::width).sum();
    if used < w {
        spans.push(Span::styled(" ".repeat(w - used), bar));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The status chip word for a PR's lifecycle; its accent comes from [`pr_status_chip`].
fn pr_status_word(s: &forge::PrSnapshot) -> &'static str {
    match s.state {
        forge::PrState::Merged => "merged",
        forge::PrState::Closed => "closed",
        forge::PrState::Open if s.is_draft => "draft",
        forge::PrState::Open => "open",
    }
}

/// The status chip word and its color, by lifecycle.
fn pr_status_chip(p: &Palette, s: &forge::PrSnapshot) -> (&'static str, Color) {
    let color = match s.state {
        forge::PrState::Merged => p.ink(Ink::Merged, Fill::Bar),
        forge::PrState::Closed => p.ink(Ink::Danger, Fill::Bar),
        // A draft is not ready for review yet: muted, the way GitHub greys it.
        forge::PrState::Open if s.is_draft => p.ink(Ink::TextMuted, Fill::Bar),
        forge::PrState::Open => p.ink(Ink::Success, Fill::Bar),
    };
    (pr_status_word(s), color)
}

/// The width of the `status #number ↗` chip.
fn pr_chip_width(app: &App, s: &forge::PrSnapshot) -> usize {
    pr_status_word(s).width()
        + " ".width()
        + format!("{}{}", app.pr_forge.sigil(), s.number).width()
        + " ↗".width()
}

/// The footer's `·`-joined merge, sync, and checks; merge and sync only while open.
fn pr_state_line(_app: &App, s: &forge::PrSnapshot) -> String {
    let mut parts: Vec<String> = Vec::new();
    if s.state == forge::PrState::Open {
        match s.merge {
            forge::Merge::Conflicting => parts.push(format!("⚠ conflicts with {}", s.base_ref)),
            forge::Merge::Blocked => parts.push("blocked".into()),
            forge::Merge::Clean => {}
        }
        match s.sync {
            forge::Sync::Unpushed(n) => parts.push(format!("⇡ {n} unpushed")),
            forge::Sync::Behind(n) => parts.push(format!("⇣ {n} behind")),
            forge::Sync::Unknown => parts.push("? sync unknown".to_string()),
            forge::Sync::InSync => {}
        }
    }
    parts.push(checks_summary(s));
    parts.push(crate::export::counted_comments(s.comments.len()));
    if s.comments_truncated {
        parts.push("newest 100 comments".into());
    }
    if s.checks_truncated {
        parts.push("newest 100 checks".into());
    }
    parts.join(" · ")
}

/// The checks rollup in one token, e.g. `✗ 1 check failing`.
fn checks_summary(s: &forge::PrSnapshot) -> String {
    let checks = |n: usize| if n == 1 { "1 check".to_string() } else { format!("{n} checks") };
    match s.checks_rollup() {
        None => "no checks".into(),
        Some(forge::CheckStatus::Failure) => format!("✗ {} failing", checks(s.failing_checks())),
        Some(forge::CheckStatus::Running) => "● checks running".into(),
        Some(_) => match s.passed_checks() {
            0 => "⊘ checks skipped".into(),
            n => format!("✓ {} passed", checks(n)),
        },
    }
}

/// The PR navigator: checks above newest-first comments.
fn render_pr_nav(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let block = bordered("Checks & comments", app.focus == Focus::Files, p);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let rows = pr_nav_rows(app, width, std::time::SystemTime::now());
    let viewport = inner.height as usize;
    // Transitional frames retain the request until both a viewport and its selected row exist.
    let can_reveal = viewport > 0 && rows.iter().any(|row| row.cursor == Some(app.pr_cursor));
    let reveal = can_reveal && app.take_pr_nav_reveal();
    let (scroll, max_scroll) =
        settle_pr_nav_scroll(&rows, app.pr_cursor, viewport, app.pr_nav_scroll(), reveal);
    app.note_pr_nav_max_scroll(max_scroll);
    app.set_pr_nav_scroll(scroll);
    let items: Vec<ListItem> = rows
        .into_iter()
        .skip(scroll)
        .take(viewport)
        .map(|row| {
            let selected = row.cursor == Some(app.pr_cursor);
            selectable_row(p, row.spans, width, cursor_fill(selected))
        })
        .collect();
    frame.render_widget(List::new(items), inner);
}

/// One painted PR navigator row and the cursor index it selects, when interactive.
struct PrNavRow {
    spans: Vec<Span<'static>>,
    cursor: Option<usize>,
}

/// The complete PR navigator layout, shared by painting and click hit-testing.
fn pr_nav_rows(app: &App, width: usize, now: std::time::SystemTime) -> Vec<PrNavRow> {
    let Some(s) = app.pr_snapshot() else { return Vec::new() };
    let p = app.palette();
    let dim = Style::default().fg(p.ink(Ink::TextMuted, Fill::Base));
    // A row the cursor is on sits on the cursor fill, so its colors resolve there.
    let on = |cursor: usize| cursor_fill(cursor == app.pr_cursor);
    let mut rows = Vec::new();
    if app.pr_has_description() {
        rows.push(PrNavRow {
            spans: vec![Span::styled("description", text_style(p, on(0)))],
            cursor: Some(0),
        });
        rows.push(PrNavRow { spans: Vec::new(), cursor: None });
    }
    rows.push(PrNavRow { spans: vec![Span::styled(pr_checks_header(s), dim)], cursor: None });
    for check in &s.checks {
        let (glyph, color) = check_glyph(p, check.status);
        rows.push(PrNavRow {
            spans: vec![
                Span::styled(format!(" {glyph} "), Style::default().fg(color)),
                Span::styled(check.name.clone(), text_style(p, Fill::Base)),
            ],
            cursor: None,
        });
    }
    rows.push(PrNavRow { spans: Vec::new(), cursor: None });
    let drafts: usize = s
        .comments
        .iter()
        .map(|c| {
            usize::from(c.draft_id.is_some())
                + c.replies.iter().filter(|r| r.draft_id.is_some()).count()
        })
        .sum();
    let mut header = format!("comments · {}", s.comments.len());
    if drafts > 0 {
        let _ = write!(header, " · {drafts} draft{}", if drafts == 1 { "" } else { "s" });
    }
    rows.push(PrNavRow { spans: vec![Span::styled(header, dim)], cursor: None });
    let offset = app.pr_description_offset();
    rows.extend(s.comments.iter().enumerate().map(|(index, comment)| PrNavRow {
        spans: pr_comment_row(comment, width, now, p, on(index + offset)),
        cursor: Some(index + offset),
    }));
    rows
}

fn settle_pr_nav_scroll(
    rows: &[PrNavRow],
    cursor: usize,
    viewport: usize,
    current: usize,
    reveal: bool,
) -> (usize, usize) {
    let max = rows.len().saturating_sub(viewport);
    let mut scroll = current.min(max);
    if reveal && let Some(target) = rows.iter().position(|row| row.cursor == Some(cursor)) {
        if target < scroll {
            scroll = target;
        } else if target >= scroll.saturating_add(viewport) {
            scroll = target.saturating_add(1).saturating_sub(viewport);
        }
    }
    (scroll.min(max), max)
}

/// The `checks` section header: the rollup the footer shows, which names checks itself.
fn pr_checks_header(s: &forge::PrSnapshot) -> String {
    checks_summary(s)
}

/// One comment row: `@author anchor`, then a trailing `draft`/`resolved`/`outdated` marker or the age.
fn pr_comment_row(
    cm: &forge::Comment,
    width: usize,
    now: std::time::SystemTime,
    p: &Palette,
    on: Fill,
) -> Vec<Span<'static>> {
    let author_color =
        p.ink(if cm.author_is_bot { Ink::TextMuted } else { Ink::TextSecondary }, on);
    let draft_reply = cm.replies.iter().any(|r| r.draft_id.is_some());
    let trailing = if cm.draft_id.is_some() {
        "draft".to_string()
    } else if draft_reply {
        "draft reply".to_string()
    } else if cm.is_resolved {
        "resolved".to_string()
    } else if cm.is_outdated {
        "outdated".to_string()
    } else {
        relative_age(&cm.created_at, now)
    };
    let author = format!("@{} ", cm.author);
    let budget = width.saturating_sub(author.width() + trailing.width() + 3).max(1);
    let anchor = elide_head(&cm.anchor, budget);
    let trailing_ink =
        if cm.draft_id.is_some() || draft_reply { Ink::Warning } else { Ink::TextMuted };
    vec![
        Span::styled(author, Style::default().fg(author_color)),
        Span::styled(anchor, text_style(p, on)),
        Span::styled(format!("  {trailing}"), Style::default().fg(p.ink(trailing_ink, on))),
    ]
}

/// Note the link and `<details>` regions of the rendered rows on screen.
fn note_rendered_regions(app: &App, slots: &[Slot], inner: Rect, prefix_w: usize) {
    let code_w = (inner.width as usize).saturating_sub(prefix_w);
    let x0 = inner.x + prefix_w as u16;
    for (off, slot) in slots.iter().enumerate() {
        let Slot::Code { row, .. } = *slot else { continue };
        let Some(Row::Rendered { kind: RenderedKind::Block { line, .. }, .. }) =
            app.visible.get(row)
        else {
            continue;
        };
        let Some(meta) = app.rendered_meta(*line) else { continue };
        note_line_regions(app, meta, x0, code_w, inner.y + off as u16);
    }
}

/// Note one painted line's regions, shifted to `x0` and clipped at `width`.
fn note_line_regions(app: &App, meta: &crate::markdown::LineMeta, x0: u16, width: usize, y: u16) {
    let clip = |c: usize| x0 + c.min(width) as u16;
    for link in &meta.links {
        let (x1, x2) = (clip(link.start), clip(link.end));
        if x1 < x2 {
            app.note_painted_link(x1, x2, y, link.url.clone());
        }
    }
    if let Some(d) = &meta.details {
        let (x1, x2) = (clip(d.start), clip(d.end));
        if x1 < x2 {
            app.note_painted_details(x1, x2, y, d.key.clone());
        }
    }
}

/// Note a render's visible links, and all its anchors, since a jump can leave the viewport.
fn note_markdown_regions(
    app: &App,
    rendered: &crate::markdown::Rendered,
    inner: Rect,
    scroll: usize,
    offset: usize,
) {
    for (slug, line) in &rendered.anchors {
        app.note_painted_anchor(slug.clone(), line + offset);
    }
    let viewport = inner.height as usize;
    let visible = rendered.meta.iter().enumerate().filter_map(|(i, m)| {
        match (i + offset).checked_sub(scroll) {
            Some(d) if d < viewport => Some((d, m)),
            _ => None,
        }
    });
    for (display, m) in visible {
        note_line_regions(app, m, inner.x, inner.width as usize, inner.y + display as u16);
    }
}

/// A scroll row, saturating so a huge render pins to the end.
fn saturating_row(scroll: usize) -> u16 {
    u16::try_from(scroll).unwrap_or(u16::MAX)
}

/// A scrollbar for the `PR` read pane when its content overflows.
fn render_overflow_scrollbar(
    frame: &mut Frame,
    track: Rect,
    max: usize,
    scroll: usize,
    p: &Palette,
) {
    if max == 0 {
        return;
    }
    let mut state = ScrollbarState::new(max).position(scroll);
    // A heavy thumb on the border, with no track.
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("┃")
            .thumb_style(Style::default().fg(p.mark(Ink::Accent, Fill::Base))),
        track,
        &mut state,
    );
}

fn push_finding_quote(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    cm: &crate::forge::Comment,
    width: usize,
    p: &Palette,
) -> Option<(std::ops::Range<usize>, usize)> {
    let place = cm.place.as_ref()?;
    let (start, end) = place.range?;
    let side = place.side.unwrap_or(crate::model::Side::New);
    let rows = cm
        .snippet
        .as_deref()
        .map(|hunk| app.snippet_rows(hunk, &place.path, start, end, side))
        .unwrap_or_default();
    let sign = snippet_caption_sign(&rows, start, end, side);
    lines.push(Line::from(Span::styled(
        forge::finding_range_caption(start, end, sign),
        Style::default().fg(p.ink(Ink::TextMuted, Fill::Base)),
    )));
    let mut snippet = None;
    if !rows.is_empty() {
        let max_no = rows.iter().filter_map(|r| r.new_no().or_else(|| r.old_no())).max();
        let gutter_w = gutter_width(max_no.unwrap_or(0) as usize);
        let layout = RowLayout {
            gutter_w,
            width,
            h_scroll: 0,
            wrap: true,
            focused: false,
            pal: p,
            find: None,
            // Snippet rows never carry the cursor, so no fold ever shows the hint here.
            expand_hint: "",
            rendered: &[],
            see: "",
        };
        let from = lines.len();
        for row in &rows {
            let state = RowState {
                commented: snippet_row_is_comment(row, start, end, side),
                cursor: false,
                selected: false,
                hovered: false,
                lead: false,
            };
            lines.extend(render_row(row, layout, state));
        }
        // The quote's line range and gutter prefix, whose cells a selection never copies
        snippet = Some((from..lines.len(), gutter_prefix_width(gutter_w)));
        push_comment_rule(lines, width, p);
    }
    lines.push(Line::raw(""));
    snippet
}

fn push_comment_rule(lines: &mut Vec<Line<'static>>, width: usize, p: &Palette) {
    lines.push(Line::from(Span::styled(
        "─".repeat(width.max(1)),
        Style::default().fg(p.mark(Ink::Border, Fill::Base)),
    )));
}

fn push_comment_byline(
    lines: &mut Vec<Line<'static>>,
    author: &str,
    is_bot: bool,
    created_at: &str,
    draft_id: Option<u64>,
    now: std::time::SystemTime,
    p: &Palette,
) {
    let author_color = p.ink(if is_bot { Ink::TextMuted } else { Ink::TextSecondary }, Fill::Base);
    let mut spans = vec![Span::styled(format!("@{author}"), Style::default().fg(author_color))];
    if let Some(id) = draft_id {
        spans.push(Span::styled(SEP, Style::default().fg(p.mark(Ink::Border, Fill::Base))));
        spans.push(Span::styled(
            format!("DRAFT {id}"),
            Style::default().fg(p.ink(Ink::Warning, Fill::Base)).add_modifier(Modifier::BOLD),
        ));
    }
    let age = relative_age(created_at, now);
    if !age.is_empty() {
        spans.push(Span::styled(SEP, Style::default().fg(p.mark(Ink::Border, Fill::Base))));
        spans.push(Span::styled(age, Style::default().fg(p.ink(Ink::TextMuted, Fill::Base))));
    }
    lines.push(Line::from(spans));
}

/// The PR read pane's content, for paint and selection alike.
struct PrReadContent {
    /// The trimmed notice lines painted above the body.
    notice: Vec<String>,
    /// The body's display lines.
    lines: Vec<Line<'static>>,
    /// Each markdown body's render metadata and its first display row, for hit-testing.
    body_meta: Vec<(usize, crate::markdown::Rendered)>,
    /// The snippet's lines and its uncopied gutter width.
    snippet: Option<(std::ops::Range<usize>, usize)>,
}

fn pr_read_content(app: &App, inner: Rect) -> PrReadContent {
    let p = app.palette();
    let selected = app.pr_selected_comment();
    let width = inner.width as usize;
    let notice_lines =
        app.pr_notice().map(|notice| wrap_text(notice, width.max(1))).unwrap_or_default();
    // Keep a body row; a tight notice keeps its head and its remedy.
    let notice_capacity = match inner.height {
        0 => 0,
        1 => 1,
        height => height - 1,
    } as usize;
    let notice = if notice_lines.len() <= notice_capacity {
        notice_lines
    } else if notice_capacity == 0 {
        Vec::new()
    } else if notice_capacity == 1 {
        notice_lines.into_iter().rev().take(1).collect()
    } else {
        let tail = notice_lines.len() - (notice_capacity - 1);
        std::iter::once(notice_lines[0].clone())
            .chain(notice_lines.into_iter().skip(tail))
            .collect()
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut body_meta: Vec<(usize, crate::markdown::Rendered)> = Vec::new();
    let mut snippet = None;
    if let Some(cm) = selected {
        // The finding's range paints as Diff-view rows; only the prose body is markdown
        snippet = push_finding_quote(&mut lines, app, cm, width, p);
        let now = std::time::SystemTime::now();
        // Every turn gets a byline, so a reply never reads as the same comment.
        push_comment_byline(
            &mut lines,
            &cm.author,
            cm.author_is_bot,
            &cm.created_at,
            cm.draft_id,
            now,
            p,
        );
        let mut rendered = app.pr_body_render(&cm.body, width.max(1), 0);
        let offset = lines.len();
        lines.append(&mut rendered.lines);
        body_meta.push((offset, rendered));
        for (i, reply) in cm.replies.iter().enumerate() {
            push_comment_rule(&mut lines, width, p);
            push_comment_byline(
                &mut lines,
                &reply.author,
                reply.author_is_bot,
                &reply.created_at,
                reply.draft_id,
                now,
                p,
            );
            let mut rendered = app.pr_body_render(&reply.body, width.max(1), i + 1);
            let offset = lines.len();
            lines.append(&mut rendered.lines);
            body_meta.push((offset, rendered));
        }
        if let Some(note) = app
            .pr_draft_target()
            .and_then(|(draft, _)| app.rework_note_for(draft.draft_id))
            .and_then(|i| app.store.get(i))
        {
            push_comment_rule(&mut lines, width, p);
            lines.push(Line::from(Span::styled(
                "rework note · queued for the agent",
                Style::default().fg(p.ink(Ink::Comment, Fill::Base)),
            )));
            for piece in wrap_text(&note.text, width.max(1)) {
                lines.push(Line::from(Span::styled(piece, text_style(p, Fill::Base))));
            }
        }
    } else if app.pr_on_description() {
        if let Some(s) = app.pr_snapshot() {
            let mut rendered = app.pr_body_render(&s.body, width.max(1), 0);
            let offset = lines.len();
            lines.append(&mut rendered.lines);
            body_meta.push((offset, rendered));
        }
    } else {
        // The empty-state remedy can outgrow a narrow pane; wrap it rather than clip it.
        let refresh = app.keymap().hint(crate::keymap::Action::Refresh);
        for piece in wrap_text(&pr_empty_msg(&app.pr, app.pr_forge, refresh), width.max(1)) {
            lines.push(Line::from(Span::styled(
                piece,
                Style::default().fg(p.ink(Ink::TextMuted, Fill::Base)),
            )));
        }
    }
    PrReadContent { notice, lines, body_meta, snippet }
}

/// The PR read pane: the selected description or comment, or the loading/degraded message.
fn render_pr_read(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let title = match app.pr_selected_comment() {
        Some(cm) => format!("@{} · {}", cm.author, cm.anchor),
        None if app.pr_on_description() => "description".to_string(),
        None => app.pr_forge.abbr().to_string(),
    };
    let block = bordered(&title, app.focus == Focus::Diff, p);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let content = pr_read_content(app, inner);
    let notice_height = content.notice.len() as u16;
    if notice_height > 0 {
        let notice_area = Rect::new(inner.x, inner.y, inner.width, notice_height);
        frame.render_widget(
            Paragraph::new(
                content
                    .notice
                    .iter()
                    .map(|line| {
                        Line::from(Span::styled(
                            line.clone(),
                            Style::default().fg(p.ink(Ink::Warning, Fill::Base)),
                        ))
                    })
                    .collect::<Vec<_>>(),
            ),
            notice_area,
        );
    }
    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(notice_height),
        inner.width,
        inner.height.saturating_sub(notice_height),
    );

    // Clamp before the `u16` cast, so a stale scroll can't wrap.
    let max = content.lines.len().saturating_sub(body.height as usize);
    app.note_pr_read_max_scroll(max);
    let scroll = app.pr_read_scroll.min(max);
    for (offset, rendered) in &content.body_meta {
        note_markdown_regions(app, rendered, body, scroll, *offset);
    }
    frame.render_widget(Paragraph::new(content.lines).scroll((saturating_row(scroll), 0)), body);
    render_overflow_scrollbar(
        frame,
        Rect::new(area.x, body.y, area.width, body.height),
        max,
        scroll,
        p,
    );
}

/// The message for a PR view with nothing to show, in the forge's noun.
fn pr_empty_msg(
    view: &forge::PrView,
    forge: crate::git::Forge,
    refresh: crate::keymap::Key,
) -> String {
    if let Some(message) = view.retry_remedy(refresh) {
        return message;
    }
    let noun = forge.noun();
    match view {
        forge::PrView::Loading => "loading…".into(),
        forge::PrView::Pending | forge::PrView::Pr(_) | forge::PrView::Held => String::new(),
        forge::PrView::Detached => format!("No {noun} for a detached HEAD."),
        forge::PrView::NoPr => format!("No {noun} yet. Ready to ship?"),
        forge::PrView::NoCli(_)
        | forge::PrView::NoExtension(_)
        | forge::PrView::NotAuthed(..)
        | forge::PrView::GitError(_)
        | forge::PrView::Error(..) => {
            unreachable!("retry failures returned above")
        }
        forge::PrView::NeedsForgeRemote => {
            "The PR tab needs a GitHub, GitLab, or Azure DevOps remote named upstream or origin."
                .into()
        }
        forge::PrView::UnsupportedHost(host) => {
            format!(
                "Unsupported host: {host}. Self-hosted? Set `github_host`, `gitlab_host`, or `azure_devops_host`."
            )
        }
        forge::PrView::MalformedOrigin(host) => {
            format!("The origin remote must point to a repository path on {host}.")
        }
    }
}

/// Whether a click lands on the header's PR chip.
#[must_use]
pub fn hit_pr_open(area: Rect, app: &App, col: u16, row: u16) -> bool {
    let Some(s) = app.pr_snapshot() else {
        return false;
    };
    if row != area.y {
        return false;
    }
    let chip_w = pr_chip_width(app, s) as u16;
    // The chip occupies the last `chip_w` columns; `saturating_sub` keeps the bound overflow-free.
    col >= area.width.saturating_sub(chip_w) && col < area.width
}

/// The cursor index a PR navigator row selects.
#[must_use]
pub fn pr_nav_cursor_at(app: &App, row: usize) -> Option<usize> {
    pr_nav_rows(app, usize::MAX, std::time::SystemTime::now()).get(row)?.cursor
}

/// The status glyph and its color for a check. Running and queued checks are yellow, like
/// herdr's working dot and GitHub's pending checks.
fn check_glyph(p: &Palette, status: forge::CheckStatus) -> (&'static str, Color) {
    let on = Fill::Base;
    match status {
        forge::CheckStatus::Success => ("✓", p.mark(Ink::Success, on)),
        forge::CheckStatus::Failure => ("✗", p.mark(Ink::Danger, on)),
        forge::CheckStatus::Running => ("●", p.mark(Ink::Warning, on)),
        forge::CheckStatus::Pending => ("○", p.mark(Ink::Warning, on)),
        forge::CheckStatus::Skipped => ("⊘", p.mark(Ink::TextMuted, on)),
    }
}

// --- helpers -------------------------------------------------------------------

fn bordered(title: &str, focused: bool, p: &Palette) -> Block<'static> {
    // A focused pane's border is the accent; an unfocused one recedes to the border tone.
    let color =
        if focused { p.mark(Ink::Accent, Fill::Base) } else { p.mark(Ink::Border, Fill::Base) };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(framed_title(title))
}

/// A block title with a space each side.
fn framed_title(title: &str) -> String {
    if title.is_empty() { String::new() } else { format!(" {title} ") }
}

fn dim_paragraph<'a>(text: &'a str, p: &Palette) -> Paragraph<'a> {
    Paragraph::new(text).style(Style::default().fg(p.ink(Ink::TextMuted, Fill::Base)))
}

/// A change marker's color: the diff's added, removed or modified hue.
fn kind_color(p: &Palette, kind: ChangeKind, on: Fill) -> Color {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => p.ink(Ink::Added, on),
        ChangeKind::Deleted => p.ink(Ink::Removed, on),
        ChangeKind::Renamed | ChangeKind::Copied | ChangeKind::Modified => p.ink(Ink::Modified, on),
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
