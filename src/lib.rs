//! herdr-reviewr — a herdr-native review sidebar.
//!
//! Browse an agent's changes (uncommitted / branch), leave line-range comments,
//! and send them back to the agent (or the clipboard) — entirely in a herdr pane.
//!
//! This crate is split into a thin binary (`src/main.rs`) and this library so the
//! interaction logic in [`app`] stays terminal-free and unit-testable. This module
//! owns the terminal lifecycle and the event loop; it maps input events onto
//! [`app::App`] methods and renders with [`ui`].

pub mod app;
pub mod browser;
pub mod config;
pub mod diff;
pub mod export;
pub mod file_list;
pub mod forge;
pub mod git;
pub mod herdr;
pub mod highlight;
pub mod keymap;
#[macro_use]
pub mod log;
pub mod markdown;
pub mod model;
pub mod proc;
pub mod theme;
pub mod turn;
pub mod ui;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;
use ratatui::layout::Rect;

use crate::app::{App, Focus, Mode};
use crate::config::{Config, PluginConfig};
use crate::export::{Agent, Clipboard};
use crate::keymap::Keymap;
use crate::model::Scope;

/// Entry point: parse config, set up the terminal, run the loop, restore.
pub fn run() -> Result<()> {
    let cfg = Config::from_env();
    log::init();
    let initial_config = config::plugin_config();
    let mut app = match &initial_config {
        Ok(plugin_config) => ready_app(&cfg, plugin_config.clone()),
        Err(error) => {
            let mut app = App::blocked(cfg.repo.clone(), Scope::Uncommitted, cfg.base.clone());
            app.set_config_error(error.to_string());
            app
        }
    };

    let mut terminal = ratatui::init();
    // Bracketed paste so a multi-line paste arrives as one event, not raw keystrokes whose
    // embedded newlines would submit the comment early.
    let _ = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    // The kitty keyboard protocol reports modifiers on keys the legacy encoding drops — most
    // notably Ctrl/Alt+arrows — so word-jump by arrow works where the terminal supports it.
    let kbd = supports_keyboard_enhancement().unwrap_or(false);
    logln!("keyboard enhancement supported={kbd}");
    if kbd {
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    // Render before the first load, so a slow, failing, or hung `git` scan shows the reviewr UI
    // instead of the blank pane herdr leaves when the process blocks or exits before it renders
    // (issue #4). Paint the empty frame first; then the initial load, non-fatal — an error opens
    // the sidebar with the reason in the status line, the same contract as a failed poll refresh.
    if let Err(error) = terminal.draw(|f| ui::render(f, &app)) {
        restore_terminal(kbd);
        return Err(error.into());
    }
    if initial_config.is_ok()
        && let Err(e) = app.reload()
    {
        logln!("startup reload failed: {e:#}");
        app.status = format!("load failed: {e}");
    }
    event_loop(&mut terminal, &mut app, &cfg, kbd)
}

/// Leave the alternate screen and release terminal input modes before any bounded worker drain.
fn restore_terminal(kbd: bool) {
    if kbd {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
}

/// Build a fresh working sidebar only after the plugin configuration has validated.
fn ready_app(cfg: &Config, plugin_config: PluginConfig) -> App {
    // A non-repo path is not an error — the sidebar opens to an empty state and starts showing
    // changes if the directory becomes a repo (specs/herdr-host.md).
    let repo = git::toplevel(&cfg.repo).unwrap_or_else(|| cfg.repo.clone());
    let scope = plugin_config.default_scope();
    logln!(
        "start repo={} poll={:?} base={:?} scope={}",
        repo.display(),
        cfg.poll,
        cfg.base,
        scope.name()
    );
    let mut app = App::new(repo, scope, cfg.base.clone());
    app.set_plugin_config(plugin_config);
    app.set_cli_theme(cfg.theme.clone());
    if let Some(wrap) = cfg.wrap {
        app.wrap = wrap;
    }
    app
}

/// A transient status message (e.g. "sent 3 comments") fades after this long idle.
const STATUS_TTL: Duration = Duration::from_secs(4);

/// While the `PR` tab is active, refetch GitHub at least this often — a fallback for forge-side
/// changes with no local signal (a reviewer's comment). Local pushes and `gh` PR actions refresh
/// sooner, on the agent's turn-end, so this cadence is the slow safety net (specs/forge-host.md).
const PR_POLL: Duration = Duration::from_mins(1);
const PR_LOADING_DELAY: Duration = Duration::from_millis(150);
const PR_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug)]
struct TaggedPr {
    generation: u64,
    config_epoch: u64,
    input: crate::forge::PrFetchInput,
    view: crate::forge::PrView,
}

#[derive(Debug)]
enum PrEffect {
    Clear,
    Apply(crate::forge::PrView),
}

/// Owns PR refresh convergence. Generations supersede mid-flight triggers, config epochs reject
/// work from another validated snapshot, and a fresh input probe must match before a completion
/// can paint. An off-tab input change clears stale state but defers its replacement fetch.
#[derive(Debug)]
struct PrRefresh {
    generation: u64,
    current_input: Option<crate::forge::PrFetchInput>,
    pending: Option<TaggedPr>,
    fetch_needed: bool,
}

/// Owns the active probe or fetch until its worker exits; start guards keep all PR work serialized.
#[derive(Debug)]
struct PrCoordinator {
    refresh: PrRefresh,
    wait_started: Option<Instant>,
    active_probe_epoch: Option<u64>,
    active_fetch: Option<ActiveFetch>,
    probe_pending: bool,
}

#[derive(Debug)]
struct ActiveFetch {
    tag: (u64, u64),
    cancelled: Arc<AtomicBool>,
}

/// The config and layout that produced the visible frame. Input dispatches only while these
/// values still match, so a late observation can never reinterpret painted keys or geometry.
#[derive(Debug)]
struct PaintedFrameSnapshot {
    plugin_config: Option<PluginConfig>,
    config_error: Option<String>,
    navigator_position: crate::config::NavigatorPosition,
    navigator_side_pct: u16,
    navigator_stack_pct: u16,
}

impl PaintedFrameSnapshot {
    fn capture(app: &App) -> Self {
        Self {
            plugin_config: app.plugin_config().cloned(),
            config_error: app.config_error().map(str::to_owned),
            navigator_position: app.navigator_position,
            navigator_side_pct: app.navigator_side_pct,
            navigator_stack_pct: app.navigator_stack_pct,
        }
    }

    fn still_current(&self, app: &App) -> bool {
        self.plugin_config.as_ref() == app.plugin_config()
            && self.config_error.as_deref() == app.config_error()
            && self.navigator_position == app.navigator_position
            && self.navigator_side_pct == app.navigator_side_pct
            && self.navigator_stack_pct == app.navigator_stack_pct
    }

    fn keymap(&self) -> &Keymap {
        match &self.plugin_config {
            Some(config) => config.keymap(),
            None => keymap::default_keymap(),
        }
    }
}

impl PrCoordinator {
    fn new(ready: bool) -> Self {
        Self {
            refresh: PrRefresh::new(ready),
            wait_started: ready.then(Instant::now),
            active_probe_epoch: None,
            active_fetch: None,
            probe_pending: ready,
        }
    }

    fn stop(&mut self) {
        self.refresh.invalidate();
        self.wait_started = None;
        self.cancel_fetch();
        self.probe_pending = false;
    }

    fn recover(&mut self) {
        self.refresh.invalidate();
        self.refresh.trigger();
        self.wait_started = Some(Instant::now());
        self.cancel_fetch();
        self.probe_pending = true;
    }

