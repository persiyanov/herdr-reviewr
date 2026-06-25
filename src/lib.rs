//! herdr-review — a herdr-native review sidebar.
//!
//! Browse an agent's changes (uncommitted / branch), leave line-range comments,
//! and send them back to the agent (or the clipboard) — entirely in a herdr pane.
//!
//! This crate is split into a thin binary (`src/main.rs`) and this library so the
//! interaction logic in [`app`] stays terminal-free and unit-testable. This module
//! owns the terminal lifecycle and the event loop; it maps input events onto
//! [`app::App`] methods and renders with [`ui`].

pub mod app;
pub mod config;
pub mod diff;
pub mod export;
pub mod file_list;
pub mod git;
pub mod herdr;
pub mod highlight;
#[macro_use]
pub mod log;
pub mod model;
pub mod ui;

use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;

use crate::app::{App, Focus, Mode};
use crate::config::Config;
use crate::export::{Agent, Clipboard};
use crate::model::Scope;

/// Entry point: parse config, set up the terminal, run the loop, restore.
pub fn run() -> Result<()> {
    let cfg = Config::from_env();
    log::init();
    // A non-repo path is not an error — the sidebar opens to an empty state and
    // starts showing changes if the directory becomes a repo (specs/herdr-host.md).
    let repo = git::toplevel(&cfg.repo).unwrap_or_else(|| cfg.repo.clone());
    logln!("start repo={} poll={:?} base={:?}", repo.display(), cfg.poll, cfg.base);
    let mut app = App::new(repo, Scope::Uncommitted, cfg.base.clone());
    app.set_theme(cfg.theme.as_deref());
    if let Some(wrap) = cfg.wrap {
        app.wrap = wrap;
    }
    app.reload()?;

    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, &mut app, cfg.poll);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// A transient status message (e.g. "sent 3 comments") fades after this long idle.
const STATUS_TTL: Duration = Duration::from_secs(4);

/// Draw, then wait up to the poll deadline for input; refresh on each tick.
fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, poll: Duration) -> Result<()> {
    let mut last_poll = Instant::now();
    let mut status_at = Instant::now();
    let mut last_status = String::new();
    while !app.should_quit {
        // Expire a stale status line: restart the timer when the message changes, and clear
        // it once it has lingered past the TTL, so a notification doesn't stay up forever.
        if app.status != last_status {
            last_status.clone_from(&app.status);
            status_at = Instant::now();
        }
        if !app.status.is_empty() && status_at.elapsed() >= STATUS_TTL {
            app.status.clear();
            last_status.clear();
        }
        // Settle both panes' scroll for this frame's viewport before painting, so the
        // diff window matches what mouse hit-testing will map against. Each pane reveals its
        // cursor only when a navigation requested it (so the wheel can scroll freely), then
        // bounds the offset every frame. While composing, reserve the inline box's rows and
        // keep revealing so the anchored line stays above the growing box.
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let viewport = ui::diff_viewport_height(area, app.list_pct);
        let effective = if app.composing() {
            let box_h = ui::composer_height(app, ui::diff_inner_width(area, app.list_pct));
            viewport.saturating_sub(box_h).max(1)
        } else {
            viewport
        };
        let heights = ui::diff_row_heights(app, area);
        if std::mem::take(&mut app.reveal_diff) || app.composing() {
            app.reveal_diff_cursor(&heights, effective);
        }
        app.bound_diff_scroll(effective);
        let file_vp = ui::file_viewport_height(area, app.list_pct);
        if std::mem::take(&mut app.reveal_files) {
            app.reveal_file_cursor(file_vp);
        }
        app.bound_file_scroll(file_vp);
        terminal.draw(|f| ui::render(f, app))?;
        // Wake at the status-expiry boundary too, so it clears on time when idle.
        let poll_left = poll.saturating_sub(last_poll.elapsed());
        let timeout = if app.status.is_empty() {
            poll_left
        } else {
            poll_left.min(STATUS_TTL.saturating_sub(status_at.elapsed()))
        };
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if let Err(e) = handle_key(app, k) {
                        app.status = format!("error: {e}");
                    }
                    logln!(
                        "key {:?}{} -> mode={:?} focus={:?} scope={:?} file={}/{} diff_cursor={} scroll={} comments={}",
                        k.code,
                        if k.modifiers.is_empty() {
                            String::new()
                        } else {
                            format!(" {:?}", k.modifiers)
                        },
                        app.mode,
                        app.focus,
                        app.scope,
                        app.file_cursor,
                        app.files.len(),
                        app.diff_cursor,
                        app.diff_scroll,
                        app.store.len()
                    );
                }
                Event::Mouse(m) => {
                    let size = terminal.size()?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    if let Err(e) = handle_mouse(app, m, area) {
                        app.status = format!("error: {e}");
                    }
                    logln!(
                        "mouse {:?} col={} row={} -> focus={:?} file={} diff_cursor={} scroll={} anchor={:?}",
                        m.kind,
                        m.column,
                        m.row,
                        app.focus,
                        app.file_cursor,
                        app.diff_cursor,
                        app.diff_scroll,
                        app.select_anchor
                    );
                }
                _ => {}
            }
        }
        if last_poll.elapsed() >= poll {
            // A failed refresh must never crash the UI or drop a comment.
            if let Err(e) = app.reload() {
                app.status = format!("refresh failed: {e}");
            }
            logln!(
                "poll files={} composing={} diff_cursor={} scroll={}",
                app.files.len(),
                app.composing(),
                app.diff_cursor,
                app.diff_scroll
            );
            last_poll = Instant::now();
        }
    }
    Ok(())
}

