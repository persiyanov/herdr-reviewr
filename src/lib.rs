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
pub mod cli;
pub mod comments;
pub mod config;
pub mod diff;
pub mod export;
pub mod file_list;
pub mod forge;
pub mod git;
pub mod herdr;
pub mod highlight;
#[macro_use]
pub mod log;
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
    terminal.draw(|f| ui::render(f, &app))?;
    if initial_config.is_ok()
        && let Err(e) = app.reload()
    {
        logln!("startup reload failed: {e:#}");
        app.status = format!("load failed: {e}");
    }
    let result = event_loop(&mut terminal, &mut app, &cfg);
    if kbd {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

/// Build a fresh working sidebar only after the plugin configuration has validated.
fn ready_app(cfg: &Config, plugin_config: PluginConfig) -> App {
    // A non-repo path is not an error — the sidebar opens to an empty state and starts showing
    // changes if the directory becomes a repo (specs/herdr-host.md).
    let repo = git::toplevel(&cfg.repo).unwrap_or_else(|| cfg.repo.clone());
    logln!("start repo={} poll={:?} base={:?}", repo.display(), cfg.poll, cfg.base);
    let mut app = App::new(repo, Scope::Uncommitted, cfg.base.clone());
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
const PR_POLL: Duration = Duration::from_secs(60);
const PR_LOADING_DELAY: Duration = Duration::from_millis(150);

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

#[derive(Debug)]
struct PrCoordinator {
    refresh: PrRefresh,
    wait_started: Option<Instant>,
    active_probe_epoch: Option<u64>,
    active_fetch: Option<ActiveFetch>,
    discard_probe_result: bool,
    probe_pending: bool,
}

#[derive(Debug)]
struct ActiveFetch {
    tag: (u64, u64),
    cancelled: Arc<AtomicBool>,
}

impl PrCoordinator {
    fn new(ready: bool) -> Self {
        Self {
            refresh: PrRefresh::new(ready),
            wait_started: ready.then(Instant::now),
            active_probe_epoch: None,
            active_fetch: None,
            discard_probe_result: false,
            probe_pending: ready,
        }
    }

    fn stop(&mut self) {
        self.refresh.invalidate();
        self.wait_started = None;
        self.cancel_fetch();
        self.discard_probe_result = false;
        self.probe_pending = false;
    }

    fn recover(&mut self) {
        self.refresh.invalidate();
        self.refresh.trigger();
        self.wait_started = Some(Instant::now());
        self.cancel_fetch();
        self.discard_probe_result = false;
        self.probe_pending = true;
    }

    fn config_changed(&mut self, active: bool) {
        self.cancel_fetch();
        self.discard_probe_result = false;
        self.refresh.config_changed(active);
        self.probe_pending = true;
    }

    fn cancel_fetch(&self) {
        if let Some(active) = &self.active_fetch {
            active.cancelled.store(true, Ordering::Release);
        }
    }

    fn active_fetch_tag(&self) -> Option<(u64, u64)> {
        self.active_fetch.as_ref().map(|active| active.tag)
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

    fn completed(&mut self, completion: TaggedPr, epoch: u64, active: bool) -> bool {
        if completion.generation == self.generation && completion.config_epoch == epoch {
            self.pending = Some(completion);
            true
        } else {
            self.pending = None;
            self.fetch_needed = self.fetch_needed || active;
            false
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
fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, cfg: &Config) -> Result<()> {
    let poll = cfg.poll;
    let mut last_poll = Instant::now();
    let mut last_pr_poll = Instant::now();
    // Local input probes and GitHub reads run on workers. A completed fetch is applied only after
    // a fresh probe proves its complete input still matches (`specs/forge-host.md`).
    let (probe_tx, probe_rx) = mpsc::channel::<(u64, Result<crate::forge::PrFetchInput, String>)>();
    let (recovery_tx, recovery_rx) = mpsc::channel::<(u64, PluginConfig, App)>();
    let mut recovery_inflight = false;
    let (pr_tx, pr_rx) = mpsc::channel::<TaggedPr>();
    let mut pr = PrCoordinator::new(app.plugin_config().is_some());
    let mut config_epoch = 0_u64;
    let mut validate_before_draw = true;
    let mut status_at = Instant::now();
    let mut last_status = String::new();
    // Fetch the PR snapshot as soon as the panel opens, not on first switching to the tab, so the
    // tab is already populated when the user gets there (specs/forge-host.md).
    app.pr_pending = false;
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

        // Revalidate after synchronous work before its result may paint. Worker completions,
        // input dispatch, and the ordinary poll each validate at their own boundary below, so a
        // slow `gh` request does not turn the 100 ms completion wake-up into repeated TOML I/O.
        if validate_before_draw {
            reconcile_plugin_config(
                app,
                cfg,
                &mut config_epoch,
                &recovery_tx,
                &mut recovery_inflight,
                &mut pr,
            );
            validate_before_draw = false;
        }
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
        app.bound_diff_scroll(&heights, effective);
        let file_vp = ui::file_viewport_height(area, app.list_pct);
        if std::mem::take(&mut app.reveal_files) {
            app.reveal_file_cursor(file_vp);
        }
        app.bound_file_scroll(file_vp);
        terminal.draw(|f| ui::render(f, app))?;
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
                let accepted =
                    pr.refresh.completed(completion, config_epoch, app.tab == crate::app::Tab::Pr);
                if accepted && pr.active_probe_epoch.is_some() {
                    pr.discard_probe_result = true;
                }
                pr.probe_pending = true;
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
            if !config_gate.pr_unchanged() || epoch != config_epoch {
                if config_gate == ConfigGate::Unchanged && epoch != config_epoch {
                    pr.config_changed(app.tab == crate::app::Tab::Pr);
                }
            } else if pr.discard_probe_result {
                pr.discard_probe_result = false;
                pr.probe_pending = true;
            } else {
                match result {
                    Err(error) => {
                        app.apply_pr(crate::forge::PrView::Error(error));
                        pr.wait_started = None;
                    }
                    Ok(input) => {
                        // Record the origin regardless of the effect below, so the PR/MR noun
                        // (`app.rs`) tracks the worktree even on a same-input reobserve.
                        app.set_fetch_input(input.clone());
                        match pr.refresh.observed(
                            input,
                            config_epoch,
                            app.tab == crate::app::Tab::Pr,
                        ) {
                            Some(PrEffect::Clear) => {
                                app.clear_pr();
                                pr.wait_started =
                                    (app.tab == crate::app::Tab::Pr).then(Instant::now);
                            }
                            Some(PrEffect::Apply(view)) => {
                                app.apply_pr(view);
                                pr.wait_started = None;
                            }
                            None => {}
                        }
                    }
                }
            }
        }

        // Record refresh triggers even while a fetch is in flight; the generation makes the old
        // completion superseded and a new fetch starts after it exits.
        let fallback_poll = app.tab == crate::app::Tab::Pr && last_pr_poll.elapsed() >= PR_POLL;
        if app.pr_pending || fallback_poll {
            app.pr_pending = false;
            last_pr_poll = Instant::now();
            pr.refresh.trigger();
            pr.wait_started.get_or_insert_with(Instant::now);
            pr.probe_pending = true;
        }

        if pr.probe_pending && pr.active_probe_epoch.is_none() && app.plugin_config().is_some() {
            pr.probe_pending = false;
            let (tx, repo, base, plugin_config, epoch) = (
                probe_tx.clone(),
                app.repo.clone(),
                app.base.clone(),
                app.plugin_config().expect("config checked above").clone(),
                config_epoch,
            );
            pr.active_probe_epoch = Some(epoch);
            thread::spawn(move || {
                let input = crate::forge::fetch_input(&repo, base.as_deref(), &plugin_config);
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
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if app.config_error().is_some() {
                        if k.code == KeyCode::Char('q') {
                            app.should_quit = true;
                        }
                        continue;
                    }
                    let config_gate = reconcile_plugin_config(
                        app,
                        cfg,
                        &mut config_epoch,
                        &recovery_tx,
                        &mut recovery_inflight,
                        &mut pr,
                    );
                    if !config_gate.ready() {
                        continue;
                    }
                    if let Err(e) = handle_key(app, k, area) {
                        app.status = format!("error: {e}");
                    }
                    validate_before_draw = true;
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
                    let config_gate = reconcile_plugin_config(
                        app,
                        cfg,
                        &mut config_epoch,
                        &recovery_tx,
                        &mut recovery_inflight,
                        &mut pr,
                    );
                    if !config_gate.ready() {
                        continue;
                    }
                    // Reuse this frame's `area` and `heights` (computed above for the scroll
                    // settle) so a drag-select doesn't re-measure the whole diff per motion.
                    if let Err(e) = handle_mouse(app, m, area, &heights) {
                        app.status = format!("error: {e}");
                    }
                    validate_before_draw = true;
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
                    let config_gate = reconcile_plugin_config(
                        app,
                        cfg,
                        &mut config_epoch,
                        &recovery_tx,
                        &mut recovery_inflight,
                        &mut pr,
                    );
                    if !config_gate.ready() {
                        continue;
                    }
                    app.input_paste(&text);
                    validate_before_draw = true;
                    logln!("paste {} chars -> composing={}", text.len(), app.composing());
                }
                _ => {}
            }
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
            pr.probe_pending = true;
            // Pick up an external comment-store write (the CLI, an agent) before this poll's
            // reload, same reasoning as the turn baseline below: cheap to check every tick, and
            // a stale comments list should never survive an extra frame past its signature change.
            app.check_comment_store();
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
            validate_before_draw = true;
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

fn handle_key(app: &mut App, key: KeyEvent, area: Rect) -> Result<()> {
    use KeyCode::{
        Backspace, Char, Delete, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Tab,
        Up,
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // A keypress ends any in-progress divider drag, so opening a modal mid-drag (which makes
    // the mouse handler ignore the releasing Up) can't strand `resizing` true.
    app.resizing = false;

    if app.composing() {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let alt_or_shift = key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        let word = alt || ctrl; // word-jump on Alt/Ctrl + arrow (terminal-dependent)
        // The wrapped width of the box, for vertical (wrapped-row) caret movement.
        let cw = ui::composer_content_width(ui::diff_inner_width(area, app.list_pct));
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

    // The read-only PR tab: navigate the snapshot and open links; authoring keys are inert.
    if app.tab == crate::app::Tab::Pr {
        match key.code {
            Char('q') => app.should_quit = true,
            Char('r') => app.pr_pending = true,
            Char('1') => app.set_tab(crate::app::Tab::Changes)?,
            Char('2') => app.set_tab(crate::app::Tab::AllFiles)?,
            Char('o') => app.pr_open(),
            Char('j') | Down => app.pr_move(1),
            Char('k') | Up => app.pr_move(-1),
            // The navigator is short; the read pane is what overflows, so the page keys scroll it.
            PageDown => app.pr_scroll_read(PAGE),
            PageUp => app.pr_scroll_read(-PAGE),
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
        (Char('u'), true) => app.move_cursor(-HALF_PAGE)?,
        (Char('d'), true) => app.move_cursor(HALF_PAGE)?,
        (Char('q'), _) => app.should_quit = true,
        (Char('r'), _) => app.reload()?,
        // `1` / `2` / `3` switch tabs (provisional; the keymap is an Open Decision in tui.md).
        (Char('1'), _) => app.set_tab(crate::app::Tab::Changes)?,
        (Char('2'), _) => app.set_tab(crate::app::Tab::AllFiles)?,
        (Char('3'), _) => app.set_tab(crate::app::Tab::Pr)?,
        (Tab, _) => app.toggle_focus(),
        (Char('j') | Down, _) => app.move_cursor(1)?,
        (Char('k') | Up, _) => app.move_cursor(-1)?,
        // Page keys move the focused pane's cursor.
        (PageDown, _) => app.move_cursor(PAGE)?,
        (PageUp, _) => app.move_cursor(-PAGE)?,
        (Char('w'), _) => app.toggle_wrap(),
        // `]` widens the file list, `[` narrows it (widening the diff).
        (Char(']'), _) => app.resize_list(4),
        (Char('['), _) => app.resize_list(-4),
        // `←`/`→` expand/collapse the collapsible under the cursor — a directory in the file
        // list, a fold in the diff (expand-only); otherwise they scroll the diff sideways
        // (`scroll_h` is a no-op while wrapping, so it only acts when h-scroll is meaningful).
        (Right, _) if app.on_folder() => app.expand_dir(),
        (Left, _) if app.on_folder() => app.collapse_dir(),
        (Right, _) if app.on_fold() => {
            let heights = ui::diff_row_heights(app, area);
            app.expand_fold(&heights, ui::diff_viewport_height(area, app.list_pct));
        }
        (Right, _) => app.scroll_h(8),
        (Left, _) => app.scroll_h(-8),
        (Char('u'), false) => app.set_scope(Scope::Uncommitted)?,
        (Char('b'), false) => app.set_scope(Scope::Branch)?,
        (Char('t'), false) => app.set_scope(Scope::LastTurn)?,
        (Char('v'), _) => app.toggle_select(),
        (Char('c'), _) => app.start_comment(),
        // `e`/`d` act on the comment under the diff cursor, so they only fire with the diff
        // focused — otherwise `d` would silently delete a comment under an off-screen cursor.
        // (The comments-list overlay has its own `e`/`d`, which target the highlighted row.)
        (Char('e'), _) if app.focus == Focus::Diff => app.start_edit(),
        (Char('d'), false) if app.focus == Focus::Diff => app.delete_comment(),
        (Char('s' | 'S'), _) => app.export(&Agent),
        (Char('y' | 'Y'), _) => app.export(&Clipboard),
        (Char('n'), _) => app.jump_comment(1),
        (Char('N'), _) => app.jump_comment(-1),
        (Char('l'), _) => app.open_list(),
        // `esc` clears an in-progress line selection (the footer's `esc clear`).
        (Esc, _) => app.clear_selection(),
        _ => {}
    }
    Ok(())
}

fn handle_mouse(app: &mut App, m: MouseEvent, area: Rect, heights: &[usize]) -> Result<()> {
    // A modal (the comment composer or the comments-list overlay) captures the screen and is
    // keyboard-driven, so the mouse is inert while one is open — otherwise clicks and the
    // wheel would drive the panes drawn underneath it.
    if app.composing() || app.mode == Mode::List {
        return Ok(());
    }
    // The read-only PR tab: click a tab or the open button, click a row to read it, wheel the
    // navigator (right) to move, wheel the read pane (left) to scroll.
    if app.tab == crate::app::Tab::Pr {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(ui::HeaderHit::Tab(tab)) = ui::hit_header(area, app, m.column, m.row) {
                    app.set_tab(tab)?;
                } else if ui::hit_pr_open(area, app, m.column, m.row) {
                    app.pr_open();
                } else if let Some(i) = ui::pr_nav_hit(area, app, m.column, m.row) {
                    app.pr_select(i);
                }
            }
            MouseEventKind::ScrollDown
                if ui::in_files_pane(area, app.list_pct, m.column, m.row) =>
            {
                app.pr_move(3);
            }
            MouseEventKind::ScrollUp if ui::in_files_pane(area, app.list_pct, m.column, m.row) => {
                app.pr_move(-3);
            }
            MouseEventKind::ScrollDown => app.pr_scroll_read(3),
            MouseEventKind::ScrollUp => app.pr_scroll_read(-3),
            _ => {}
        }
        return Ok(());
    }
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // The divider is checked first: a grab there starts a resize, not a selection.
            if ui::hit_divider(area, app.list_pct, m.column, m.row) {
                app.resizing = true;
            } else if let Some(hit) = ui::hit_header(area, app, m.column, m.row) {
                match hit {
                    ui::HeaderHit::Tab(tab) => app.set_tab(tab)?,
                    ui::HeaderHit::Scope => app.set_scope(app.scope.cycle())?,
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
            } else if let Some(i) =
                ui::hit_diff(area, app.list_pct, m.column, m.row, heights, app.diff_scroll)
            {
                app.focus = Focus::Diff;
                app.diff_cursor = i;
                app.select_anchor = None;
                // A click on a fold marker expands it, keeping the viewport still.
                app.expand_fold(heights, ui::diff_viewport_height(area, app.list_pct));
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.resizing {
                let body = ui::body_rect(area);
                app.drag_divider(body.width, m.column.saturating_sub(body.x));
            } else if let Some(i) =
                ui::hit_diff(area, app.list_pct, m.column, m.row, heights, app.diff_scroll)
            {
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

#[cfg(test)]
mod refresh_tests {
    use super::{
        ActiveFetch, PrCoordinator, PrEffect, PrRefresh, TaggedPr, apply_plugin_config_observation,
    };
    use crate::app::App;
    use crate::config::{Config, plugin_config_in};
    use crate::forge::{PrFetchInput, PrView};
    use crate::git::OriginIdentity;
    use crate::model::Scope;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    fn input(head: &str) -> PrFetchInput {
        PrFetchInput {
            origin: OriginIdentity::Missing,
            branch: Some("feature".to_string()),
            head_oid: Some(head.to_string()),
            candidates: vec!["feature".to_string()],
            base: None,
            base_branches: vec!["main".to_string()],
        }
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
                view: PrView::NoPr(vec![]),
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
            TaggedPr { generation, config_epoch: 0, input: old_input, view: PrView::NoPr(vec![]) },
            0,
            true,
        );

        assert!(matches!(refresh.observed(b, 0, true), Some(PrEffect::Clear)));
    }

    #[test]
    fn stale_config_epoch_and_off_tab_input_change_do_not_start_or_apply_work() {
        let a = input("a");
        let b = input("b");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 1, true);
        let (generation, old_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 1, input: old_input, view: PrView::NoPr(vec![]) },
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
            TaggedPr {
                generation,
                config_epoch: 3,
                input: fetch_input,
                view: PrView::NoPr(vec!["feature".to_string()]),
            },
            3,
            true,
        );

        assert!(matches!(refresh.observed(a, 3, true), Some(PrEffect::Apply(PrView::NoPr(_)))));
    }

    #[test]
    fn a_failed_verification_probe_keeps_the_completion_for_the_next_probe() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0, true);
        let (generation, fetch_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr {
                generation,
                config_epoch: 0,
                input: fetch_input,
                view: PrView::NoPr(vec![]),
            },
            0,
            true,
        );

        assert!(refresh.take_fetch().is_none());
        assert!(matches!(refresh.observed(a, 0, true), Some(PrEffect::Apply(PrView::NoPr(_)))));
    }

    #[test]
    fn config_change_off_the_pr_tab_does_not_schedule_a_fetch() {
        let mut refresh = PrRefresh::new(false);
        refresh.observed(input("a"), 0, false);
        refresh.config_changed(false);
        assert!(refresh.take_fetch().is_none());
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