    fn config_changed(&mut self, active: bool) {
        self.cancel_fetch();
        self.refresh.config_changed(active);
        self.probe_pending = active;
    }

    fn cancel_fetch(&self) {
        if let Some(active) = &self.active_fetch {
            active.cancelled.store(true, Ordering::Release);
        }
    }

    fn active_fetch_tag(&self) -> Option<(u64, u64)> {
        self.active_fetch.as_ref().map(|active| active.tag)
    }

    fn can_start_probe(&self, config_ready: bool) -> bool {
        self.probe_pending
            && self.active_probe_epoch.is_none()
            && self.active_fetch.is_none()
            && config_ready
    }
}

impl Drop for PrCoordinator {
    fn drop(&mut self) {
        self.cancel_fetch();
    }
}

/// Cancel the active GitHub fetch and briefly drain matching probe/fetch completions before exit.
fn drain_pr_shutdown(
    pr: &mut PrCoordinator,
    probe_rx: &mpsc::Receiver<(
        u64,
        Result<crate::forge::PrFetchInput, crate::forge::PrInputError>,
    )>,
    pr_rx: &mpsc::Receiver<TaggedPr>,
) {
    pr.stop();
    let deadline = Instant::now() + PR_SHUTDOWN_GRACE;
    while (pr.active_probe_epoch.is_some() || pr.active_fetch.is_some())
        && Instant::now() < deadline
    {
        if let Ok((epoch, _)) = probe_rx.try_recv()
            && pr.active_probe_epoch == Some(epoch)
        {
            pr.active_probe_epoch = None;
        }
        if let Ok(completion) = pr_rx.try_recv() {
            let tag = (completion.generation, completion.config_epoch);
            if pr.active_fetch_tag() == Some(tag) {
                pr.active_fetch = None;
            }
        }
        if pr.active_probe_epoch.is_some() || pr.active_fetch.is_some() {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn schedule_poll_probe(pr: &mut PrCoordinator, tab: crate::app::Tab) {
    if tab == crate::app::Tab::Pr {
        pr.probe_pending = true;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigGate {
    Blocked,
    Unchanged,
    Changed { file_reloaded: bool, pr_changed: bool },
}

impl ConfigGate {
    fn ready(self) -> bool {
        self != Self::Blocked
    }

    fn pr_unchanged(self) -> bool {
        !matches!(self, Self::Blocked | Self::Changed { pr_changed: true, .. })
    }

    fn file_reloaded(self) -> bool {
        matches!(self, Self::Changed { file_reloaded: true, .. })
    }
}

impl PrRefresh {
    fn new(ready: bool) -> Self {
        Self { generation: 1, current_input: None, pending: None, fetch_needed: ready }
    }

    fn trigger(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.fetch_needed = true;
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.current_input = None;
        self.pending = None;
        self.fetch_needed = false;
    }

    fn config_changed(&mut self, active: bool) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.fetch_needed = active;
    }

    fn completed(&mut self, completion: TaggedPr, epoch: u64, active: bool) {
        if completion.generation == self.generation && completion.config_epoch == epoch {
            self.pending = Some(completion);
        } else {
            self.pending = None;
            self.fetch_needed = self.fetch_needed || active;
        }
    }

    fn observed(
        &mut self,
        input: crate::forge::PrFetchInput,
        epoch: u64,
        active: bool,
    ) -> Option<PrEffect> {
        let changed = self.current_input.as_ref().is_some_and(|old| old != &input);
        if changed {
            self.generation = self.generation.wrapping_add(1);
            self.pending = None;
            self.current_input = Some(input);
            self.fetch_needed = active;
            return Some(PrEffect::Clear);
        }
        self.current_input = Some(input.clone());
        if let Some(completion) = self.pending.take() {
            if completion.generation == self.generation
                && completion.config_epoch == epoch
                && completion.input == input
            {
                self.fetch_needed = false;
                return Some(PrEffect::Apply(completion.view));
            }
            self.fetch_needed = true;
        }
        None
    }

    fn probe_failed(&mut self, retry_pending: bool) {
        self.pending = None;
        self.fetch_needed = retry_pending;
    }

    fn take_fetch(&mut self) -> Option<(u64, crate::forge::PrFetchInput)> {
        if !self.fetch_needed {
            return None;
        }
        let input = self.current_input.clone()?;
        self.fetch_needed = false;
        Some((self.generation, input))
    }
}

/// Draw, then wait up to the poll deadline for input; refresh on each tick.
fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    cfg: &Config,
    kbd: bool,
) -> Result<()> {
    let poll = cfg.poll;
    let mut last_poll = Instant::now();
    let mut last_pr_poll = Instant::now();
    // Local input probes and GitHub reads run on workers. A completed fetch is applied only after
    // a fresh probe proves its complete input still matches (`specs/forge-host.md`).
    let (probe_tx, probe_rx) =
        mpsc::channel::<(u64, Result<crate::forge::PrFetchInput, crate::forge::PrInputError>)>();
    let (recovery_tx, recovery_rx) = mpsc::channel::<(u64, PluginConfig, App)>();
    let mut recovery_inflight = false;
    let (pr_tx, pr_rx) = mpsc::channel::<TaggedPr>();
    let mut pr = PrCoordinator::new(app.plugin_config().is_some());
    let mut config_epoch = 0_u64;
    let mut status_at = Instant::now();
    let mut last_status = String::new();
    // Fetch the PR snapshot as soon as the panel opens, not on first switching to the tab, so the
    // tab is already populated when the user gets there (specs/forge-host.md).
    app.pr_pending = false;
    let result: Result<()> = (|| {
        while !app.should_quit {
            if let Ok((epoch, target, mut recovered)) = recovery_rx.try_recv() {
                recovery_inflight = false;
                if epoch == config_epoch {
                    match config::plugin_config() {
                        Ok(current) if current == target => {
                            recovered.carry_authored_state_from(app);
                            *app = recovered;
                            pr.recover();
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let message = error.to_string();
                            if app.config_error() != Some(message.as_str()) {
                                config_epoch = config_epoch.wrapping_add(1);
                            }
                            app.set_config_error(message);
                            pr.stop();
                        }
                    }
                }
            }

            // Every frame starts from one complete validated config snapshot. Input below uses the
            // keymap and geometry this draw paints; later observations continue before another input.
            reconcile_plugin_config(
                app,
                cfg,
                &mut config_epoch,
                &recovery_tx,
                &mut recovery_inflight,
                &mut pr,
            );
            if pr.wait_started.is_some_and(|started| started.elapsed() >= PR_LOADING_DELAY) {
                app.set_pr_refreshing(true);
                pr.wait_started = None;
            }
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
            let viewport = ui::diff_viewport_height(area, app);
            let effective = if app.composing() {
                let box_h = ui::composer_height(app, ui::diff_inner_width(area, app));
                viewport.saturating_sub(box_h).max(1)
            } else {
                viewport
            };
            let heights = ui::diff_row_heights(app, area);
            if std::mem::take(&mut app.reveal_diff) || app.composing() {
                app.reveal_diff_cursor(&heights, effective);
            }
            app.bound_diff_scroll(&heights, effective);
            let file_vp = ui::file_viewport_height(area, app);
            if std::mem::take(&mut app.reveal_files) {
                app.reveal_file_cursor(file_vp);
            }
            app.bound_file_scroll(file_vp);
            let painted_frame = PaintedFrameSnapshot::capture(app);
            terminal.draw(|f| ui::render(f, app))?;

            // Record user and fallback refreshes before consuming worker results. A trigger that
            // arrived during completion verification supersedes that completion before it can
            // paint, while the generation still coalesces repeated triggers into one fresh fetch.
            let fallback_poll = app.tab == crate::app::Tab::Pr && last_pr_poll.elapsed() >= PR_POLL;
            if app.pr_pending || fallback_poll {
                app.pr_pending = false;
                last_pr_poll = Instant::now();
                pr.refresh.trigger();
                pr.wait_started.get_or_insert_with(Instant::now);
                pr.probe_pending = true;
            }

            // A fetch completion waits for a fresh local-input probe before it may paint.
            if let Ok(completion) = pr_rx.try_recv() {
                let tag = (completion.generation, completion.config_epoch);
                if pr.active_fetch_tag() != Some(tag) {
                    continue;
                }
                pr.active_fetch = None;
                let config_gate = reconcile_plugin_config(
                    app,
                    cfg,
                    &mut config_epoch,
                    &recovery_tx,
                    &mut recovery_inflight,
                    &mut pr,
                );
                if config_gate.pr_unchanged() {
                    pr.refresh.completed(completion, config_epoch, app.tab == crate::app::Tab::Pr);
                    pr.probe_pending = true;
                }
                if config_gate != ConfigGate::Unchanged {
                    continue;
                }
            }

            // A probe result is the authority for the current input. Input changes blank the old PR;
            // a fetch result paints only when this probe exactly matches its tagged input.
            if let Ok((epoch, result)) = probe_rx.try_recv() {
                if pr.active_probe_epoch != Some(epoch) {
                    continue;
                }
                pr.active_probe_epoch = None;
                let config_gate = reconcile_plugin_config(
                    app,
                    cfg,
                    &mut config_epoch,
                    &recovery_tx,
                    &mut recovery_inflight,
                    &mut pr,
                );
                let mut repaint = false;
                if !config_gate.pr_unchanged() || epoch != config_epoch {
                    if config_gate == ConfigGate::Unchanged && epoch != config_epoch {
                        pr.config_changed(app.tab == crate::app::Tab::Pr);
                    }
                } else {
                    repaint = apply_pr_probe_result(app, &mut pr, result, config_epoch);
                }
                if config_gate != ConfigGate::Unchanged || repaint {
                    continue;
                }
            }

            if pr.can_start_probe(app.plugin_config().is_some()) {
                pr.probe_pending = false;
                let (tx, repo, base, plugin_config, epoch) = (
                    probe_tx.clone(),
                    app.repo.clone(),
                    app.base.clone(),
                    app.plugin_config().expect("config checked above").clone(),
                    config_epoch,
                );
                let verifies_completion = pr.refresh.pending.is_some();
                pr.active_probe_epoch = Some(epoch);
                thread::spawn(move || {
                    let input = if verifies_completion {
                        crate::forge::verify_input(&repo, base.as_deref(), &plugin_config)
                    } else {
                        crate::forge::fetch_input(&repo, base.as_deref(), &plugin_config)
                    };
                    let _ = tx.send((epoch, input));
                });
            }

            if pr.active_fetch.is_none()
                && pr.active_probe_epoch.is_none()
                && !pr.probe_pending
                && let Some((generation, input)) = pr.refresh.take_fetch()
            {
                let (tx, repo, epoch) = (pr_tx.clone(), app.repo.clone(), config_epoch);
                let cancelled = Arc::new(AtomicBool::new(false));
                pr.active_fetch =
                    Some(ActiveFetch { tag: (generation, epoch), cancelled: cancelled.clone() });
                thread::spawn(move || {
                    let view = crate::forge::fetch_cancellable(&repo, &input, &cancelled);
                    let _ = tx.send(TaggedPr { generation, config_epoch: epoch, input, view });
                });
            }
            // Wake at the status-expiry boundary too, so it clears on time when idle.
            let poll_left = poll.saturating_sub(last_poll.elapsed());
            let mut timeout = if app.status.is_empty() {
                poll_left
            } else {
                poll_left.min(STATUS_TTL.saturating_sub(status_at.elapsed()))
            };
            // While a fetch is in flight, wake often so its result paints promptly when it lands.
            if pr.active_fetch.is_some() || pr.active_probe_epoch.is_some() {
                timeout = timeout.min(Duration::from_millis(100));
            }
            if let Some(started) = pr.wait_started {
                timeout = timeout.min(PR_LOADING_DELAY.saturating_sub(started.elapsed()));
            }
            if event::poll(timeout)? {
                if !painted_frame.still_current(app) {
                    continue;
                }
                let event = event::read()?;
                if app.config_error().is_some() {
                    handle_blocked_event(app, &event);
                    continue;
                }
                match event {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        if let Err(e) = handle_key(app, k, area, painted_frame.keymap()) {
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
                            app.entries.len(),
                            app.diff_cursor,
                            app.diff_scroll,
                            app.store.len()
                        );
                    }
                    Event::Mouse(m) => {
                        // Reuse this frame's `area` and `heights` (computed above for the scroll
                        // settle) so a drag-select doesn't re-measure the whole diff per motion.
                        if let Err(e) = handle_mouse(app, m, area, &heights, painted_frame.keymap())
                        {
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
                    // Bracketed paste: insert at the caret while composing, ignored otherwise.
                    Event::Paste(text) => {
                        app.input_paste(&text);
                        logln!("paste {} chars -> composing={}", text.len(), app.composing());
                    }
                    Event::Resize(_, _) => {
                        handle_resize(app);
                    }
                    _ => {}
                }
            }
            if app.should_quit {
                break;
            }
            if last_poll.elapsed() >= poll {
                let config_gate = reconcile_plugin_config(
                    app,
                    cfg,
                    &mut config_epoch,
                    &recovery_tx,
                    &mut recovery_inflight,
                    &mut pr,
                );
                if !config_gate.ready() {
                    last_poll = Instant::now();
                    continue;
                }
                schedule_poll_probe(&mut pr, app.tab);
                // Advance the last-turn baseline before reloading, so a turn promoted this poll
                // is visible to this poll's changed-files build. When the agent just went idle, its
                // turn may have pushed or run `gh pr merge`; refetch the PR if the tab is showing it
                // (entering the tab refetches on its own otherwise) (specs/forge-host.md).
                let turn_changed = app.track_turn();
                if turn_changed && app.tab == crate::app::Tab::Pr {
                    app.pr_pending = true;
                }
                // A failed refresh must never crash the UI or drop a comment.
                if (!config_gate.file_reloaded() || turn_changed)
                    && let Err(e) = app.reload()
                {
                    app.status = format!("refresh failed: {e}");
                }
                logln!(
                    "poll files={} composing={} diff_cursor={} scroll={}",
                    app.entries.len(),
                    app.composing(),
                    app.diff_cursor,
                    app.diff_scroll
                );
                last_poll = Instant::now();
            }
        }
        Ok(())
    })();
    restore_terminal(kbd);
    drain_pr_shutdown(&mut pr, &probe_rx, &pr_rx);
    result
}

/// A blocked frame accepts no normal input. Quit remains available, and terminal/pointer cleanup
/// may release state that was captured before the config became invalid.
fn handle_blocked_event(app: &mut App, event: &Event) {
    match event {
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            if let KeyCode::Char(c) = k.code
                && keymap::default_keymap().action_for(c) == Some(keymap::Action::Quit)
            {
                app.should_quit = true;
            }
        }
        Event::Mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), .. })
            if app.divider_drag_captured() =>
        {
            app.finish_divider_drag();
        }
        Event::Resize(_, _) => handle_resize(app),
        _ => {}
    }
}