/// Diff scroll steps: a full page for `PageUp`/`PageDown`, half for `ctrl+u`/`ctrl+d`.
const PAGE: isize = 15;
const HALF_PAGE: isize = 8;

fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    use KeyCode::{Backspace, Char, Down, Enter, Esc, Left, PageDown, PageUp, Right, Tab, Up};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if app.composing() {
        let alt_or_shift = key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        match key.code {
            Esc => app.cancel_comment(),
            // Alt/Shift+Enter (and Ctrl+J) insert a newline; plain Enter submits.
            Enter if alt_or_shift => app.input_push('\n'),
            Enter => app.submit_comment(),
            Char('j') if ctrl => app.input_push('\n'),
            Char('w') if ctrl => app.input_delete_word(),
            Backspace => app.input_backspace(),
            Char(c) if !ctrl => app.input_push(c),
            _ => {}
        }
        return Ok(());
    }

    if app.mode == Mode::List {
        match key.code {
            Esc | Char('l' | 'q') => app.close_list(),
            Char('j') | Down => app.list_move(1),
            Char('k') | Up => app.list_move(-1),
            Char('s') => app.export(&Agent),
            Char('y') => app.export(&Clipboard),
            Char('e') => app.start_edit(),
            Char('d') => app.delete_comment(),
            _ => {}
        }
        return Ok(());
    }

    match (key.code, ctrl) {
        // ctrl combos first, so they win over the plain `u`/`d` bindings below. Half-page
        // keys move the focused pane's cursor (the view follows), like `j`/`k`.
        (Char('u'), true) if app.focus == Focus::Files => app.page_files(-HALF_PAGE),
        (Char('d'), true) if app.focus == Focus::Files => app.page_files(HALF_PAGE),
        (Char('u'), true) => app.page_diff(-HALF_PAGE),
        (Char('d'), true) => app.page_diff(HALF_PAGE),
        (Char('q'), _) => app.should_quit = true,
        (Char('r'), _) => app.reload()?,
        (Tab, _) => app.toggle_focus(),
        (Char('j') | Down, _) => app.move_cursor(1)?,
        (Char('k') | Up, _) => app.move_cursor(-1)?,
        // Page keys move the focused pane's cursor.
        (PageDown, _) if app.focus == Focus::Files => app.page_files(PAGE),
        (PageUp, _) if app.focus == Focus::Files => app.page_files(-PAGE),
        (PageDown, _) => app.page_diff(PAGE),
        (PageUp, _) => app.page_diff(-PAGE),
        (Char('w'), _) => app.toggle_wrap(),
        // `]` widens the file list, `[` narrows it (widening the diff).
        (Char(']'), _) => app.resize_list(4),
        (Char('['), _) => app.resize_list(-4),
        // `←`/`→` expand/collapse the collapsible under the cursor — a directory in the file
        // list, a fold in the diff (expand-only); otherwise they scroll the diff sideways
        // (`scroll_h` is a no-op while wrapping, so it only acts when h-scroll is meaningful).
        (Right, _) if app.on_folder() => app.expand_dir(),
        (Left, _) if app.on_folder() => app.collapse_dir(),
        (Right, _) if app.on_fold() => app.expand_fold(),
        (Right, _) => app.scroll_h(8),
        (Left, _) => app.scroll_h(-8),
        (Char('u'), false) => app.set_scope(Scope::Uncommitted)?,
        (Char('b'), false) => app.set_scope(Scope::Branch)?,
        (Char('v'), _) => app.toggle_select(),
        (Char('c'), _) => app.start_comment(),
        (Char('e'), _) => app.start_edit(),
        (Char('d'), false) => app.delete_comment(),
        (Char('s' | 'S'), _) => app.export(&Agent),
        (Char('y' | 'Y'), _) => app.export(&Clipboard),
        (Char('n'), _) => app.jump_comment(1),
        (Char('N'), _) => app.jump_comment(-1),
        (Char('l'), _) => app.open_list(),
        _ => {}
    }
    Ok(())
}