/// Apply a verified PR probe result and report whether the painted model changed. The event loop
/// must repaint before accepting input whenever this returns true.
fn apply_pr_probe_result(
    app: &mut App,
    pr: &mut PrCoordinator,
    result: Result<crate::forge::PrFetchInput, crate::forge::PrInputError>,
    config_epoch: u64,
) -> bool {
    match result {
        Err(error) => {
            pr.refresh.probe_failed(pr.probe_pending);
            let (message, same_target) = match error {
                crate::forge::PrInputError::TargetRead(message) => (message, false),
                crate::forge::PrInputError::BranchState { target, message } => {
                    let same_target = pr.refresh.current_input.as_ref().is_some_and(|input| {
                        matches!(
                            &input.repository,
                            crate::git::RepositoryIdentity::Repository(current)
                                if current == &target
                        )
                    });
                    (message, same_target)
                }
            };
            if !same_target {
                app.clear_pr();
            }
            app.apply_pr(crate::forge::PrView::GitError(message));
            pr.wait_started = None;
            true
        }
        Ok(input) => {
            match pr.refresh.observed(input, config_epoch, app.tab == crate::app::Tab::Pr) {
                Some(PrEffect::Clear) => {
                    app.clear_pr();
                    pr.wait_started = (app.tab == crate::app::Tab::Pr).then(Instant::now);
                    true
                }
                Some(PrEffect::Apply(view)) => {
                    app.apply_pr(view);
                    pr.wait_started = None;
                    true
                }
                None => false,
            }
        }
    }
}

fn reconcile_plugin_config(
    app: &mut App,
    cfg: &Config,
    config_epoch: &mut u64,
    recovery_tx: &mpsc::Sender<(u64, PluginConfig, App)>,
    recovery_inflight: &mut bool,
    pr: &mut PrCoordinator,
) -> ConfigGate {
    let previous = app.plugin_config().cloned();
    if !observe_plugin_config(app, cfg, config_epoch, recovery_tx, recovery_inflight) {
        pr.stop();
        return ConfigGate::Blocked;
    }
    let current = app.plugin_config().expect("ready after successful observation");
    let Some(previous) = previous.filter(|previous| previous != current) else {
        return ConfigGate::Unchanged;
    };

    let bases_changed = previous.base_branches() != current.base_branches();
    let file_changed = bases_changed || previous.theme() != current.theme();
    let pr_changed = bases_changed || previous.github_host() != current.github_host();
    if pr_changed {
        pr.config_changed(app.tab == crate::app::Tab::Pr);
    }
    if file_changed {
        // `base_branches` participates in every Branch-scope derivation, and a theme change
        // invalidates highlighted diffs. Rebuild before another input or frame can mix states;
        // `reload` preserves the frozen diff while composing.
        if let Err(error) = app.reload() {
            app.status = format!("config refresh failed: {error}");
        }
    }
    ConfigGate::Changed { file_reloaded: file_changed, pr_changed }
}

/// Observe one complete config snapshot. Invalid state blocks work. Recovery loads a fresh app on
/// a tagged worker, then the event loop revalidates its target and carries authored review state
/// before swapping it in.
fn observe_plugin_config(
    app: &mut App,
    cfg: &Config,
    epoch: &mut u64,
    recovery_tx: &mpsc::Sender<(u64, PluginConfig, App)>,
    recovery_inflight: &mut bool,
) -> bool {
    apply_plugin_config_observation(
        app,
        cfg,
        epoch,
        recovery_tx,
        recovery_inflight,
        config::plugin_config(),
    )
}

fn apply_plugin_config_observation(
    app: &mut App,
    cfg: &Config,
    epoch: &mut u64,
    recovery_tx: &mpsc::Sender<(u64, PluginConfig, App)>,
    recovery_inflight: &mut bool,
    observed: Result<PluginConfig, config::PluginConfigError>,
) -> bool {
    match observed {
        Ok(next) => {
            let recovering = app.plugin_config().is_none();
            let changed = app.plugin_config().is_some_and(|current| current != &next);
            if recovering {
                if !*recovery_inflight {
                    *epoch = epoch.wrapping_add(1);
                    *recovery_inflight = true;
                    let (tx, cfg, target, recovery_epoch) =
                        (recovery_tx.clone(), cfg.clone(), next, *epoch);
                    thread::spawn(move || {
                        let mut recovered = ready_app(&cfg, target.clone());
                        if let Err(error) = recovered.reload() {
                            recovered.status = format!("load failed: {error}");
                        }
                        let _ = tx.send((recovery_epoch, target, recovered));
                    });
                }
                return false;
            } else if changed {
                let current = app.plugin_config().expect("ready config");
                if current.base_branches() != next.base_branches()
                    || current.github_host() != next.github_host()
                {
                    *epoch = epoch.wrapping_add(1);
                }
                app.set_plugin_config(next);
            }
            true
        }
        Err(error) => {
            let message = error.to_string();
            if app.plugin_config().is_some() || app.config_error() != Some(message.as_str()) {
                *epoch = epoch.wrapping_add(1);
            }
            app.set_config_error(message);
            false
        }
    }
}

/// Diff scroll steps: a full page for `PageUp`/`PageDown`, half for `ctrl+u`/`ctrl+d`.
const PAGE: isize = 15;
const HALF_PAGE: isize = 8;