fn handle_mouse(app: &mut App, m: MouseEvent, area: Rect) -> Result<()> {
    // A modal (the comment composer or the comments-list overlay) captures the screen and is
    // keyboard-driven, so the mouse is inert while one is open — otherwise clicks and the
    // wheel would drive the panes drawn underneath it.
    if app.composing() || app.mode == Mode::List {
        return Ok(());
    }
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // The divider is checked first: a grab there starts a resize, not a selection.
            if ui::hit_divider(area, app.list_pct, m.column, m.row) {
                app.resizing = true;
            } else if let Some(hit) = ui::hit_header(area, app, m.column, m.row) {
                match hit {
                    ui::HeaderHit::Scope => app.set_scope(app.scope.toggled())?,
                    ui::HeaderHit::Send => app.export(&Agent),
                }
            } else if let Some(i) = ui::hit_file(
                area,
                app.list_pct,
                m.column,
                m.row,
                app.file_rows.len(),
                app.file_scroll,
            ) {
                app.select_file(i)?;
            } else if let Some(i) = ui::hit_diff(
                area,
                app.list_pct,
                m.column,
                m.row,
                &ui::diff_row_heights(app, area),
                app.diff_scroll,
            ) {
                app.focus = Focus::Diff;
                app.diff_cursor = i;
                app.select_anchor = None;
                // A click on a fold marker toggles it.
                app.expand_fold();
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.resizing {
                let body = ui::body_rect(area);
                app.drag_divider(body.width, m.column.saturating_sub(body.x));
            } else if let Some(i) = ui::hit_diff(
                area,
                app.list_pct,
                m.column,
                m.row,
                &ui::diff_row_heights(app, area),
                app.diff_scroll,
            ) {
                app.drag_select_to(i);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => app.resizing = false,
        // The wheel scrolls the viewport of whichever pane it is over — never the cursor, so
        // a comment is never anchored to a wheeled-past line. Horizontal scroll is
        // keyboard-only (`←`/`→`), since multiplexers don't reliably deliver h-wheel events.
        MouseEventKind::ScrollDown if ui::in_files_pane(area, app.list_pct, m.column, m.row) => {
            app.wheel_files(3);
        }
        MouseEventKind::ScrollUp if ui::in_files_pane(area, app.list_pct, m.column, m.row) => {
            app.wheel_files(-3);
        }
        MouseEventKind::ScrollDown => app.wheel_diff(3),
        MouseEventKind::ScrollUp => app.wheel_diff(-3),
        _ => {}
    }
    Ok(())
}