/// Map one key press onto `App` through `keymap` — the keymap of the frame on screen, so a
/// stale hint never dispatches a different action than it advertised (`specs/config.md`).
/// Public for the dispatch tests; the event loop is the runtime caller.
pub fn handle_key(app: &mut App, key: KeyEvent, area: Rect, keymap: &Keymap) -> Result<()> {
    use crate::keymap::Action as K;
    use KeyCode::{
        Backspace, Char, Delete, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Tab,
        Up,
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // A keypress cancels the gesture but keeps consuming its drag events until mouse-up.
    app.cancel_divider_drag();

    if app.composing() {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let alt_or_shift = key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        let word = alt || ctrl; // word-jump on Alt/Ctrl + arrow (terminal-dependent)
        // The wrapped width of the box, for vertical (wrapped-row) caret movement.
        let cw = ui::composer_content_width(ui::diff_inner_width(area, app));
        match key.code {
            Esc => app.cancel_comment(),
            // Alt/Shift+Enter (and Ctrl+J) insert a newline; plain Enter submits.
            Enter if alt_or_shift => app.input_push('\n'),
            Enter => app.submit_comment(),
            Char('j') if ctrl => app.input_push('\n'),
            Char('w') if ctrl => app.input_delete_word(),
            Char('a') if ctrl => app.caret_home(),
            Char('e') if ctrl => app.caret_end(),
            Char('u') if ctrl => app.input_kill_to_start(),
            Char('k') if ctrl => app.input_kill_to_end(),
            // Word-jump: `Alt+b`/`Alt+f` (readline; survives as ESC-prefixed, unlike modified
            // arrows, which many terminals/multiplexers strip) and modified arrows where they
            // are delivered. These precede the plain-character insert below.
            Char('b') if alt => app.caret_word_left(),
            Char('f') if alt => app.caret_word_right(),
            Left if word => app.caret_word_left(),
            Right if word => app.caret_word_right(),
            Left => app.caret_left(),
            Right => app.caret_right(),
            Up => app.caret = ui::caret_vertical(&app.input, app.caret, cw, false),
            Down => app.caret = ui::caret_vertical(&app.input, app.caret, cw, true),
            Home => app.caret_home(),
            End => app.caret_end(),
            Delete => app.input_delete_forward(),
            Backspace => app.input_backspace(),
            Char(c) if !ctrl => app.input_push(c),
            _ => {}
        }
        return Ok(());
    }

    // The bound character shortcuts dispatch through the frame's keymap; a `ctrl` chord is
    // never a bound key. `↓`/`↑` are fixed synonyms of the `down`/`up` actions, folded in here
    // so every context pairs them exactly once. The other fixed keys (`tab`, `esc`, the page
    // keys, `←`/`→`) stay hardcoded per context below (`specs/tui.md`).
    let action = match key.code {
        Char(c) if !ctrl => keymap.action_for(c),
        Down => Some(K::Down),
        Up => Some(K::Up),
        _ => None,
    };

    // An armed crossing waits for a repeat of the hunk step that armed it. Every other key drops
    // it, and still does its own work (`specs/tui.md`). The steps themselves settle their arm in
    // `step_hunk`, which is what makes the other direction disarm too.
    if !matches!(action, Some(K::NextHunk | K::PrevHunk)) {
        app.disarm_cross();
    }

    // The read-only PR tab: navigate the snapshot and open links; authoring actions are inert.
    if app.tab == crate::app::Tab::Pr {
        match (action, key.code) {
            (Some(K::Quit), _) => app.should_quit = true,
            (Some(K::Refresh), _) => app.pr_pending = true,
            (Some(K::TabChanges), _) => app.set_tab(crate::app::Tab::Changes)?,
            (Some(K::TabAllFiles), _) => app.set_tab(crate::app::Tab::AllFiles)?,
            (Some(K::OpenPr), _) => app.pr_open(),
            (Some(K::NavigatorPosition), _) => app.cycle_navigator_position(),
            (Some(K::NavigatorGrow), _) => app.resize_navigator(4),
            (Some(K::NavigatorShrink), _) => app.resize_navigator(-4),
            (Some(K::Down), _) => app.pr_move(1),
            (Some(K::Up), _) => app.pr_move(-1),
            (_, Tab) => app.toggle_focus(),
            (_, PageDown) if app.focus == Focus::Files => app.pr_scroll_nav(PAGE),
            (_, PageUp) if app.focus == Focus::Files => app.pr_scroll_nav(-PAGE),
            (_, PageDown) => app.pr_scroll_read(PAGE),
            (_, PageUp) => app.pr_scroll_read(-PAGE),
            _ => {}
        }
        return Ok(());
    }

    // The comments-list overlay acts through the same bindings and closes on `esc` and the
    // `comments` binding (`specs/tui.md`).
    if app.mode == Mode::List {
        match (action, key.code) {
            (Some(K::Comments), _) | (_, Esc) => app.close_list(),
            (Some(K::Down), _) => app.list_move(1),
            (Some(K::Up), _) => app.list_move(-1),
            (Some(K::Send), _) => app.export(&Agent),
            (Some(K::Copy), _) => app.export(&Clipboard),
            (Some(K::Edit), _) => app.start_edit(),
            (Some(K::Delete), _) => app.delete_comment(),
            _ => {}
        }
        return Ok(());
    }

    if let Some(action) = action {
        match action {
            K::Quit => app.should_quit = true,
            K::Refresh => app.reload()?,
            K::TabChanges => app.set_tab(crate::app::Tab::Changes)?,
            K::TabAllFiles => app.set_tab(crate::app::Tab::AllFiles)?,
            K::TabPr => app.set_tab(crate::app::Tab::Pr)?,
            K::Down => app.move_cursor(1)?,
            K::Up => app.move_cursor(-1)?,
            K::NextHunk => app.next_hunk(),
            K::PrevHunk => app.prev_hunk(),
            K::NextFile => app.next_file(),
            K::PrevFile => app.prev_file(),
            K::Wrap => app.toggle_wrap(),
            K::Preview => app.toggle_preview(),
            K::NavigatorPosition => app.cycle_navigator_position(),
            K::NavigatorGrow => app.resize_navigator(4),
            K::NavigatorShrink => app.resize_navigator(-4),
            K::ScopeUncommitted => app.set_scope(Scope::Uncommitted)?,
            K::ScopeBranch => app.set_scope(Scope::Branch)?,
            K::ScopeLastTurn => app.set_scope(Scope::LastTurn)?,
            K::Select => app.toggle_select(),
            K::Comment => app.start_comment(),
            // `edit`/`delete` act on the comment under the diff cursor, so they only fire with
            // the diff focused — otherwise `delete` would silently drop a comment under an
            // off-screen cursor. (The comments-list overlay targets the highlighted row instead.)
            K::Edit if app.focus == Focus::Diff => app.start_edit(),
            K::Delete if app.focus == Focus::Diff => app.delete_comment(),
            K::Send => app.export(&Agent),
            K::Copy => app.export(&Clipboard),
            K::NextComment => app.jump_comment(1),
            K::PrevComment => app.jump_comment(-1),
            K::Comments => app.open_list(),
            // `edit`/`delete` off the diff, and `open-pr` off the `PR` tab, are inert.
            K::Edit | K::Delete | K::OpenPr => {}
        }
        return Ok(());
    }

    match key.code {
        Tab => app.toggle_focus(),
        // Page and half-page keys move the focused pane's cursor (the view follows).
        Char('u') if ctrl => app.move_cursor(-HALF_PAGE)?,
        Char('d') if ctrl => app.move_cursor(HALF_PAGE)?,
        PageDown => app.move_cursor(PAGE)?,
        PageUp => app.move_cursor(-PAGE)?,
        // `←`/`→` expand/collapse the collapsible under the cursor — a directory in the file
        // list, a fold in the diff (expand-only); otherwise they scroll the diff sideways
        // (`scroll_h` is a no-op while wrapping, so it only acts when h-scroll is meaningful).
        Right if app.on_folder() => app.expand_dir(),
        Left if app.on_folder() => app.collapse_dir(),
        Right if app.on_fold() => {
            let heights = ui::diff_row_heights(app, area);
            app.expand_fold(&heights, ui::diff_viewport_height(area, app));
        }
        Right => app.scroll_h(8),
        Left => app.scroll_h(-8),
        // `esc` clears an in-progress line selection (the footer's `esc clear`).
        Esc => app.clear_selection(),
        _ => {}
    }
    Ok(())
}

/// Cancel pointer state whose coordinates belonged to the old terminal geometry.
fn handle_resize(app: &mut App) {
    app.cancel_divider_drag();
}

/// Map one mouse event onto `App`. Header hit-testing uses `keymap` — the keymap of the frame
/// on screen — so a config swap at the click boundary cannot shift the spans under the pointer.
/// Public for the dispatch tests, like [`handle_key`]; the event loop is the runtime caller.
pub fn handle_mouse(
    app: &mut App,
    m: MouseEvent,
    area: Rect,
    heights: &[usize],
    keymap: &Keymap,
) -> Result<()> {
    // A modal captures new mouse gestures, but a divider gesture cancelled by the key that
    // opened it still owns its remaining drag and mouse-up events.
    if app.composing() || app.mode == Mode::List {
        match m.kind {
            MouseEventKind::Drag(MouseButton::Left) if app.divider_drag_captured() => {
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left) if app.divider_drag_captured() => {
                app.finish_divider_drag();
            }
            _ => {}
        }
        return Ok(());
    }
    // A mouse gesture is one of the "any other input" that drops an armed crossing: the reviewer
    // who reaches for the mouse has left the file's edge behind (`specs/tui.md`). Pointer motion
    // is not a gesture — capture reports every move over the pane, and a pointer resting on the
    // sidebar would otherwise disarm the crossing without the reviewer touching anything.
    if !matches!(m.kind, MouseEventKind::Moved) {
        app.disarm_cross();
    }

    // Divider gestures are common to every tab. A cancelled drag remains consumed until its
    // mouse-up, so rotating or reconfiguring the layout cannot turn it into a line selection.
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) if ui::hit_divider(area, app, m.column, m.row) => {
            app.start_divider_drag();
            return Ok(());
        }
        MouseEventKind::Drag(MouseButton::Left) if app.divider_drag_active() => {
            let body = ui::body_rect(area);
            let (axis_len, offset) = if app.navigator_position.stacked() {
                (body.height, m.row.saturating_sub(body.y))
            } else {
                (body.width, m.column.saturating_sub(body.x))
            };
            app.drag_divider(axis_len, offset);
            return Ok(());
        }
        MouseEventKind::Drag(MouseButton::Left) if app.divider_drag_cancelled() => return Ok(()),
        MouseEventKind::Up(MouseButton::Left) if app.divider_drag_captured() => {
            app.finish_divider_drag();
            return Ok(());
        }
        _ => {}
    }

    // The read-only PR tab: click a tab or the open button, click a row to read it, and wheel
    // either pane without moving the selection.
    if app.tab == crate::app::Tab::Pr {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(url) = app.painted_link_at(m.column, m.row) {
                    app.focus = Focus::Diff;
                    // A link click resolves against the painted frame (specs/markdown.md).
                    app.open_link(&url);
                } else if let Some(ui::HeaderHit::Tab(tab)) =
                    ui::hit_header(area, app, keymap, m.column, m.row)
                {
                    app.set_tab(tab)?;
                } else if ui::hit_pr_open(area, app, m.column, m.row) {
                    app.pr_open();
                } else if ui::in_files_pane(area, app, m.column, m.row) {
                    app.focus = Focus::Files;
                    if let Some(i) = ui::pr_nav_hit(area, app, m.column, m.row) {
                        app.pr_select(i);
                    }
                } else if ui::in_diff_pane(area, app, m.column, m.row) {
                    app.focus = Focus::Diff;
                }
            }
            MouseEventKind::ScrollDown if ui::in_files_pane(area, app, m.column, m.row) => {
                app.pr_scroll_nav(3);
            }
            MouseEventKind::ScrollUp if ui::in_files_pane(area, app, m.column, m.row) => {
                app.pr_scroll_nav(-3);
            }
            MouseEventKind::ScrollDown => app.pr_scroll_read(3),
            MouseEventKind::ScrollUp => app.pr_scroll_read(-3),
            _ => {}
        }
        return Ok(());
    }
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(hit) = ui::hit_header(area, app, keymap, m.column, m.row) {
                match hit {
                    ui::HeaderHit::Tab(tab) => app.set_tab(tab)?,
                    ui::HeaderHit::Scope => app.set_scope(app.scope.cycle())?,
                    ui::HeaderHit::Send => app.export(&Agent),
                }
            } else if let Some(i) =
                ui::hit_file(area, app, m.column, m.row, app.file_rows.len(), app.file_scroll)
            {
                app.select_file(i)?;
            } else if let Some(url) = app.painted_link_at(m.column, m.row) {
                // A link click resolves against the painted frame (specs/markdown.md).
                app.open_link(&url);
            } else if app.preview_active() {
                // The preview has no cursor or selection: a click in the pane only
                // focuses it. The pane-rect test, not the source-row hit test — the
                // rendered preview can be taller than the source has rows.
                if ui::in_diff_pane(area, app, m.column, m.row) {
                    app.focus = Focus::Diff;
                }
            } else if let Some(i) =
                ui::hit_diff(area, app, m.column, m.row, heights, app.diff_scroll)
            {
                app.focus = Focus::Diff;
                app.diff_cursor = i;
                app.select_anchor = None;
                // A click on a fold marker expands it, keeping the viewport still.
                app.expand_fold(heights, ui::diff_viewport_height(area, app));
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.preview_active() {
                // No drag-selection in the read-only preview.
            } else if let Some(i) =
                ui::hit_diff(area, app, m.column, m.row, heights, app.diff_scroll)
            {
                app.drag_select_to(i);
            }
        }
        // The wheel scrolls the viewport of whichever pane it is over — never the cursor, so
        // a comment is never anchored to a wheeled-past line. Horizontal scroll is
        // keyboard-only (`←`/`→`), since multiplexers don't reliably deliver h-wheel events.
        MouseEventKind::ScrollDown if ui::in_files_pane(area, app, m.column, m.row) => {
            app.wheel_files(3);
        }
        MouseEventKind::ScrollUp if ui::in_files_pane(area, app, m.column, m.row) => {
            app.wheel_files(-3);
        }
        MouseEventKind::ScrollDown => app.wheel_diff(3),
        MouseEventKind::ScrollUp => app.wheel_diff(-3),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod refresh_tests {
    use super::{
        ActiveFetch, PaintedFrameSnapshot, PrCoordinator, PrEffect, PrRefresh, TaggedPr,
        apply_plugin_config_observation, apply_pr_probe_result, drain_pr_shutdown,
        handle_blocked_event, handle_resize, ready_app, schedule_poll_probe,
    };
    use crate::app::{App, Tab};
    use crate::config::{Config, plugin_config_in};
    use crate::forge::{PrFetchInput, PrView};
    use crate::git::RepositoryIdentity;
    use crate::model::Scope;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    fn input(head: &str) -> PrFetchInput {
        input_with("github.com", "acme", "widgets", head, &["feature"])
    }

    fn input_with(
        host: &str,
        owner: &str,
        name: &str,
        head: &str,
        candidates: &[&str],
    ) -> PrFetchInput {
        PrFetchInput {
            repository: RepositoryIdentity::Repository(
                crate::git::RepoTarget::new(host, owner, name).unwrap(),
            ),
            head_oid: Some(head.to_string()),
            candidates: candidates.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    fn no_pr() -> PrView {
        PrView::NoPr
    }

    #[test]
    fn terminal_resize_cancels_the_active_divider_coordinates() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        app.start_divider_drag();

        handle_resize(&mut app);

        assert!(app.divider_drag_cancelled());
    }

    #[test]
    fn a_probe_that_changes_pr_rows_requires_a_repaint_before_input() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        let mut coordinator = PrCoordinator::new(true);
        coordinator.refresh.current_input = Some(input("old"));

        assert!(apply_pr_probe_result(&mut app, &mut coordinator, Ok(input("new")), 0));
        assert!(matches!(app.pr, PrView::Pending));
        assert!(!apply_pr_probe_result(&mut app, &mut coordinator, Ok(input("new")), 0));
    }

    #[test]
    fn a_config_layout_change_invalidates_the_painted_frame_before_input() {
        let repo = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let path = config_dir.path().join("config.toml");
        std::fs::write(&path, "navigator_position = \"right\"\n").unwrap();
        let cfg = Config::parse([repo.path().display().to_string()]);
        let mut app = App::new(repo.path().to_path_buf(), Scope::Uncommitted, None);
        app.set_plugin_config(plugin_config_in(config_dir.path()).unwrap());
        let painted = PaintedFrameSnapshot::capture(&app);
        let (tx, _rx) = mpsc::channel();
        let mut epoch = 0;
        let mut recovery_inflight = false;

        std::fs::write(&path, "navigator_position = \"bottom\"\n").unwrap();
        assert!(apply_plugin_config_observation(
            &mut app,
            &cfg,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            plugin_config_in(config_dir.path()),
        ));
        assert!(!painted.still_current(&app), "input must wait for the bottom layout to paint");

        let repainted = PaintedFrameSnapshot::capture(&app);
        assert!(apply_plugin_config_observation(
            &mut app,
            &cfg,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            plugin_config_in(config_dir.path()),
        ));
        assert!(repainted.still_current(&app), "an unchanged observation keeps the frame valid");
    }

    #[test]
    fn blocked_frames_ignore_normal_events_but_keep_quit_and_capture_cleanup() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        app.mode = crate::app::Mode::Composing { editing: None };
        app.input = "draft".to_string();
        app.start_divider_drag();
        app.set_config_error("invalid config".to_string());
        assert!(app.divider_drag_cancelled());

        handle_blocked_event(&mut app, &Event::Paste(" hidden paste".to_string()));
        handle_blocked_event(
            &mut app,
            &Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert_eq!(app.input, "draft");
        assert!(!app.should_quit);

        handle_blocked_event(
            &mut app,
            &Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(!app.divider_drag_cancelled());

        handle_blocked_event(&mut app, &Event::Key(KeyEvent::from(KeyCode::Char('q'))));
        assert!(app.should_quit);
    }

    #[test]
    fn superseded_completion_never_applies_and_schedules_the_new_generation() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        assert!(refresh.observed(a.clone(), 0, true).is_none());
        let (old_generation, old_input) = refresh.take_fetch().unwrap();

        refresh.trigger();
        refresh.completed(
            TaggedPr {
                generation: old_generation,
                config_epoch: 0,
                input: old_input,
                view: no_pr(),
            },
            0,
            true,
        );
        assert!(refresh.observed(a, 0, true).is_none());

        let (new_generation, _) = refresh.take_fetch().unwrap();
        assert_ne!(new_generation, old_generation);
    }

    #[test]
    fn changed_input_clears_instead_of_applying_a_completed_old_snapshot() {
        let a = input("a");
        let b = input("b");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0, true);
        let (generation, old_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 0, input: old_input, view: no_pr() },
            0,
            true,
        );

        assert!(matches!(refresh.observed(b, 0, true), Some(PrEffect::Clear)));
    }

    #[test]
    fn a_trigger_during_completion_verification_supersedes_before_apply() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0, true);
        let (old_generation, fetch_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr {
                generation: old_generation,
                config_epoch: 0,
                input: fetch_input,
                view: no_pr(),
            },
            0,
            true,
        );

        refresh.trigger();
        assert!(refresh.observed(a, 0, true).is_none(), "the completed snapshot never applies");
        let (new_generation, _) = refresh.take_fetch().unwrap();
        assert_ne!(new_generation, old_generation);
    }

    #[test]
    fn repository_target_and_candidates_are_refresh_boundaries() {
        let original = input("head");
        let changes = [
            input_with("github.com", "upstream", "widgets", "head", &["feature"]),
            input_with("github.enterprise.test", "acme", "widgets", "head", &["feature"]),
            input_with("github.com", "acme", "other-widgets", "head", &["feature"]),
            input_with("github.com", "acme", "widgets", "head", &["published", "feature"]),
        ];

        for changed in changes {
            let mut refresh = PrRefresh::new(true);
            refresh.observed(original.clone(), 0, true);
            let (generation, old_input) = refresh.take_fetch().unwrap();
            refresh.completed(
                TaggedPr { generation, config_epoch: 0, input: old_input, view: no_pr() },
                0,
                true,
            );

            assert!(matches!(refresh.observed(changed.clone(), 0, true), Some(PrEffect::Clear)));
            assert_eq!(refresh.take_fetch().map(|(_, input)| input), Some(changed));
        }
    }

    #[test]
    fn off_tab_repository_change_clears_and_defers_its_replacement() {
        let old = input("head");
        let changed = input_with("github.com", "upstream", "widgets", "head", &["feature"]);
        let mut refresh = PrRefresh::new(true);
        refresh.observed(old, 0, true);
        let _ = refresh.take_fetch().unwrap();

        assert!(matches!(refresh.observed(changed.clone(), 0, false), Some(PrEffect::Clear)));
        assert!(refresh.take_fetch().is_none());

        refresh.trigger();
        assert!(refresh.observed(changed.clone(), 0, true).is_none());
        assert_eq!(refresh.take_fetch().map(|(_, input)| input), Some(changed));
    }

    #[test]
    fn stale_config_epoch_and_off_tab_input_change_do_not_start_or_apply_work() {
        let a = input("a");
        let b = input("b");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 1, true);
        let (generation, old_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 1, input: old_input, view: no_pr() },
            2,
            false,
        );
        assert!(matches!(refresh.observed(b, 2, false), Some(PrEffect::Clear)));
        assert!(refresh.take_fetch().is_none());
    }

    #[test]
    fn matching_completion_applies_only_after_the_verification_probe() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 3, true);
        let (generation, fetch_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 3, input: fetch_input, view: no_pr() },
            3,
            true,
        );

        assert!(matches!(refresh.observed(a, 3, true), Some(PrEffect::Apply(PrView::NoPr))));
    }

    #[test]
    fn a_failed_verification_probe_discards_the_hidden_completion() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0, true);
        let (generation, fetch_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 0, input: fetch_input, view: no_pr() },
            0,
            true,
        );

        refresh.probe_failed(false);
        assert!(refresh.take_fetch().is_none());
        refresh.trigger();
        assert!(refresh.observed(a, 0, true).is_none());
        assert!(refresh.take_fetch().is_some(), "the next refresh starts a fresh GitHub fetch");
    }

    #[test]
    fn a_failed_probe_cannot_fetch_the_previous_repository() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0, true);
        let _ = refresh.take_fetch().unwrap();
        refresh.trigger();

        refresh.probe_failed(false);
        assert!(refresh.take_fetch().is_none());

        refresh.trigger();
        assert!(refresh.observed(a, 0, true).is_none());
        assert!(refresh.take_fetch().is_some());
    }

    #[test]
    fn a_failed_probe_keeps_a_refresh_that_was_queued_behind_it() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0, true);
        let _ = refresh.take_fetch().unwrap();

        refresh.trigger();
        refresh.probe_failed(true);
        assert!(refresh.observed(a, 0, true).is_none());
        assert!(refresh.take_fetch().is_some(), "the queued refresh still starts GitHub work");
    }

    #[test]
    fn an_unproven_repository_replaces_the_snapshot_and_blocks_a_stale_fetch() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        let mut coordinator = PrCoordinator::new(true);
        coordinator.refresh.observed(input("head"), 0, true);
        let _ = coordinator.refresh.take_fetch().unwrap();
        coordinator.refresh.trigger();
        coordinator.probe_pending = false;
        app.apply_pr(no_pr());

        assert!(apply_pr_probe_result(
            &mut app,
            &mut coordinator,
            Err(crate::forge::PrInputError::TargetRead("repository read failed".to_string())),
            0,
        ));
        assert_eq!(app.pr, PrView::GitError("repository read failed".to_string()));
        assert!(coordinator.wait_started.is_none());
        assert!(coordinator.refresh.take_fetch().is_none());
    }

    #[test]
    fn a_local_read_failure_preserves_a_snapshot_for_the_same_repository() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        let snapshot = no_pr();
        app.apply_pr(snapshot.clone());
        let mut coordinator = PrCoordinator::new(true);
        coordinator.refresh.observed(input("head"), 0, true);
        coordinator.probe_pending = false;

        assert!(apply_pr_probe_result(
            &mut app,
            &mut coordinator,
            Err(crate::forge::PrInputError::BranchState {
                target: crate::git::RepoTarget::new("github.com", "acme", "widgets").unwrap(),
                message: "HEAD read failed".to_string(),
            }),
            0,
        ));

        assert_eq!(app.pr, snapshot);
        assert!(app.pr_notice().is_some_and(|notice| notice.starts_with("Git read failed")));
        assert!(coordinator.refresh.take_fetch().is_none());
    }

    #[test]
    fn a_local_read_failure_for_a_different_repository_replaces_the_snapshot() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        app.apply_pr(no_pr());
        let mut coordinator = PrCoordinator::new(true);
        coordinator.refresh.observed(input("head"), 0, true);
        coordinator.probe_pending = false;

        assert!(apply_pr_probe_result(
            &mut app,
            &mut coordinator,
            Err(crate::forge::PrInputError::BranchState {
                target: crate::git::RepoTarget::new("github.com", "other", "widgets").unwrap(),
                message: "HEAD read failed".to_string(),
            }),
            0,
        ));

        assert_eq!(app.pr, PrView::GitError("HEAD read failed".to_string()));
        assert!(app.pr_notice().is_none());
    }

    #[test]
    fn config_change_off_the_pr_tab_does_not_schedule_a_fetch() {
        let mut refresh = PrRefresh::new(false);
        refresh.observed(input("a"), 0, false);
        refresh.config_changed(false);
        assert!(refresh.take_fetch().is_none());
    }

    #[test]
    fn normal_poll_schedules_repository_probe_only_on_the_pr_tab() {
        let mut coordinator = PrCoordinator::new(false);
        schedule_poll_probe(&mut coordinator, Tab::Changes);
        assert!(!coordinator.probe_pending);

        schedule_poll_probe(&mut coordinator, Tab::Pr);
        assert!(coordinator.probe_pending);
    }

    #[test]
    fn cancelling_a_fetch_retains_real_worker_ownership_until_completion() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut coordinator = PrCoordinator::new(true);
        coordinator.active_fetch = Some(ActiveFetch { tag: (7, 3), cancelled: cancelled.clone() });

        coordinator.config_changed(true);

        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(coordinator.active_fetch_tag(), Some((7, 3)));
    }

    #[test]
    fn repository_probe_waits_for_the_active_fetch_to_exit() {
        let mut coordinator = PrCoordinator::new(true);
        coordinator.active_fetch =
            Some(ActiveFetch { tag: (7, 3), cancelled: Arc::new(AtomicBool::new(false)) });

        assert!(!coordinator.can_start_probe(true));
        coordinator.active_fetch = None;
        assert!(coordinator.can_start_probe(true));
    }

    #[test]
    fn a_config_change_retains_probe_ownership_until_completion() {
        let mut coordinator = PrCoordinator::new(true);
        coordinator.active_probe_epoch = Some(3);

        coordinator.config_changed(true);

        assert_eq!(coordinator.active_probe_epoch, Some(3));
        assert!(coordinator.probe_pending);
    }

    #[test]
    fn shutdown_cancels_and_drains_matching_pr_workers() {
        let fetch_cancelled = Arc::new(AtomicBool::new(false));
        let mut coordinator = PrCoordinator::new(true);
        coordinator.active_probe_epoch = Some(3);
        coordinator.active_fetch =
            Some(ActiveFetch { tag: (7, 3), cancelled: fetch_cancelled.clone() });
        let (probe_tx, probe_rx) = mpsc::channel();
        let (fetch_tx, fetch_rx) = mpsc::channel();
        probe_tx.send((3, Ok(input("probe")))).unwrap();
        fetch_tx
            .send(TaggedPr {
                generation: 7,
                config_epoch: 3,
                input: input("fetch"),
                view: PrView::Pending,
            })
            .unwrap();

        drain_pr_shutdown(&mut coordinator, &probe_rx, &fetch_rx);

        assert!(fetch_cancelled.load(Ordering::Acquire));
        assert!(coordinator.active_probe_epoch.is_none());
        assert!(coordinator.active_fetch.is_none());
    }

    #[test]
    fn shell_only_config_changes_do_not_invalidate_runtime_work() {
        let repo = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        std::fs::write(config_dir.path().join("config.toml"), "auto_open = false\n").unwrap();
        let cfg = Config::parse([repo.path().display().to_string()]);
        let mut app = App::new(repo.path().to_path_buf(), Scope::Uncommitted, None);
        let (tx, _rx) = mpsc::channel();
        let mut epoch = 0;
        let mut recovery_inflight = false;

        assert!(apply_plugin_config_observation(
            &mut app,
            &cfg,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            plugin_config_in(config_dir.path()),
        ));
        assert_eq!(epoch, 0);
        assert!(!app.plugin_config().unwrap().auto_open());

        std::fs::write(config_dir.path().join("config.toml"), "base_branches = [\"develop\"]\n")
            .unwrap();
        assert!(apply_plugin_config_observation(
            &mut app,
            &cfg,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            plugin_config_in(config_dir.path()),
        ));
        assert_eq!(epoch, 1);
    }

    #[test]
    fn default_scope_seeds_a_fresh_sidebar_and_a_reread_never_switches_it() {
        let repo = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let path = config_dir.path().join("config.toml");
        std::fs::write(&path, "default_scope = \"branch\"\n").unwrap();
        let cfg = Config::parse([repo.path().display().to_string()]);
        let mut app = ready_app(&cfg, plugin_config_in(config_dir.path()).unwrap());
        assert_eq!(app.scope, Scope::Branch, "startup seeds the configured scope");

        // The user switches in-session; a reread with a different default must not move it.
        app.set_scope(Scope::LastTurn).unwrap();
        std::fs::write(&path, "default_scope = \"uncommitted\"\n").unwrap();
        let (tx, _rx) = mpsc::channel();
        let mut epoch = 0;
        let mut recovery_inflight = false;
        assert!(apply_plugin_config_observation(
            &mut app,
            &cfg,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            plugin_config_in(config_dir.path()),
        ));
        assert_eq!(app.scope, Scope::LastTurn, "a reread never switches the active scope");
        assert_eq!(epoch, 0, "a default_scope change invalidates no running work");
    }

    #[test]
    fn invalid_then_valid_observation_blocks_and_recovers_through_a_fresh_worker() {
        let repo = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let path = config_dir.path().join("config.toml");
        std::fs::write(&path, "unknown = true\n").unwrap();
        let cfg = Config::parse([repo.path().display().to_string()]);
        let mut app = App::new(repo.path().to_path_buf(), Scope::Uncommitted, None);
        let (tx, rx) = mpsc::channel();
        let mut epoch = 0;
        let mut recovery_inflight = false;

        assert!(!apply_plugin_config_observation(
            &mut app,
            &cfg,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            plugin_config_in(config_dir.path()),
        ));
        assert!(app.plugin_config().is_none());
        assert!(app.config_error().unwrap().contains("unknown key"));

        std::fs::write(&path, "theme = \"gruvbox\"\n").unwrap();
        assert!(!apply_plugin_config_observation(
            &mut app,
            &cfg,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            plugin_config_in(config_dir.path()),
        ));
        let (recovery_epoch, target, recovered) =
            rx.recv_timeout(Duration::from_secs(5)).expect("recovery worker");
        assert_eq!(recovery_epoch, epoch);
        assert_eq!(target.theme(), "gruvbox");
        assert_eq!(recovered.plugin_config().unwrap().theme(), "gruvbox");
    }
}
