//! herdr-reviewr — a herdr-native review pane.
//!
//! Browse an agent's changes (uncommitted / branch), leave line-range comments,
//! and send them back to the agent (or the clipboard) — entirely in a herdr pane.
//!
//! This crate is split into a thin binary (`src/main.rs`) and this library so the
//! interaction logic in [`app`] stays terminal-free and unit-testable. This module
//! owns the terminal lifecycle and the event loop; it maps input events onto
//! [`app::App`] methods and renders with [`ui`].

pub mod app;
pub mod azure_devops;
pub mod browser;
pub mod config;
pub mod diff;
pub mod export;
pub mod file_list;
pub mod forge;
pub mod git;
pub mod gitlab;
pub mod herdr;
pub mod highlight;
pub mod keymap;
#[macro_use]
pub mod log;
pub mod markdown;
pub mod model;
pub mod proc;
pub mod search;
pub mod selection;
pub mod snippet;
pub mod theme;
pub mod turn;
pub mod ui;
pub mod world;

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
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::layout::Rect;

use crate::app::{App, Focus, Mode};
use crate::config::{Config, PluginConfig};
use crate::export::Clipboard;
use crate::keymap::Keymap;
use crate::model::Scope;

/// The status-line note a slow config-dir lookup paints before its answer swaps the frame,
/// and retracts if the lookup resolves nothing (`policies/ux-responsiveness.md`).
const RESOLVING_NOTE: &str = "resolving plugin config…";

/// Entry point: parse config, set up the terminal, run the loop, restore.
pub fn run() -> Result<()> {
    let mut cfg = Config::from_env();
    log::init();
    // The config directory resolves once, at startup; every later read rereads only the
    // file inside it (`specs/config.md`). Only the environment names it here: the CLI
    // fallback is a herdr subprocess, so it waits until after the first paint below —
    // a wedged herdr must never hold the paint (issue #4).
    cfg.plugin_config_dir = config::resolve_config_dir(|| None);
    let mut initial_config = config::plugin_config(cfg.plugin_config_dir.as_deref());
    let mut app = app_for(&cfg, &initial_config);

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
    // (issue #4). Paint the empty frame first; then the initial load, non-fatal — an error
    // opens the pane with the reason in the status line, the same contract as a failed poll
    // refresh.
    if let Err(error) = terminal.draw(|f| ui::render(f, &app)) {
        restore_terminal(kbd);
        return Err(error.into());
    }
    // The cosmetic pane label, stamped after the first paint and cleared on a normal exit
    // (`specs/herdr-host.md` Pane identity). Display only: identity is the process.
    herdr::label_pane();
    // The CLI half of config-dir resolution, on a painted pane: with no environment
    // directory, ask herdr and rebuild from the directory it names. Nothing user-held
    // exists yet — the rebuild happens before the first load — and a wedged herdr
    // degrades this pane to the defaults instead of holding herdr's blank grid
    // (issue #4, `specs/config.md` The file). A slow answer paints its note
    // first, so the config swap is never a silent stale-then-swap; a fast one shows
    // nothing (`policies/ux-responsiveness.md`).
    let cli_dir = cfg
        .plugin_config_dir
        .is_none()
        .then(|| {
            herdr::plugin_config_dir_with(|| {
                app.status = RESOLVING_NOTE.into();
                let _ = terminal.draw(|f| ui::render(f, &app));
            })
        })
        .flatten();
    if let Some(dir) = cli_dir {
        cfg.plugin_config_dir = Some(dir.into());
        initial_config = config::plugin_config(cfg.plugin_config_dir.as_deref());
        app = app_for(&cfg, &initial_config);
        if let Err(error) = terminal.draw(|f| ui::render(f, &app)) {
            restore_terminal(kbd);
            herdr::clear_pane_label();
            return Err(error.into());
        }
    }
    // A slow lookup that then resolved nothing leaves its note behind; retract it.
    if app.status == RESOLVING_NOTE {
        app.status.clear();
        let _ = terminal.draw(|f| ui::render(f, &app));
    }
    if initial_config.is_ok()
        && let Err(e) = app.reload()
    {
        logln!("startup reload failed: {e:#}");
        app.status = format!("load failed: {e}");
    }
    let result = event_loop(&mut terminal, &mut app, &cfg, kbd);
    herdr::clear_pane_label();
    result
}

/// Leave the alternate screen and release terminal input modes before any bounded worker drain.
fn restore_terminal(kbd: bool) {
    if kbd {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
}

/// Service one `edit-file` request (`specs/input.md`): suspend the TUI exactly as
/// [`restore_terminal`] tears it down, run `$VISUAL`/`$EDITOR` on the file — `+LINE`
/// first when a line is known, the vi-family convention vim, nvim, nano, hx, and
/// kakoune share — then rebuild the terminal and repaint. The editor inherits the pane's
/// environment; its PATH resolves through [`proc::command`], since a herdr pane may strip
/// it. Afterward the file may have changed on disk, so a world refresh reconciles the
/// diff in place (`overview.md` Continuity) and the status names what happened.
fn run_editor(terminal: &mut DefaultTerminal, app: &mut App, kbd: bool) -> Result<()> {
    use crate::app::EditorTarget;
    let Some(EditorTarget { path, line }) = app.editor_request.take() else {
        return Ok(());
    };
    // The whole value is the command: `$EDITOR="code -w"` runs `code` with `-w`. A bare
    // name resolves on the host PATH; anything else must be runnable as spelled.
    let Some(program) = std::env::var_os("VISUAL")
        .or_else(|| std::env::var_os("EDITOR"))
        .map(|value| value.to_string_lossy().into_owned())
    else {
        app.status = "set $EDITOR (or $VISUAL) to edit files".into();
        return Ok(());
    };
    let mut words = program.split_whitespace();
    let Some(bin) = words.next() else {
        app.status = "set $EDITOR (or $VISUAL) to edit files".into();
        return Ok(());
    };
    let mut cmd = proc::command(bin);
    cmd.args(words);
    if let Some(no) = line {
        cmd.arg(format!("+{no}"));
    }
    cmd.arg(app.repo.join(&path));

    // Suspend: leave the alternate screen and release every input mode, so the editor
    // owns the terminal cleanly.
    let _ = terminal.clear();
    if kbd {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ =
        execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, DisableBracketedPaste);
    let _ = disable_raw_mode();

    let launched = cmd.status();

    // Resume: rebuild the exact mode stack the loop expects, force a full repaint, and
    // drop any keystrokes the editor's exit left unread.
    let _ = enable_raw_mode();
    if kbd {
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste);
    let _ = terminal.clear();
    while event::poll(Duration::ZERO)? {
        let _ = event::read();
    }
    terminal.draw(|f| ui::render(f, app))?;

    match launched {
        Ok(status) if status.success() => {
            app.status = format!("edited {path}");
            app.request_world_refresh(false, false);
            app.refresh_commanded = true;
        }
        Ok(status) => app.status = format!("editor exited with {status}"),
        Err(e) => app.status = format!("editor failed: {e}"),
    }
    Ok(())
}

/// The reviewed repository, resolved to its git top level. Every `App` goes through this,
/// blocked or ready, so the app and the worker's `TurnHost` hold the same spelling: they key
/// the baseline ref off it independently, and turn membership compares resolved top levels
/// against it (`specs/herdr-host.md`).
///
/// A non-repo path is not an error — the pane opens to an empty state and starts showing
/// changes if the directory becomes a repo.
fn repo_root(cfg: &Config) -> std::path::PathBuf {
    git::toplevel(&cfg.repo).unwrap_or_else(|| cfg.repo.clone())
}

/// The startup app for one config snapshot: ready on `Ok`, blocked with the error on
/// `Err`. Both the env-resolved build and the post-paint CLI-resolved rebuild go through
/// here, so the two paths cannot drift.
fn app_for(cfg: &Config, initial_config: &Result<PluginConfig, config::PluginConfigError>) -> App {
    match initial_config {
        Ok(plugin_config) => ready_app(cfg, plugin_config.clone()),
        Err(error) => {
            let mut app = App::blocked(repo_root(cfg), Scope::Uncommitted, cfg.base.clone());
            app.set_config_error(error.to_string());
            app
        }
    }
}

/// Build a fresh working reviewr pane only after the plugin configuration has validated.
fn ready_app(cfg: &Config, plugin_config: PluginConfig) -> App {
    let repo = repo_root(cfg);
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

/// The exit deadline: stillness this long after the pointer's last event sat on the pane's
/// edge completes the live gesture — herdr routes mouse by pointer position, so a release
/// past the pane never arrives. Long enough that a pause while border-scrolling survives,
/// short enough that an overshoot release's copy beats the paste (specs/text-selection.md).
/// Its own constant, never the `--poll` cadence: the two measure unrelated things.
const EXIT_DEADLINE: Duration = Duration::from_secs(1);

/// While the `PR` tab is active, refetch the forge at least this often — a fallback for
/// forge-side changes with no local signal (a reviewer's comment). Local pushes and forge PR
/// actions refresh sooner, on the worktree's turn-end, so this cadence is the slow safety net
/// (specs/forge-host.md).
const PR_POLL: Duration = Duration::from_mins(1);

/// How long an in-flight PR fetch may run before a refresh trigger stops waiting on it.
/// Generous against slow forges, short enough that the fallback poll recovers a wedged
/// fetch within one cycle.
const FETCH_HANG: Duration = Duration::from_mins(1);
/// How long an ambient refresh must stay in flight before the tab-strip glyph shows —
/// routine refreshes stay invisible; a commanded one (`r`) shows immediately (specs/tui.md).
const INDICATOR_DELAY: Duration = Duration::from_millis(200);
/// Once lit, the glyph holds at least this long, so a fast landing still reads.
const INDICATOR_MIN_SHOW: Duration = Duration::from_millis(300);
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
    /// The view's identity changed (repository or candidate set): the snapshot may describe
    /// the wrong pull request, so it blanks while the replacement fetches.
    Clear,
    /// Only `HEAD` moved: the same pull request gained newer commits. The snapshot stays on
    /// screen — stale, never wrong — while the replacement fetches behind it (forge-host.md).
    Refetch,
    Apply(crate::forge::PrView),
}

/// Owns PR refresh convergence. Generations supersede mid-flight triggers, config epochs reject
/// work from another validated snapshot, and a fresh input probe must match before a completion
/// can paint. An off-tab input change clears stale state but defers its replacement fetch.
#[derive(Debug)]
struct PrRefresh {
    /// The last attached checked-out branch name. A change is an identity change — a new
    /// branch is a new PR story — while a transient detach keeps it, so a rebase never
    /// reads as a switch (`specs/forge-host.md` Refresh).
    last_branch: Option<String>,
    generation: u64,
    current_input: Option<crate::forge::PrFetchInput>,
    pending: Option<TaggedPr>,
    fetch_needed: bool,
    /// An ambient trigger rode the in-flight fetch; one fresh fetch follows the ridden
    /// result so a remote change that read predates still paints promptly.
    trailing: bool,
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
    /// When the fetch dispatched — the hang bound in `request_refresh` measures from it.
    started: Instant,
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

    /// Start a refresh, or ride the one already underway. A commanded refresh cancels the
    /// in-flight fetch and starts fresh. The ambient triggers — tab entry, a turn end, the
    /// fallback timer — join a fetch in flight or a completion awaiting its probe, and arm
    /// one trailing fetch behind it: the ridden result still paints (nothing waits on a
    /// repeated fetch), and the trailing fetch supersedes it with a read the trigger is
    /// guaranteed to predate — a remote-only change is never lost to the fallback timer
    /// (specs/forge-host.md). A fetch past the hang bound is abandoned instead of joined,
    /// so a reader that died without sending a completion can never wedge the tab — its
    /// zombie completion, if one ever lands, fails the tag check.
    fn request_refresh(&mut self, kind: crate::app::RefreshKind) {
        self.wait_started.get_or_insert_with(Instant::now);
        let hung =
            self.active_fetch.as_ref().is_some_and(|active| active.started.elapsed() >= FETCH_HANG);
        if kind == crate::app::RefreshKind::Ambient
            && !hung
            && (self.active_fetch.is_some() || self.refresh.pending.is_some())
        {
            self.refresh.trailing = true;
            return;
        }
        self.cancel_fetch();
        if hung {
            self.active_fetch = None;
        }
        self.refresh.trigger();
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

/// Cancel the active forge fetch and briefly drain matching probe/fetch completions before exit.
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
    Changed { pr_changed: bool },
}

impl ConfigGate {
    fn ready(self) -> bool {
        self != Self::Blocked
    }

    fn pr_unchanged(self) -> bool {
        !matches!(self, Self::Blocked | Self::Changed { pr_changed: true, .. })
    }
}

/// The hold gate: a resolution that found nothing keeps the painted story while the
/// pinned `HEAD` still is or contains the shown PR's head commit
/// (`specs/forge-host.md` Refresh). `contains(pin, oid)` answers that ancestry question;
/// its failure is a retryable branch-state Git error, never proof of absence
/// (`specs/forge-host.md` failure table — a same-target failure preserves the snapshot).
fn hold_gate(
    view: crate::forge::PrView,
    held: Option<&str>,
    pin: Option<&str>,
    contains: impl FnOnce(&str, &str) -> Result<bool, crate::git::GitFail>,
) -> crate::forge::PrView {
    if matches!(view, crate::forge::PrView::NoPr)
        && let (Some(held), Some(pin)) = (held, pin)
        && !held.is_empty()
    {
        return match contains(pin, held) {
            Ok(true) => crate::forge::PrView::Held,
            Ok(false) => view,
            Err(error) => crate::forge::PrView::GitError(error.0),
        };
    }
    view
}

impl PrRefresh {
    fn new(ready: bool) -> Self {
        Self {
            generation: 1,
            current_input: None,
            pending: None,
            fetch_needed: ready,
            trailing: false,
            last_branch: None,
        }
    }

    fn trigger(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.fetch_needed = true;
        // The fresh fetch reads the current remote, satisfying any armed trailing fetch.
        self.trailing = false;
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.current_input = None;
        self.pending = None;
        self.fetch_needed = false;
        self.trailing = false;
        self.last_branch = None;
    }

    fn config_changed(&mut self, active: bool) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.fetch_needed = active;
        self.trailing = false;
    }

    fn completed(&mut self, completion: TaggedPr, epoch: u64, active: bool) {
        if completion.generation == self.generation && completion.config_epoch == epoch {
            self.pending = Some(completion);
        } else {
            self.pending = None;
            self.fetch_needed = self.fetch_needed || active;
        }
    }

    fn observed(&mut self, input: crate::forge::PrFetchInput, epoch: u64) -> Option<PrEffect> {
        // Two tiers (forge-host.md). Identity is whose story the tab tells: the resolved
        // repository target, the origin, and the checked-out branch — a change there
        // clears, because the snapshot belongs to another branch's story. Everything
        // locally derived — the pinned `HEAD` and base, the branch's forge names — moves
        // on a mere commit or push, so it is freshness: the snapshot stays painted while
        // the replacement fetches behind it, stale, never wrong (`overview.md`
        // Continuity). Both tiers start that fetch at once, on or off the tab, so
        // entering the tab finds fresh work already underway.
        let branch =
            (!input.local.detached).then(|| input.local.names.first().cloned().unwrap_or_default());
        let branch_changed = matches!((&self.last_branch, &branch),
            (Some(previous), Some(current)) if previous != current);
        if let Some(branch) = branch {
            self.last_branch = Some(branch);
        }
        let Some(previous) = self.current_input.as_ref() else {
            self.current_input = Some(input.clone());
            return self.take_pending(&input, epoch);
        };
        let identity_changed = previous.repository != input.repository
            || previous.origin_repository != input.origin_repository
            || branch_changed;
        let freshness_changed = previous.local != input.local;
        if identity_changed || freshness_changed {
            self.generation = self.generation.wrapping_add(1);
            self.pending = None;
            self.current_input = Some(input);
            self.fetch_needed = true;
            return Some(if identity_changed { PrEffect::Clear } else { PrEffect::Refetch });
        }
        self.current_input = Some(input.clone());
        self.take_pending(&input, epoch)
    }

    /// Apply the pending completion when it exactly matches the just-verified input, or ask
    /// for a fresh fetch when it doesn't. The shared tail of [`Self::observed`].
    fn take_pending(&mut self, input: &crate::forge::PrFetchInput, epoch: u64) -> Option<PrEffect> {
        if let Some(completion) = self.pending.take() {
            if completion.generation == self.generation
                && completion.config_epoch == epoch
                && completion.input == *input
            {
                // A ridden trigger's trailing fetch dispatches behind the paint.
                self.fetch_needed = self.trailing;
                return Some(PrEffect::Apply(completion.view));
            }
            self.fetch_needed = true;
        }
        None
    }

    fn probe_failed(&mut self, retry_pending: bool) {
        self.pending = None;
        // An armed trailing fetch survives the failed probe as a plain fetch request, so a
        // ridden trigger is never silently lost to the fallback timer.
        self.fetch_needed = retry_pending || std::mem::take(&mut self.trailing);
    }

    fn take_fetch(&mut self) -> Option<(u64, crate::forge::PrFetchInput)> {
        if !self.fetch_needed {
            return None;
        }
        let input = self.current_input.clone()?;
        self.fetch_needed = false;
        // Any dispatched fetch reads the current remote — the trailing request is served.
        self.trailing = false;
        Some((self.generation, input))
    }
}

/// Land one world completion. The worker's baseline syncs and a turn end schedules the PR
/// refetch regardless of the tag; the snapshot reconciles only when the completion carries
/// the live generation and its input still matches the view — a mismatched snapshot is
/// discarded whole and a fresh refresh queued (specs/tui.md). Returns whether the
/// completion matched the live generation — the caller clears the in-flight marker on
/// `true`.
pub fn land_world_completion(
    app: &mut App,
    completion: crate::world::WorldCompletion,
    generation: u64,
) -> bool {
    app.sync_turn_baseline(completion.input.turn_baseline.clone());
    if let Some(turn) = completion.turn.as_ref() {
        app.sync_agents_present(turn.agents_present);
        if turn.ended {
            // One fetch per turn, on any tab: the turn may have pushed or merged, and
            // entering the tab then finds fresh work already underway (forge-host.md).
            app.request_pr_refresh(crate::app::RefreshKind::Ambient);
        }
    }
    if completion.generation != generation {
        // A superseding job carries reveal=false, so a superseded switch's reveal would
        // die here; re-arm it to ride the next dispatch instead (specs/tui.md).
        if completion.reveal {
            app.request_world_refresh(false, true);
        }
        return false;
    }
    match completion.snapshot {
        Some(Ok(snapshot))
            if app.config_error().is_none() && app.world_input() == completion.input =>
        {
            app.reconcile_world(snapshot);
            if completion.reveal {
                // The switch frame revealed the stashed cursor; the landing may have
                // re-anchored it, so settle and reveal again.
                app.settle_tab_entry();
                app.reveal_files = true;
            }
        }
        // The view moved on while the build ran: discard whole, refresh again, keeping
        // an undelivered reveal alive.
        Some(Ok(_)) => app.request_world_refresh(false, completion.reveal),
        // A failed refresh reports and keeps the stale frame — the same contract as a
        // failed poll (specs/tui.md).
        Some(Err(e)) => app.status = format!("refresh failed: {e}"),
        None => {}
    }
    true
}

/// Land one search completion. A stale generation is discarded whole — a result set
/// paints only while it matches the query as typed (specs/search.md). Returns whether the
/// completion matched the live generation, mirroring [`land_world_completion`].
pub fn land_search_completion(
    app: &mut App,
    completion: crate::search::SearchCompletion,
    generation: u64,
) -> bool {
    if completion.generation != generation {
        return false;
    }
    app.apply_search_completion(completion);
    true
}

/// Whether a file tab's in-flight refresh shows the tab-strip glyph: past the delay, and
/// only for a job that builds a snapshot — a sample-only job never lights it (specs/tui.md).
fn world_indicator(inflight: Option<(Duration, bool)>) -> bool {
    inflight.is_some_and(|(elapsed, builds)| builds && elapsed >= INDICATOR_DELAY)
}

/// Whether a lit glyph may go dark: only once the minimum display has passed, so the
/// acknowledgment is perceptible rather than a two-frame blink (specs/tui.md).
fn glyph_clears(lit_for: Duration) -> bool {
    lit_for >= INDICATOR_MIN_SHOW
}

/// The tight wake while a worker owes a completion, so its landing paints near the
/// build's own speed — shared by the world and search workers.
const WORKER_TIGHT_WAKE: Duration = Duration::from_millis(15);

/// The wake while a world job is in flight: tight for a building job so its landing paints
/// near the build's own speed, the fetch cadence for a sample-only one.
fn world_wake(builds: bool) -> Duration {
    if builds { WORKER_TIGHT_WAKE } else { Duration::from_millis(100) }
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
    // The exit signature's two halves: the last mouse event's arrival, and whether that
    // event sat on the pane's edge — only both together let the exit deadline complete a
    // gesture whose release was lost past the pane (specs/text-selection.md).
    let mut last_mouse = Instant::now();
    let mut mouse_exited = false;
    let mut last_pr_poll = Instant::now();
    // Local input probes and forge reads run on workers. A completed fetch is applied only after
    // a fresh probe proves its complete input still matches (`specs/forge-host.md`).
    let (probe_tx, probe_rx) =
        mpsc::channel::<(u64, Result<crate::forge::PrFetchInput, crate::forge::PrInputError>)>();
    let (recovery_tx, recovery_rx) = mpsc::channel::<(u64, PluginConfig, App)>();
    let mut recovery_inflight = false;
    let (pr_tx, pr_rx) = mpsc::channel::<TaggedPr>();
    let mut pr = PrCoordinator::new(app.plugin_config().is_some());
    // The world worker owns every refresh build and the turn tracker; the loop sends
    // input-tagged jobs and reconciles the completions (specs/tui.md).
    let (world_tx, world_job_rx) = mpsc::channel::<crate::world::WorldJob>();
    let (world_res_tx, world_rx) = mpsc::channel::<crate::world::WorldCompletion>();
    let _world_worker = crate::world::spawn(
        crate::world::TurnHost::open(app.repo.clone()),
        world_job_rx,
        world_res_tx,
    );
    let mut world_generation = 0_u64;
    let mut world_inflight: Option<(Instant, bool)> = None;
    // The search worker spawns on the first overlay open, so a session that never
    // searches never pays for the engine's index (specs/search.md).
    let mut search_worker: Option<(
        mpsc::Sender<crate::search::SearchJob>,
        mpsc::Receiver<crate::search::SearchCompletion>,
    )> = None;
    let mut search_generation = 0_u64;
    let mut search_inflight = false;
    // When the tab-strip glyph turned on — the minimum-display clock (specs/tui.md).
    let mut glyph_since: Option<Instant> = None;
    let mut config_epoch = 0_u64;
    let mut status_at = Instant::now();
    let mut last_status = String::new();
    // Fetch the PR snapshot as soon as the panel opens, not on first switching to the tab, so the
    // tab is already populated when the user gets there (specs/forge-host.md).
    app.pr_pending = None;
    let result: Result<()> = (|| {
        while !app.should_quit {
            if let Ok((epoch, target, mut recovered)) = recovery_rx.try_recv() {
                recovery_inflight = false;
                if epoch == config_epoch {
                    match config::plugin_config(cfg.plugin_config_dir.as_deref()) {
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

            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            // Every frame starts from one complete validated config snapshot. Input below uses the
            // keymap and geometry this draw paints; later observations continue before another input.
            reconcile_plugin_config(
                app,
                cfg,
                area,
                &mut config_epoch,
                &recovery_tx,
                &mut recovery_inflight,
                &mut pr,
            );
            if pr.wait_started.is_some_and(|started| started.elapsed() >= INDICATOR_DELAY) {
                app.set_pr_refreshing(true);
                pr.wait_started = None;
            }
            // The tab-strip refresh glyph (specs/tui.md): a commanded refresh lights it
            // immediately, an ambient one past the appear delay, each tab only for its own
            // refresh. Once lit it holds a minimum, so a fast landing still reads.
            if std::mem::take(&mut app.refresh_commanded) {
                glyph_since.get_or_insert_with(Instant::now);
            }
            let glyph_due = if app.tab == crate::app::Tab::Pr {
                app.pr_refreshing()
            } else {
                world_indicator(world_inflight.map(|(started, builds)| (started.elapsed(), builds)))
            };
            let mut glyph_wake = None;
            if glyph_due {
                glyph_since.get_or_insert_with(Instant::now);
            } else if let Some(lit) = glyph_since {
                if glyph_clears(lit.elapsed()) {
                    glyph_since = None;
                } else {
                    // Wake at the hold boundary, so the glyph goes dark on time when idle.
                    glyph_wake = Some(INDICATOR_MIN_SHOW.saturating_sub(lit.elapsed()));
                }
            }
            app.refresh_indicator = glyph_since.is_some();
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
            // Rebuild the search preview only once input has settled: with input still queued
            // the build defers, so a pick sweep never waits on it. `build_search_preview` is
            // idempotent — it rebuilds only when the preview no longer matches the pick
            // (specs/search.md Preview).
            if app.mode == crate::app::Mode::Search && !event::poll(Duration::ZERO)? {
                app.build_search_preview();
            }
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
            // While the navigator is hidden its viewport is zero, and a reveal computed
            // there would zero the kept scroll — it stays pending for the show frame.
            if !app.navigator_hidden_here() && std::mem::take(&mut app.reveal_files) {
                app.reveal_file_cursor(file_vp);
            }
            app.bound_file_scroll(file_vp);
            let painted_frame = PaintedFrameSnapshot::capture(app);
            terminal.draw(|f| ui::render(f, app))?;

            // A world completion reconciles into the view only while the view it described is
            // still current; the worker's baseline is authoritative either way (specs/tui.md).
            // A navigator drag holds the drain — the input-tagged channel is the queue, no
            // stored deferral — and the pass that finds the gesture gone drains it to empty,
            // landing only the live generation (specs/text-selection.md).
            if !app.gates_world_drain() {
                let mut landed = false;
                while let Ok(completion) = world_rx.try_recv() {
                    if land_world_completion(app, completion, world_generation) {
                        world_inflight = None;
                    }
                    landed = true;
                }
                if landed {
                    continue;
                }
            }

            // A search completion paints only while it matches the query as typed: a stale
            // generation is discarded whole (specs/search.md).
            if let Some((_, rx)) = &search_worker
                && let Ok(completion) = rx.try_recv()
            {
                if land_search_completion(app, completion, search_generation) {
                    // A warming engine answers `indexing…` and re-runs by itself, so
                    // the tight wake stays on until real results land.
                    search_inflight = app
                        .search
                        .as_ref()
                        .is_some_and(|s| s.phase == crate::app::SearchPhase::Indexing);
                }
                // Repaint at once, like a world landing — without this the results sit
                // computed but unpainted until the next wake (policies/ux-responsiveness.md).
                continue;
            }
            // A closed overlay owes no landing: without this, a still-warming engine's
            // periodic `indexing…` completions would re-arm the tight wake after `esc`
            // and spin the loop until the cold scan finishes.
            if app.search.is_none() {
                search_inflight = false;
            }

            // Dispatch the queued query after the frame above painted, so typing paints at
            // input speed and the results land behind it (specs/search.md).
            if std::mem::take(&mut app.search_dirty)
                && app.mode == crate::app::Mode::Search
                && app.config_error().is_none()
            {
                let (tx, _) = search_worker.get_or_insert_with(|| {
                    let (job_tx, job_rx) = mpsc::channel();
                    let (res_tx, res_rx) = mpsc::channel();
                    crate::search::spawn(
                        app.repo.clone(),
                        crate::search::cache_dir(),
                        job_rx,
                        res_tx,
                    );
                    (job_tx, res_rx)
                });
                search_generation = search_generation.wrapping_add(1);
                let query = app.search.as_ref().map(|s| s.query.clone()).unwrap_or_default();
                search_inflight = tx
                    .send(crate::search::SearchJob::Query { generation: search_generation, query })
                    .is_ok();
                if !search_inflight
                    && let Some(s) = app.search.as_mut()
                    && !matches!(s.phase, crate::app::SearchPhase::Error(_))
                {
                    // A dead worker's first, specific error stays up; only a phase that
                    // never saw one gets the generic message.
                    s.phase = crate::app::SearchPhase::Error("search worker unavailable".into());
                    // Drop the last preview, like a failed completion, so no stale file shows
                    // under the error (specs/search.md).
                    s.preview = None;
                }
            }
            if let Some(path) = app.search_track.take()
                && let Some((tx, _)) = &search_worker
            {
                let _ = tx.send(crate::search::SearchJob::Track { path });
            }

            // Dispatch the queued refresh after the frame above painted, so a switch stays
            // instant and the fresh state lands behind it (specs/tui.md).
            if app.world_request.is_some() && app.config_error().is_none() {
                let request = app.world_request.take().expect("checked above");
                world_generation = world_generation.wrapping_add(1);
                let job = crate::world::WorldJob {
                    generation: world_generation,
                    input: app.world_input(),
                    sample_turn: request.sample_turn,
                    reveal: request.reveal,
                };
                // A sample-only job (the `PR` tab's poll) builds no snapshot: it neither
                // lights the file tabs' glyph nor deserves the tight landing wake.
                let builds = job.input.tab.is_file_tab();
                world_inflight = if world_tx.send(job).is_ok() {
                    Some((Instant::now(), builds))
                } else {
                    // A dead worker must not pin the in-flight marker (and its glyph and
                    // tight wake) for the rest of the session.
                    app.status = "refresh worker unavailable".to_string();
                    None
                };
            }

            // Record user and fallback refreshes before consuming worker results. A commanded
            // trigger that arrived during completion verification supersedes that completion
            // before it can paint, while the generation still coalesces repeated triggers into
            // one fresh fetch. Ambient triggers ride in-flight work and arm a trailing fetch
            // (`request_refresh`).
            let fallback_poll = app.tab == crate::app::Tab::Pr && last_pr_poll.elapsed() >= PR_POLL;
            let refresh =
                app.pr_pending.take().or(fallback_poll.then_some(crate::app::RefreshKind::Ambient));
            if let Some(kind) = refresh {
                last_pr_poll = Instant::now();
                pr.request_refresh(kind);
            }

            // A fetch completion waits for a fresh local-input probe before it may paint.
            // A gesture over the `PR` tab holds both PR drains — the coordinator's channels
            // are the queue, and its input tags discard whatever went stale meanwhile
            // (specs/text-selection.md).
            if !app.gates_pr_drain()
                && let Ok(completion) = pr_rx.try_recv()
            {
                let tag = (completion.generation, completion.config_epoch);
                if pr.active_fetch_tag() != Some(tag) {
                    continue;
                }
                pr.active_fetch = None;
                let config_gate = reconcile_plugin_config(
                    app,
                    cfg,
                    area,
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
            if !app.gates_pr_drain()
                && let Ok((epoch, result)) = probe_rx.try_recv()
            {
                if pr.active_probe_epoch != Some(epoch) {
                    continue;
                }
                pr.active_probe_epoch = None;
                let config_gate = reconcile_plugin_config(
                    app,
                    cfg,
                    area,
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
                pr.active_fetch = Some(ActiveFetch {
                    tag: (generation, epoch),
                    cancelled: cancelled.clone(),
                    started: Instant::now(),
                });
                let held = match &app.pr {
                    crate::forge::PrView::Pr(snapshot) => Some(snapshot.head_oid.clone()),
                    _ => None,
                };
                thread::spawn(move || {
                    let view = crate::forge::fetch_cancellable(&repo, &input, &cancelled);
                    // Checked here, on the worker, so the event loop never runs git.
                    // Re-proving each poll is the accepted cost of keeping the snapshot
                    // the only state.
                    let view = hold_gate(
                        view,
                        held.as_deref(),
                        input.local.head_oid.as_deref(),
                        |pin, oid| crate::git::contains_commit(&repo, pin, oid),
                    );
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
            // While a fetch is in flight, wake often so its result paints promptly when it
            // lands. A world refresh usually lands within tens of milliseconds, so its wake
            // is tighter — the landing paints near the build's own speed.
            if pr.active_fetch.is_some() || pr.active_probe_epoch.is_some() {
                timeout = timeout.min(Duration::from_millis(100));
            }
            if let Some((_, builds)) = world_inflight {
                timeout = timeout.min(world_wake(builds));
            }
            if search_inflight {
                timeout = timeout.min(WORKER_TIGHT_WAKE);
            }
            if let Some(wake) = glyph_wake {
                timeout = timeout.min(wake.max(Duration::from_millis(15)));
            }
            if let Some(started) = pr.wait_started {
                timeout = timeout.min(INDICATOR_DELAY.saturating_sub(started.elapsed()));
            }
            if app.config_error().is_none()
                && let Some(wait) = app.base_probe_wait()
            {
                timeout = timeout.min(wait);
            }
            // Wake at the exit deadline, so an abandoned gesture completes on time.
            if app.gesture_active() && mouse_exited {
                timeout = timeout.min(EXIT_DEADLINE.saturating_sub(last_mouse.elapsed()));
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
                        last_mouse = Instant::now();
                        // Reuse this frame's `area` and `heights` (computed above for the scroll
                        // settle) so a drag-select doesn't re-measure the whole diff per motion.
                        if let Err(e) =
                            handle_mouse(app, m, area, &heights, painted_frame.keymap(), &Clipboard)
                        {
                            app.status = format!("error: {e}");
                        }
                        mouse_exited = app.gesture_active() && pointer_at_pane_edge(m, area);
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
            if app.config_error().is_none() {
                app.tick_base_picker_probe();
            }
            // An `edit-file` press left a target: suspend, run `$EDITOR`, resume, and
            // repaint before the loop continues (`specs/input.md`).
            if app.editor_request.is_some() {
                run_editor(terminal, app, kbd)?;
            }
            if app.should_quit {
                break;
            }
            // Interior stillness is a held button (a release inside would have arrived), so
            // only the exit signature reaches this completion (specs/text-selection.md).
            if app.gesture_active() && mouse_exited && last_mouse.elapsed() >= EXIT_DEADLINE {
                complete_gesture(app, area, &Clipboard);
            }
            if last_poll.elapsed() >= poll {
                let config_gate = reconcile_plugin_config(
                    app,
                    cfg,
                    area,
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
                // The tick's refresh runs on the worker. The same request samples the agents
                // in the worktree there, so a turn promoted by the sample is visible to the
                // same request's changed-files build (specs/herdr-host.md). A turn end sets
                // the PR refetch when the completion lands.
                app.request_world_refresh(true, false);
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
            // The blocked screen's escape hatch stays modifier-agnostic: a stuck user's `q` quits
            // whatever the modifiers, exactly as before the keymap gained chords.
            if let KeyCode::Char(c) = k.code
                && keymap::default_keymap().action_for(keymap::Key::plain(c))
                    == Some(keymap::Action::Quit)
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
            // The display noun follows the resolved forge (`specs/forge-providers.md`). A forge
            // change is a target change, so `observed` clears any painted snapshot in the same
            // step and the noun can never caption another forge's PR. A target that resolves to
            // no forge takes the default noun rather than keeping the last one.
            app.pr_forge = match &input.repository {
                crate::git::RepositoryIdentity::Repository(target) => target.forge(),
                _ => crate::git::Forge::default(),
            };
            match pr.refresh.observed(input, config_epoch) {
                Some(PrEffect::Clear) => {
                    app.clear_pr();
                    pr.wait_started = (app.tab == crate::app::Tab::Pr).then(Instant::now);
                    true
                }
                Some(PrEffect::Refetch) => {
                    // The snapshot stays painted; only the refreshing indicator may appear
                    // once the wait crosses the loading delay. Nothing repaints now.
                    pr.wait_started = (app.tab == crate::app::Tab::Pr).then(Instant::now);
                    false
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
    area: Rect,
    config_epoch: &mut u64,
    recovery_tx: &mpsc::Sender<(u64, PluginConfig, App)>,
    recovery_inflight: &mut bool,
    pr: &mut PrCoordinator,
) -> ConfigGate {
    let previous = app.plugin_config().cloned();
    let observed = config::plugin_config(cfg.plugin_config_dir.as_deref());
    // A config layout or theme change reflows the frame under a live gesture, and the block
    // screen replaces its body: both complete the gesture's copy before the new frame
    // applies, so a world event never silently ends a visible selection
    // (specs/text-selection.md, "How a gesture ends").
    if app.gesture_active()
        && let Some(p) = &previous
        && config_ends_gesture(p, observed.as_ref().ok())
    {
        complete_gesture(app, area, &Clipboard);
    }
    if !apply_plugin_config_observation(
        app,
        cfg,
        config_epoch,
        recovery_tx,
        recovery_inflight,
        observed,
    ) {
        pr.stop();
        return ConfigGate::Blocked;
    }
    let current = app.plugin_config().expect("ready after successful observation");
    let Some(previous) = previous.filter(|previous| previous != current) else {
        return ConfigGate::Unchanged;
    };

    let pr_changed = previous.forge_hosts() != current.forge_hosts();
    if pr_changed {
        pr.config_changed(app.tab == crate::app::Tab::Pr);
    }
    if previous.theme() != current.theme() {
        // A theme change invalidates highlighted diffs. Rebuild before another input or
        // frame can mix states; `reload` preserves the frozen diff while composing.
        if let Err(error) = app.reload() {
            app.status = format!("config refresh failed: {error}");
        }
    }
    ConfigGate::Changed { pr_changed }
}

/// Whether a fresh config observation ends a live gesture — the end table's config row: a
/// layout or theme change reflows the frame, and a failed observation (`None`) blocks the
/// body (specs/text-selection.md).
#[must_use]
fn config_ends_gesture(previous: &PluginConfig, observed: Option<&PluginConfig>) -> bool {
    match observed {
        Some(c) => {
            previous.navigator_position() != c.navigator_position() || previous.theme() != c.theme()
        }
        None => true,
    }
}

/// Apply one complete config observation. Invalid state blocks work. Recovery loads a fresh
/// app on a tagged worker, then the event loop revalidates its target and carries authored
/// review state before swapping it in.
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
                if current.forge_hosts() != next.forge_hosts() {
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

/// Apply one readline-style editing key to the active field — the comment draft or the
/// search query — so the two input surfaces stay in lockstep, edited by one control set
/// (`specs/input.md`). The caller handles its own mode keys first and delegates the rest
/// here. `word` requests a word-wise horizontal move (Alt/Ctrl + arrow, terminal-dependent).
fn apply_text_edit(app: &mut App, code: KeyCode, ctrl: bool, alt: bool, word: bool) {
    use KeyCode::{Backspace, Char, Delete, End, Home, Left, Right};
    match code {
        Char('w') if ctrl => app.input_delete_word(),
        Char('a') if ctrl => app.caret_home(),
        Char('e') if ctrl => app.caret_end(),
        Char('u') if ctrl => app.input_kill_to_start(),
        Char('k') if ctrl => app.input_kill_to_end(),
        // Word-jump: `Alt+b`/`Alt+f` (readline; survives as ESC-prefixed, unlike modified
        // arrows, which many terminals/multiplexers strip) and modified arrows where they are
        // delivered. These precede the plain-character insert below.
        Char('b') if alt => app.caret_word_left(),
        Char('f') if alt => app.caret_word_right(),
        Left if word => app.caret_word_left(),
        Right if word => app.caret_word_right(),
        Left => app.caret_left(),
        Right => app.caret_right(),
        Home => app.caret_home(),
        End => app.caret_end(),
        Delete => app.input_delete_forward(),
        Backspace => app.input_backspace(),
        Char(c) if !ctrl => app.input_push(c),
        _ => {}
    }
}

/// Map one key press onto `App` through `keymap` — the keymap of the frame on screen, so a
/// stale hint never dispatches a different action than it advertised (`specs/config.md`).
/// Public for the dispatch tests; the event loop is the runtime caller.
pub fn handle_key(app: &mut App, key: KeyEvent, area: Rect, keymap: &Keymap) -> Result<()> {
    use crate::keymap::Action as K;
    use KeyCode::{Char, Down, Enter, Esc, Left, PageDown, PageUp, Right, Tab, Up};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // A keypress cancels the gesture but keeps consuming its drag events until mouse-up.
    app.cancel_divider_drag();
    // A reflow input cancels a live text or gutter gesture: nothing copies, and the key
    // still performs its own action (specs/text-selection.md). The hover `+` stays — it
    // recomputes each frame from the pointer's last reported cell (specs/diff-view.md).
    app.cancel_gesture();
    // Any keypress is the user doing something else: the settled highlight clears
    // (specs/text-selection.md).
    app.clear_settled_selection();

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
            // The box wraps, so `↑`/`↓` walk display rows here rather than editing text.
            Up => app.caret = ui::caret_vertical(&app.input, app.caret, cw, false),
            Down => app.caret = ui::caret_vertical(&app.input, app.caret, cw, true),
            code => apply_text_edit(app, code, ctrl, alt, word),
        }
        return Ok(());
    }

    // The search screen: the query edits with the comment editor's caret controls,
    // newlines excluded — every edit re-queries off the frame loop. `tab` flips the
    // mode; the page keys scroll the preview (specs/search.md).
    if app.mode == Mode::Search {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let word = alt || ctrl;
        match key.code {
            Esc => app.close_search(),
            Enter => app.search_open_pick()?,
            // `edit-file` on the picked result. The chord is the only way in — every
            // printable, `E` included, types into the query (`specs/search.md`) — so
            // here it outranks the shared caret-to-end control, which `End` still does.
            Char('e') if ctrl => app.request_edit_file(),
            Tab => app.search_flip(),
            PageDown => app.scroll_search_preview(PAGE),
            PageUp => app.scroll_search_preview(-PAGE),
            // The single-line query has no rows, so `↑`/`↓` (and `ctrl+n`/`p`) move the pick.
            Down => app.search_move(1),
            Up => app.search_move(-1),
            Char('n') if ctrl => app.search_move(1),
            Char('p') if ctrl => app.search_move(-1),
            code => apply_text_edit(app, code, ctrl, alt, word),
        }
        return Ok(());
    }

    // The in-file find band: printable keys edit the query, the steps move the cursor between
    // matches (`↑`/`↓` are the steps, so the single-line query has no vertical caret), `esc`
    // closes. Every other key is inert (specs/find-in-file.md).
    if app.mode == Mode::Find {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let word = alt || ctrl;
        match key.code {
            Esc => app.close_find(),
            Enter | Down => app.find_step(1),
            Up => app.find_step(-1),
            code => apply_text_edit(app, code, ctrl, alt, word),
        }
        return Ok(());
    }

    // The bound shortcuts dispatch through the frame's keymap: a character, a chord, or a
    // named key — the arrows and page keys are default bindings like any other
    // (`specs/input.md`). A key resolving to no action falls through to the fixed keys below
    // (`tab`, `esc`), which stay hardcoded per context (`specs/input.md`).
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let code = match key.code {
        Char(c) => Some(keymap::KeyCode::Char(c)),
        Left => Some(keymap::KeyCode::Left),
        Right => Some(keymap::KeyCode::Right),
        Up => Some(keymap::KeyCode::Up),
        Down => Some(keymap::KeyCode::Down),
        PageUp => Some(keymap::KeyCode::PageUp),
        PageDown => Some(keymap::KeyCode::PageDown),
        _ => None,
    };
    let action = code.and_then(|code| keymap.action_for(crate::keymap::Key { ctrl, alt, code }));

    // An armed crossing waits for a repeat of the hunk step that armed it. Every other key drops
    // it, and still does its own work (`specs/input.md`). The steps themselves settle their arm in
    // `step_hunk`, which is what makes the other direction disarm too. `esc` is exempt: the `esc`
    // ladder drops the crossing as its own explicit step, so a later layer is not also consumed.
    if !matches!(action, Some(K::NextHunk | K::PrevHunk)) && key.code != Esc {
        app.disarm_cross();
    }

    // The agent picker is strictly modal: `enter` sends, `esc` cancels, the movement bindings
    // and the literal digits move the highlight, and every other key is inert. `q` must not
    // quit here and `y` must not copy, or a habitual keystroke destroys or consumes the whole
    // review while the picker is up (`specs/input.md`). It is checked before the tab handlers,
    // like every other modal, so no tab can ever eat the modal's keys.
    if app.mode == Mode::Picker {
        // The send is irreversible and consumes every comment, so only the bare key fires it:
        // `alt+enter` and `shift+enter` mean "newline, not submit" in the comment editor the
        // reviewer was in moments ago, and that muscle memory must not send a review. The digits
        // are literal here, whatever `tab-changes` and its siblings are bound to, so a chord
        // carrying one must not move the highlight either. `esc` stays deliberately permissive:
        // cancelling is always safe, and no stray modifier should trap anyone in the modal.
        let bare = key.modifiers.is_empty();
        match (action, key.code) {
            (_, Esc) => app.close_picker(),
            (_, Enter) if bare => app.picker_pick(),
            // The digits outrank the movement bindings, so a reviewer who bound `down` to a
            // digit still gets the row that digit names (`specs/input.md`).
            (_, Char(c @ '1'..='9')) if bare => {
                app.picker_goto(c as usize - '1' as usize);
            }
            (Some(K::Down), _) => app.picker_move(1),
            (Some(K::Up), _) => app.picker_move(-1),
            _ => {}
        }
        return Ok(());
    }

    // The base picker: every printable narrows the filter — the bound shortcuts included, so
    // a branch named `qa` is typable — and the filter edits with the comment editor's
    // controls, like every other text field. `↑`/`↓` (and `ctrl+n`/`p`) move the highlight,
    // so the single-line filter keeps `←`/`→` for its caret, `enter` picks, `esc` cancels
    // (`specs/input.md` Base picker).
    if app.mode == Mode::BasePick {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let word = alt || ctrl;
        match key.code {
            Esc => app.close_base_picker(),
            Enter => app.base_picker_pick()?,
            Down => app.base_picker_move(1),
            Up => app.base_picker_move(-1),
            Char('n') if ctrl => app.base_picker_move(1),
            Char('p') if ctrl => app.base_picker_move(-1),
            code => apply_text_edit(app, code, ctrl, alt, word),
        }
        return Ok(());
    }

    // The read-only PR tab: navigate the snapshot and open links; authoring actions are inert.
    if app.tab == crate::app::Tab::Pr {
        match (action, key.code) {
            (Some(K::Quit), _) => app.should_quit = true,
            (Some(K::Refresh), _) => {
                app.request_pr_refresh(crate::app::RefreshKind::Forced);
                app.refresh_commanded = true;
            }
            (Some(K::TabChanges), _) => app.set_tab(crate::app::Tab::Changes)?,
            (Some(K::TabAllFiles), _) => app.set_tab(crate::app::Tab::AllFiles)?,
            (Some(K::OpenPr), _) => app.pr_open(),
            (Some(K::Search), _) => app.open_search(),
            (Some(K::NavigatorPosition), _) => app.cycle_navigator_position(),
            (Some(K::NavigatorGrow), _) => app.resize_navigator(4),
            (Some(K::NavigatorShrink), _) => app.resize_navigator(-4),
            (Some(K::Down), _) => app.pr_move(1),
            (Some(K::Up), _) => app.pr_move(-1),
            (Some(K::Keys), _) => app.toggle_keys(),
            (_, Esc) => app.escape(),
            (_, Tab) => app.toggle_focus(),
            (Some(K::PageDown), _) if app.focus == Focus::Files => app.pr_scroll_nav(PAGE),
            (Some(K::PageUp), _) if app.focus == Focus::Files => app.pr_scroll_nav(-PAGE),
            (Some(K::PageDown), _) => app.pr_scroll_read(PAGE),
            (Some(K::PageUp), _) => app.pr_scroll_read(-PAGE),
            _ => {}
        }
        return Ok(());
    }

    // The comments-list overlay acts through the same bindings and closes on `esc` and the
    // `comments` binding (`specs/input.md`).
    if app.mode == Mode::List {
        match (action, key.code) {
            (Some(K::Comments), _) | (_, Esc) => app.close_list(),
            (Some(K::Down), _) => app.list_move(1),
            (Some(K::Up), _) => app.list_move(-1),
            (Some(K::Send), _) => app.send_to_agent(),
            (Some(K::Copy), _) => {
                app.export(&Clipboard);
            }
            (Some(K::Edit), _) => app.start_edit(),
            (Some(K::EditFile), _) => app.request_edit_file(),
            (Some(K::Delete), _) => app.delete_comment(),
            _ => {}
        }
        return Ok(());
    }

    if let Some(action) = action {
        match action {
            K::Quit => app.should_quit = true,
            K::Refresh => {
                app.request_world_refresh(false, false);
                app.refresh_commanded = true;
            }
            K::TabChanges => app.set_tab(crate::app::Tab::Changes)?,
            K::TabAllFiles => app.set_tab(crate::app::Tab::AllFiles)?,
            K::TabPr => app.set_tab(crate::app::Tab::Pr)?,
            K::Down => app.move_cursor(1)?,
            K::Up => app.move_cursor(-1)?,
            // `expand`/`collapse` act on the collapsible under the cursor — a directory in the
            // file list, a fold in the diff (expand-only) — and otherwise scroll the diff
            // sideways (`scroll_h` is a no-op while wrapping, so it only acts when h-scroll is
            // meaningful) (`specs/input.md` Expand and collapse).
            K::Expand if app.on_folder() => app.expand_dir(),
            K::Collapse if app.on_folder() => app.collapse_dir(),
            K::Expand if app.on_fold() => {
                let heights = ui::diff_row_heights(app, area);
                app.expand_fold(&heights, ui::diff_viewport_height(area, app));
            }
            K::Expand => app.scroll_h(8),
            K::Collapse => app.scroll_h(-8),
            K::PageDown => app.move_cursor(PAGE)?,
            K::PageUp => app.move_cursor(-PAGE)?,
            K::HalfDown => app.move_cursor(HALF_PAGE)?,
            K::HalfUp => app.move_cursor(-HALF_PAGE)?,
            K::NextHunk => app.next_hunk(),
            K::PrevHunk => app.prev_hunk(),
            K::NextFile => app.next_file(),
            K::PrevFile => app.prev_file(),
            K::Wrap => app.toggle_wrap(),
            K::Preview => app.toggle_preview(),
            K::NavigatorPosition => app.cycle_navigator_position(),
            K::NavigatorHide => app.toggle_navigator_hidden(),
            K::NavigatorGrow => app.resize_navigator(4),
            K::NavigatorShrink => app.resize_navigator(-4),
            K::ScopeUncommitted => app.set_scope(Scope::Uncommitted)?,
            K::ScopeBranch => app.set_scope(Scope::Branch)?,
            K::ScopeLastTurn => app.set_scope(Scope::LastTurn)?,
            K::BasePick => app.open_base_picker(),
            K::Select => app.toggle_select(),
            K::Comment => app.start_comment(),
            // `edit`/`delete` act on the comment under the diff cursor, so they only fire with
            // the diff focused — otherwise `delete` would silently drop a comment under an
            // off-screen cursor. (The comments-list overlay targets the highlighted row instead.)
            K::Edit if app.focus == Focus::Diff => app.start_edit(),
            K::Delete if app.focus == Focus::Diff => app.delete_comment(),
            // `edit-file` follows the cursor wherever a file is named: the read pane's
            // line, the navigator's selected file row (`specs/input.md`).
            K::EditFile => app.request_edit_file(),
            K::Send => app.send_to_agent(),
            K::Copy => {
                app.export(&Clipboard);
            }
            K::NextComment => app.jump_comment(1),
            K::PrevComment => app.jump_comment(-1),
            K::Comments => app.open_list(),
            K::Search => app.open_search(),
            K::Find => app.open_find(),
            K::Keys => app.toggle_keys(),
            // `edit`/`delete` off the diff, and `open-pr` off the `PR` tab, are inert.
            K::Edit | K::Delete | K::OpenPr => {}
        }
        return Ok(());
    }

    match key.code {
        Tab => app.toggle_focus(),
        // `esc` peels one layer: a live selection, then an armed crossing, then the footer
        // expansion (the `esc` ladder, `specs/input.md`).
        Esc => app.escape(),
        _ => {}
    }
    Ok(())
}

/// Cancel pointer state whose coordinates belonged to the old terminal geometry.
fn handle_resize(app: &mut App) {
    app.cancel_divider_drag();
    // A resize reflows wrapping under an active gesture, so it cancels like a keypress —
    // and re-wraps the display rows a settled span is anchored to, so that clears too
    // (specs/text-selection.md).
    app.cancel_gesture();
    app.clear_settled_selection();
    app.hover = None;
}

/// Route a mouse-down over selectable text: it arms the text gesture, carrying its
/// multi-click count. The count acts at the release, so a press inside the double-click
/// window that drags away is still a plain drag selection. Returns whether the event landed
/// on text (specs/text-selection.md).
fn handle_text_down(app: &mut App, m: MouseEvent, area: Rect) -> bool {
    use crate::selection::{Gesture, Point, Surface, TextDrag};
    let file_tab = app.tab != crate::app::Tab::Pr;
    let arm = |app: &mut App, surface: Surface, point: Point| {
        let count = app.note_click(m.column, m.row, point.row);
        app.gesture =
            Gesture::Text { drag: TextDrag { surface, anchor: point, extent: point }, count };
    };
    if file_tab && let Some(point) = ui::read_point_at(area, app, m.column, m.row) {
        arm(app, Surface::Read, point);
        return true;
    }
    if file_tab && let Some((comment, point)) = ui::card_point_at(area, app, m.column, m.row) {
        arm(app, Surface::Card { comment }, point);
        return true;
    }
    if let Some(point) = ui::painted_point(area, app, m.column, m.row, false) {
        arm(app, Surface::Painted, point);
        return true;
    }
    if file_tab
        && let Some(i) =
            ui::hit_file(area, app, m.column, m.row, app.file_rows.len(), app.file_scroll)
    {
        arm(app, Surface::Files, Point { row: i, chr: 0 });
        return true;
    }
    if !file_tab && let Some(i) = ui::pr_nav_display_row(area, app, m.column, m.row, false) {
        arm(app, Surface::PrNav, Point { row: i, chr: 0 });
        return true;
    }
    false
}

/// Extend the active text drag to the pointer: scroll while the pointer sits past the pane's
/// content, then move the extent (specs/text-selection.md).
fn text_drag_extend(app: &mut App, m: MouseEvent, area: Rect) {
    text_drag_edge_scroll(app, m, area);
    text_drag_set_extent(app, m, area);
}

/// The vertical scroll for a drag pointer against `inner`'s rows: past them — on the border
/// or beyond — scrolls, so the outermost content rows stay selectable without scrolling
/// (specs/text-selection.md).
fn edge_delta(row: u16, inner: Rect) -> isize {
    if row < inner.y {
        return -1;
    }
    isize::from(row >= inner.y + inner.height)
}

/// Whether the pointer's cell sits on reviewr's own pane edge — the position half of the
/// exit signature (specs/text-selection.md). herdr delivers mouse events from anywhere
/// inside the pane, so a release anywhere further in would have arrived; only the outermost
/// cells can precede a lost release. Public for the gesture tests, like [`handle_mouse`].
#[must_use]
pub fn pointer_at_pane_edge(m: MouseEvent, area: Rect) -> bool {
    m.row <= area.y
        || m.row >= area.y + area.height.saturating_sub(1)
        || m.column <= area.x
        || m.column >= area.x + area.width.saturating_sub(1)
}

/// Scroll the active drag's pane while the pointer sits past its content rows.
fn text_drag_edge_scroll(app: &mut App, m: MouseEvent, area: Rect) {
    use crate::selection::Surface;
    let Some(drag) = app.text_drag() else { return };
    match drag.surface {
        Surface::Read => read_edge_scroll(app, m, area, true),
        // Card text ignores h-scroll, so a card drag scrolls vertically only —
        // the gesture never moves a surface it is not on (TS-ONE-SURFACE).
        Surface::Card { .. } => read_edge_scroll(app, m, area, false),
        Surface::Files => {
            let inner = ui::files_inner_rect(area, app);
            let delta = edge_delta(m.row, inner);
            if inner.height > 0 && delta != 0 {
                app.wheel_files(delta);
            }
        }
        Surface::Painted => {
            // The painted rect, not the pane's inner rect: a `PR` notice sits above the
            // content, and its rows must scroll a drag, not dead-zone it.
            let Some(rect) = ui::painted_sel(app, area).map(|s| s.rect) else { return };
            let delta = edge_delta(m.row, rect);
            if rect.height > 0 && delta != 0 {
                if app.tab == crate::app::Tab::Pr {
                    app.pr_scroll_read(delta);
                } else {
                    app.wheel_diff(delta); // the preview's scroll path
                }
            }
        }
        Surface::PrNav => {
            let inner = ui::files_inner_rect(area, app);
            let delta = edge_delta(m.row, inner);
            if inner.height > 0 && delta != 0 {
                app.pr_scroll_nav(delta);
            }
        }
    }
}

/// Move the active drag's extent to the pointer, clamped into its surface (TS-ONE-SURFACE).
/// The wheel arms call this alone — their scroll already happened, and adding the edge scroll
/// on top would make wheel speed depend on where the pointer rests.
fn text_drag_set_extent(app: &mut App, m: MouseEvent, area: Rect) {
    use crate::selection::{Point, Surface};
    let Some(drag) = app.text_drag() else { return };
    let extent = match drag.surface {
        Surface::Read => ui::read_point_clamped(area, app, m.column, m.row),
        Surface::Card { comment } => ui::card_point_clamped(area, app, comment, m.column, m.row),
        Surface::Painted => ui::painted_point(area, app, m.column, m.row, true),
        Surface::Files => {
            let inner = ui::files_inner_rect(area, app);
            if inner.height == 0 || app.file_rows.is_empty() {
                None
            } else {
                let y = m.row.clamp(inner.y, inner.y + inner.height - 1);
                let i = ((y - inner.y) as usize + app.file_scroll).min(app.file_rows.len() - 1);
                Some(Point { row: i, chr: 0 })
            }
        }
        Surface::PrNav => ui::pr_nav_display_row(area, app, m.column, m.row, true)
            .map(|i| Point { row: i, chr: 0 }),
    };
    if let Some(p) = extent
        && let crate::selection::Gesture::Text { drag, .. } = &mut app.gesture
    {
        drag.extent = p;
    }
}

/// Scroll the read pane while a drag holds the pointer past its content rows (the find band
/// counts as chrome, not content): rows on the border or beyond, and with `horizontal` set
/// and wrap off the same for columns.
fn read_edge_scroll(app: &mut App, m: MouseEvent, area: Rect, horizontal: bool) {
    let content = ui::read_content_rect(area, app);
    if content.height == 0 {
        return;
    }
    let delta = edge_delta(m.row, content);
    if delta != 0 {
        app.wheel_diff(delta);
        // The same event's extent update maps against the post-scroll layout.
        ui::refresh_read_layout(app, area);
    }
    if horizontal && !app.wrap {
        if m.column < content.x {
            app.h_scroll = app.h_scroll.saturating_sub(2);
        } else if m.column >= content.x + content.width {
            // Capped at the widest visible row's last column, so a held border drag cannot
            // strand the view past all content — and never pulled back, so a keyboard
            // scroll already past the cap keeps its place (specs/text-selection.md).
            let cap = ui::widest_visible_row(app, area).saturating_sub(1);
            if app.h_scroll < cap {
                app.h_scroll = (app.h_scroll + 2).min(cap);
            }
        }
    }
}

/// Finish a text drag at its release: a release whose point never left the anchor's
/// performs the click, or the multi-click's copy — a navigator row, the word under the
/// cell, or the triple's whole line; anything else copies the selection. `clicks_act` is
/// false while composing, whose pane clicks are inert — the multi-click copies still fire
/// there, being selection copies, not pane clicks (specs/text-selection.md).
fn finish_text_drag(
    app: &mut App,
    m: MouseEvent,
    area: Rect,
    heights: &[usize],
    clicks_act: bool,
    target: &dyn crate::export::ExportTarget,
) -> Result<()> {
    use crate::selection::{Gesture, Surface};
    let Gesture::Text { count, .. } = app.gesture else { return Ok(()) };
    // One predicate decides the whole release: the extent moves to the release point, and a
    // release whose point never left the anchor's is the click — which grants the slop the
    // spec names for free (a navigator row, a wide character's cells, a tab's expansion all
    // map many cells onto one point). Anything else is a drag with a selection, and copies
    // (specs/text-selection.md).
    text_drag_set_extent(app, m, area);
    let drag = app.text_drag().expect("matched above");
    if drag.anchor == drag.extent {
        app.gesture = Gesture::None;
        match drag.surface {
            // A navigator double copies the row's path or text, and a triple repeats it;
            // an empty row falls back to the click, so the gesture is never a silent
            // no-op (specs/text-selection.md).
            Surface::Files | Surface::PrNav if count >= 2 => {
                if !multi_click_copy(app, area, drag, target) && clicks_act {
                    perform_click(app, m, area, heights, drag)?;
                }
            }
            // A double on a character surface copies the word under the cell; whitespace
            // and wordless cells act as the click (specs/text-selection.md).
            Surface::Read | Surface::Painted | Surface::Card { .. } if count == 2 => {
                if !word_click_copy(app, area, drag, target) && clicks_act {
                    perform_click(app, m, area, heights, drag)?;
                }
            }
            // A triple on a character surface copies the row's whole source line; an
            // empty line acts as the click (specs/text-selection.md).
            Surface::Read | Surface::Painted | Surface::Card { .. } if count >= 3 => {
                if !line_click_copy(app, area, drag, target) && clicks_act {
                    perform_click(app, m, area, heights, drag)?;
                }
            }
            _ if clicks_act => perform_click(app, m, area, heights, drag)?,
            _ => {}
        }
        // The release continues the multi-click chain; only a non-release end resets it.
        app.lift_gesture_freeze();
    } else {
        complete_gesture(app, area, target);
    }
    Ok(())
}

/// The navigator double-click copy, fired at the release: a file row's repo-relative path, a
/// `PR` row's full text. Returns whether anything copied (specs/text-selection.md).
fn multi_click_copy(
    app: &mut App,
    area: Rect,
    drag: crate::selection::TextDrag,
    target: &dyn crate::export::ExportTarget,
) -> bool {
    use crate::selection::Point;
    let row = drag.anchor.row;
    let whole_row = Point { row, chr: usize::MAX };
    match surface_text(app, area, drag.surface, Point { row, chr: 0 }, whole_row) {
        // An empty row yields empty text: fall back to the click, so the gesture is never
        // a silent no-op (specs/text-selection.md).
        Some(t) if !t.is_empty() => {
            app.copy_selection_text(target, &t);
            app.settle_selection(drag, t);
            true
        }
        _ => false,
    }
}

/// The word double-click copy on the character surfaces, fired at the release: the word
/// under the cell copies and its highlight settles. Whitespace, punctuation, and cells past
/// the text return `false`, falling back to the click (specs/text-selection.md).
fn word_click_copy(
    app: &mut App,
    area: Rect,
    drag: crate::selection::TextDrag,
    target: &dyn crate::export::ExportTarget,
) -> bool {
    use crate::selection::{Point, TextDrag};
    let row = drag.anchor.row;
    let whole_row = Point { row, chr: usize::MAX };
    let Some(line) = surface_text(app, area, drag.surface, Point { row, chr: 0 }, whole_row) else {
        return false;
    };
    let Some((s, e)) = crate::selection::token_at(&line, drag.anchor.chr) else {
        return false;
    };
    let word: String = line.chars().skip(s).take(e - s + 1).collect();
    app.copy_selection_text(target, &word);
    app.settle_selection(
        TextDrag {
            surface: drag.surface,
            anchor: Point { row, chr: s },
            extent: Point { row, chr: e },
        },
        word,
    );
    true
}

/// The line triple-click copy on the character surfaces, fired at the release: the row's
/// whole source line copies and its highlight settles. An empty line returns `false`,
/// falling back to the click (specs/text-selection.md).
fn line_click_copy(
    app: &mut App,
    area: Rect,
    drag: crate::selection::TextDrag,
    target: &dyn crate::export::ExportTarget,
) -> bool {
    use crate::selection::{Point, TextDrag};
    let row = drag.anchor.row;
    let whole_row = Point { row, chr: usize::MAX };
    match surface_text(app, area, drag.surface, Point { row, chr: 0 }, whole_row) {
        Some(line) if !line.is_empty() => {
            let last = line.chars().count() - 1;
            app.copy_selection_text(target, &line);
            app.settle_selection(
                TextDrag {
                    surface: drag.surface,
                    anchor: Point { row, chr: 0 },
                    extent: Point { row, chr: last },
                },
                line,
            );
            true
        }
        _ => false,
    }
}

/// The active drag's clipboard text. Public for the gesture tests, like [`handle_mouse`].
pub fn drag_text(app: &App, area: Rect) -> Option<String> {
    let drag = app.text_drag()?;
    let (a, b) = drag.ordered();
    surface_text(app, area, drag.surface, a, b)
}

/// Complete the live gesture — the drag's own off-cell release, the release proofs, the
/// exit deadline, and a config layout or theme change (specs/text-selection.md, "How a
/// gesture ends"): a drag with a visible selection copies it to `target`
/// (`TS-NO-SILENT-LOSS`), a press that never moved and a gutter gesture dissolve with
/// nothing. Public for the gesture tests.
pub fn complete_gesture(app: &mut App, area: Rect, target: &dyn crate::export::ExportTarget) {
    if let Some(drag) = app.text_drag()
        && drag.anchor != drag.extent
    {
        let text = drag_text(app, area).unwrap_or_default();
        app.copy_selection_text(target, &text);
        // The copy leaves its span highlighted as feedback (specs/text-selection.md).
        app.settle_selection(drag, text);
    }
    app.cancel_gesture();
}

/// The copied text for a span on `surface` — the one extractor behind the drag release and
/// the multi-click copies, so a new surface cannot be extractable in one and forgotten in
/// the other (specs/text-selection.md Copy).
fn surface_text(
    app: &App,
    area: Rect,
    surface: crate::selection::Surface,
    a: crate::selection::Point,
    b: crate::selection::Point,
) -> Option<String> {
    use crate::selection::{Surface, lines_text};
    match surface {
        Surface::Read => Some(crate::selection::read_text(&app.visible, a, b)),
        Surface::Files => {
            Some(crate::selection::files_text(&app.file_rows, &app.entries, a.row, b.row))
        }
        Surface::Painted => Some(lines_text(&ui::painted_texts(app, area), a, b)),
        Surface::Card { comment } => {
            let width = ui::read_inner_rect(area, app).width as usize;
            Some(lines_text(&ui::card_body_lines(app.store.get(comment)?, width), a, b))
        }
        Surface::PrNav => {
            let texts = ui::pr_nav_texts(app);
            if texts.is_empty() {
                return None;
            }
            let hi = b.row.min(texts.len() - 1);
            Some(texts.get(a.row..=hi)?.join("\n"))
        }
    }
}

/// The click a same-cell release performs — the pre-selection mouse-down meanings
/// (specs/input.md).
fn perform_click(
    app: &mut App,
    m: MouseEvent,
    area: Rect,
    heights: &[usize],
    drag: crate::selection::TextDrag,
) -> Result<()> {
    use crate::selection::Surface;
    match drag.surface {
        // The navigators act on the drag's already-clamped row, not the raw release cell:
        // the row slop that classified the release as a click also delivers it
        // (specs/text-selection.md).
        Surface::Files => app.select_file(drag.extent.row)?,
        Surface::PrNav => {
            app.focus = Focus::Files;
            if let Some(i) = ui::pr_nav_cursor_at(app, drag.extent.row) {
                app.pr_select(i);
            }
        }
        Surface::Read | Surface::Painted | Surface::Card { .. } => {
            if let Some(url) = app.painted_link_at(m.column, m.row) {
                app.focus = Focus::Diff;
                app.open_link(&url);
            } else if app.tab == crate::app::Tab::Pr || app.preview_active() {
                // The painted surfaces have no cursor: a click only focuses the pane.
                if ui::in_diff_pane(area, app, m.column, m.row) {
                    app.focus = Focus::Diff;
                }
            } else if let Some(i) =
                ui::hit_diff(area, app, m.column, m.row, heights, app.diff_scroll)
            {
                app.focus = Focus::Diff;
                app.diff_cursor = i;
                app.select_anchor = None;
                app.expand_fold(heights, ui::diff_viewport_height(area, app));
            }
        }
    }
    Ok(())
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
    target: &dyn crate::export::ExportTarget,
) -> Result<()> {
    app.hover = Some((m.column, m.row));
    // Pointer motion with no button held, or a fresh mouse-down, proves an active gesture's
    // release was lost — herdr routes mouse by pointer position, so a release over another
    // pane never arrives here. The proof completes the old gesture (a visible selection
    // copies, `TS-NO-SILENT-LOSS`), then the event acts as any event. A motionless held drag
    // feeds no events at all and stays alive. One guard for every dispatch path below
    // (specs/text-selection.md).
    if app.gesture_active() && matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Down(_)) {
        complete_gesture(app, area, target);
    }
    // The next mouse-down is the user doing something else: the settled highlight clears,
    // after the proof above so a down-completed gesture never leaves one behind either
    // (specs/text-selection.md).
    if matches!(m.kind, MouseEventKind::Down(_)) {
        app.clear_settled_selection();
    }
    // On the search screen: chips flip, a click picks (a second click on the picked row
    // opens), the wheel moves the pick over results and scrolls the preview, and the
    // divider drags search's own share (specs/search.md Keys). A cancelled divider
    // gesture still owns its remaining drag and mouse-up events like in every modal.
    if app.mode == Mode::Search {
        use ui::SearchTarget as T;
        match m.kind {
            MouseEventKind::Drag(MouseButton::Left) if app.divider_drag_active() => {
                // The share maps the pointer's row into the band-to-footer span the two
                // panes divide, matching `search_layout`'s geometry.
                let l = ui::search_layout(ui::body_rect(area, app), app);
                let axis_len = l.results.height + l.preview.height;
                let offset = m.row.saturating_sub(l.results.y);
                app.drag_search_divider(axis_len, offset);
            }
            MouseEventKind::Drag(MouseButton::Left) if app.divider_drag_captured() => {}
            MouseEventKind::Up(MouseButton::Left) if app.divider_drag_captured() => {
                app.finish_divider_drag();
            }
            MouseEventKind::Down(MouseButton::Left) => {
                match ui::search_target(app, area, m.column, m.row) {
                    Some(T::Chips) => app.search_flip(),
                    Some(T::Divider) => app.start_divider_drag(),
                    Some(T::Row(pick)) => {
                        let picked = app.search.as_ref().is_some_and(|s| s.pick == pick);
                        if picked {
                            app.search_open_pick()?;
                        } else if let Some(s) = app.search.as_mut() {
                            s.pick = pick;
                        }
                    }
                    _ => {}
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta: isize = if m.kind == MouseEventKind::ScrollDown { 1 } else { -1 };
                match ui::search_target(app, area, m.column, m.row) {
                    Some(T::Row(_) | T::Results) => app.search_move(delta),
                    Some(T::Preview | T::Divider) => app.scroll_search_preview(delta * 3),
                    _ => {}
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // A modal captures new mouse gestures, but a divider gesture cancelled by the key that
    // opened it still owns its remaining drag and mouse-up events.
    if app.mode.is_modal() {
        // Text selection stays available while the comment editor is open, selecting from the
        // frozen view under it; its clicks stay inert like the rest of the modal's pane
        // (specs/text-selection.md).
        if app.composing() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) if !app.divider_drag_captured() => {
                    if handle_text_down(app, m, area) {
                        return Ok(());
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) if app.text_drag().is_some() => {
                    text_drag_extend(app, m, area);
                    return Ok(());
                }
                MouseEventKind::Up(MouseButton::Left) if app.text_drag().is_some() => {
                    finish_text_drag(app, m, area, heights, false, target)?;
                    return Ok(());
                }
                _ => {}
            }
        }
        match m.kind {
            // A click moves the highlight; a click on the already-highlighted row sends. The
            // highlight is armed when the picker opens, so a first click on the armed row
            // sends straight away (`specs/input.md`). Every other gesture is inert.
            MouseEventKind::Down(MouseButton::Left) if app.mode == Mode::Picker => {
                match ui::hit_picker_row(area, app, m.column, m.row) {
                    Some(i) if i == app.picker_cursor => app.picker_pick(),
                    Some(i) => app.picker_goto(i),
                    None => {}
                }
            }
            // Same shape in the base picker: click to highlight, click the highlight to pick
            // (`specs/input.md` Base picker).
            MouseEventKind::Down(MouseButton::Left) if app.mode == Mode::BasePick => {
                match ui::hit_base_picker_row(area, app, m.column, m.row) {
                    Some(i) if app.base_picker.as_ref().is_some_and(|bp| bp.cursor == i) => {
                        app.base_picker_pick()?;
                    }
                    Some(i) => app.base_picker_goto(i),
                    None => {}
                }
            }
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
    // who reaches for the mouse has left the file's edge behind (`specs/input.md`). Pointer motion
    // is not a gesture — capture reports every move over the pane, and a pointer resting on
    // the reviewr pane would otherwise disarm the crossing without the reviewer touching
    // anything.
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
            let body = ui::body_rect(area, app);
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

    // The read-only PR tab: click a tab or the open button, click a row to read it, drag over
    // text to select and copy it, and wheel either pane without moving the selection.
    if app.tab == crate::app::Tab::Pr {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(ui::HeaderHit::Tab(tab)) =
                    ui::hit_header(area, app, keymap, m.column, m.row)
                {
                    app.set_tab(tab)?;
                } else if ui::hit_pr_open(area, app, m.column, m.row) {
                    app.pr_open();
                } else if handle_text_down(app, m, area) {
                    // A pending click or text drag armed. Every painted row is claimed here,
                    // so link opens and row selection live in `perform_click`, at the release
                    // (specs/text-selection.md).
                } else if ui::in_files_pane(area, app, m.column, m.row) {
                    // Only blank space below the navigator rows reaches here: the click
                    // focuses the pane, selecting nothing.
                    app.focus = Focus::Files;
                } else if ui::in_diff_pane(area, app, m.column, m.row) {
                    app.focus = Focus::Diff;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if app.text_drag().is_some() => {
                text_drag_extend(app, m, area);
            }
            MouseEventKind::Up(MouseButton::Left) if app.text_drag().is_some() => {
                finish_text_drag(app, m, area, heights, true, target)?;
            }
            // The wheel during an active drag scrolls the drag's pane and extends the
            // selection (specs/text-selection.md).
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if app.text_drag().is_some() => {
                let delta: isize = if m.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
                if app.text_drag().map(|d| d.surface) == Some(crate::selection::Surface::PrNav) {
                    app.pr_scroll_nav(delta);
                } else {
                    app.pr_scroll_read(delta);
                }
                text_drag_set_extent(app, m, area);
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
                    // Inert when the picker cannot open here — with a `--base` flag the
                    // label names the base without offering a choice (`specs/input.md`).
                    ui::HeaderHit::Base => app.open_base_picker(),
                }
            } else if let Some(row) = ui::gutter_row_at(area, app, m.column, m.row) {
                // The gutter owns mouse commenting: click a line or drag a range, and the
                // composer opens on release (specs/input.md).
                app.start_gutter_drag(row);
            } else if handle_text_down(app, m, area) {
                // A pending click or text drag armed. Every painted preview cell is claimed
                // here, so link opens live in `perform_click`, at the release
                // (specs/text-selection.md, specs/markdown.md).
            } else if app.preview_active() {
                // A preview click only focuses the pane. The pane-rect test, not the
                // source-row hit test — the rendered preview can be taller than the
                // source has rows.
                if ui::in_diff_pane(area, app, m.column, m.row) {
                    app.focus = Focus::Diff;
                }
            } else if let Some(i) =
                ui::hit_diff(area, app, m.column, m.row, heights, app.diff_scroll)
            {
                // Only non-text display lines reach here: a fold or a comment card's line.
                app.focus = Focus::Diff;
                app.diff_cursor = i;
                app.select_anchor = None;
                // A click on a fold marker expands it, keeping the viewport still.
                app.expand_fold(heights, ui::diff_viewport_height(area, app));
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if app.gutter_drag() => {
            // A gutter drag selects rows, so it never scrolls horizontally.
            read_edge_scroll(app, m, area, false);
            if let Some(p) = ui::read_point_clamped(area, app, m.column, m.row) {
                app.drag_select_to(p.row);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if app.text_drag().is_some() => {
            text_drag_extend(app, m, area);
        }
        MouseEventKind::Up(MouseButton::Left) if app.gutter_drag() => {
            app.finish_gutter_drag();
        }
        MouseEventKind::Up(MouseButton::Left) if app.text_drag().is_some() => {
            finish_text_drag(app, m, area, heights, true, target)?;
        }
        // The wheel during an active drag scrolls the drag's pane and extends the selection
        // (specs/text-selection.md).
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
            if app.text_drag().is_some() || app.gutter_drag() =>
        {
            let delta: isize = if m.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
            let files =
                app.text_drag().map(|d| d.surface) == Some(crate::selection::Surface::Files);
            if files {
                app.wheel_files(delta);
            } else {
                app.wheel_diff(delta);
                // The same event's extent update maps against the post-scroll layout.
                ui::refresh_read_layout(app, area);
            }
            if app.gutter_drag() {
                if let Some(p) = ui::read_point_clamped(area, app, m.column, m.row) {
                    app.drag_select_to(p.row);
                }
            } else {
                text_drag_set_extent(app, m, area);
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
    use std::time::Instant;

    use super::{
        ActiveFetch, FETCH_HANG, PaintedFrameSnapshot, PrCoordinator, PrEffect, PrRefresh,
        TaggedPr, apply_plugin_config_observation, apply_pr_probe_result, drain_pr_shutdown,
        glyph_clears, handle_blocked_event, handle_resize, ready_app, schedule_poll_probe,
        world_indicator, world_wake,
    };
    use crate::app::{App, Tab};

    #[test]
    fn the_indicator_lights_only_for_a_building_job_past_the_delay() {
        use std::time::Duration;
        assert!(!world_indicator(None), "nothing in flight, nothing lit");
        assert!(
            !world_indicator(Some((Duration::from_millis(500), false))),
            "sample-only jobs never light it"
        );
        assert!(!world_indicator(Some((Duration::from_millis(100), true))), "below the delay");
        assert!(
            world_indicator(Some((Duration::from_millis(200), true))),
            "a building job past the delay lights it"
        );
    }

    #[test]
    fn the_lit_glyph_holds_its_minimum_display() {
        use std::time::Duration;
        assert!(!glyph_clears(Duration::from_millis(100)), "a fast landing keeps the glyph lit");
        assert!(glyph_clears(Duration::from_millis(300)), "past the hold it goes dark");
    }

    #[test]
    fn the_in_flight_wake_is_tight_only_for_a_building_job() {
        use std::time::Duration;
        assert_eq!(world_wake(true), Duration::from_millis(15));
        assert_eq!(world_wake(false), Duration::from_millis(100));
    }
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
        input_with("github.com", "acme", "widgets", head)
    }

    fn input_with(host: &str, owner: &str, name: &str, head: &str) -> PrFetchInput {
        PrFetchInput {
            repository: RepositoryIdentity::Repository(
                crate::git::RepoTarget::new(host, owner, name).unwrap(),
            ),
            origin_repository: None,
            local: crate::git::PrLocalState {
                head_oid: Some(head.to_string()),
                base_oid: Some("base".to_string()),
                names: vec!["feature".to_string()],
                detached: false,
            },
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
    fn terminal_resize_cancels_a_live_text_drag_without_copying() {
        use crate::selection::{Gesture, Point, Surface, TextDrag};
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        let drag = TextDrag {
            surface: Surface::Read,
            anchor: Point { row: 0, chr: 0 },
            extent: Point { row: 0, chr: 4 },
        };
        app.gesture = Gesture::Text { drag, count: 1 };
        app.settle_selection(drag, "alpha".into());

        handle_resize(&mut app);

        // The reflow row of the gesture end table: nothing copies, the input acts — and
        // the settled span clears, since the resize re-wraps the rows it anchors to
        // (specs/text-selection.md).
        assert!(!app.gesture_active(), "a resize ends the gesture");
        assert_eq!(app.status, "", "a resize-cancelled drag copies nothing");
        assert!(app.settled_selection().is_none(), "a resize clears the settled highlight");
    }

    #[test]
    fn an_ambient_refresh_rides_the_in_flight_fetch_and_a_commanded_one_supersedes_it() {
        let mut pr = PrCoordinator::new(true);
        pr.refresh.current_input = Some(input("head"));
        let (generation, _) = pr.refresh.take_fetch().expect("startup fetch");
        let in_flight = |generation| ActiveFetch {
            tag: (generation, 0),
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            started: Instant::now(),
        };
        pr.active_fetch = Some(in_flight(generation));

        // Tab entry while the startup fetch runs: the generation holds, so that fetch's
        // completion still paints instead of being discarded and repeated.
        pr.request_refresh(crate::app::RefreshKind::Ambient);
        assert_eq!(pr.refresh.generation, generation, "the ambient trigger joins");
        assert!(pr.refresh.take_fetch().is_none(), "no fetch starts while the ride is on");

        // The ridden completion paints, then exactly one trailing fetch follows it, so a
        // remote change the ridden read predates is not lost to the fallback timer.
        pr.active_fetch = None;
        pr.refresh.completed(
            TaggedPr { generation, config_epoch: 0, input: input("head"), view: no_pr() },
            0,
            true,
        );
        let effect = pr.refresh.observed(input("head"), 0);
        assert!(matches!(effect, Some(PrEffect::Apply(_))), "the ridden result paints");
        let (trailing_generation, _) = pr.refresh.take_fetch().expect("the trailing fetch");
        assert!(pr.refresh.take_fetch().is_none(), "the trailing fetch is one, not a loop");
        pr.active_fetch = Some(in_flight(trailing_generation));

        // The user's refresh key supersedes: the in-flight fetch cancels, a fresh
        // generation and fetch replace it.
        let generation = trailing_generation;
        pr.request_refresh(crate::app::RefreshKind::Forced);
        assert_ne!(pr.refresh.generation, generation);
        let cancelled = pr.active_fetch.as_ref().unwrap().cancelled.clone();
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire), "the old fetch cancels");
        assert!(pr.refresh.take_fetch().is_some(), "the commanded refresh starts fresh work");

        // A fetch past the hang bound no longer blocks even an ambient trigger: it is
        // abandoned, so a reader that died without a completion cannot wedge the tab.
        let generation = pr.refresh.generation;
        let mut hung = in_flight(generation);
        hung.started = Instant::now().checked_sub(FETCH_HANG).unwrap();
        pr.active_fetch = Some(hung);
        pr.request_refresh(crate::app::RefreshKind::Ambient);
        assert!(pr.active_fetch.is_none(), "the hung fetch is abandoned");
        assert_ne!(pr.refresh.generation, generation);
        assert!(pr.refresh.take_fetch().is_some(), "a replacement fetch starts");
    }

    #[test]
    fn a_probe_that_changes_pr_rows_requires_a_repaint_before_input() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        let mut coordinator = PrCoordinator::new(true);
        coordinator.refresh.current_input = Some(input("head"));
        let moved = input_with("github.com", "upstream", "widgets", "head");

        assert!(apply_pr_probe_result(&mut app, &mut coordinator, Ok(moved.clone()), 0));
        assert!(matches!(app.pr, PrView::Pending));
        assert!(!apply_pr_probe_result(&mut app, &mut coordinator, Ok(moved), 0));
    }

    #[test]
    fn a_forge_swap_on_the_same_path_clears_the_snapshot_and_never_paints_it() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        app.apply_pr(no_pr()); // a resolved GitHub view is on screen
        let mut coordinator = PrCoordinator::new(true);
        coordinator.refresh.current_input = Some(input("head"));

        // The origin moved from github.com/acme/widgets to gitlab.com/acme/widgets. The path
        // is identical, but the repository target is forge-qualified (`specs/forge-host.md`),
        // so the probe observes a different target: the view clears instead of keeping a
        // GitHub snapshot painted over a GitLab origin.
        let mut swapped = input("head");
        swapped.repository = RepositoryIdentity::Repository(
            crate::git::RepoTarget::with_path(
                crate::git::Forge::GitLab,
                "gitlab.com",
                &["acme", "widgets"],
            )
            .unwrap(),
        );
        assert!(apply_pr_probe_result(&mut app, &mut coordinator, Ok(swapped), 0));
        assert!(matches!(app.pr, PrView::Pending), "the stale view cleared");
        assert_eq!(app.pr_forge, crate::git::Forge::GitLab, "display strings follow the forge");
    }

    #[test]
    fn a_moved_head_keeps_the_snapshot_painted_and_refetches_behind_it() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        app.apply_pr(no_pr()); // a resolved snapshot is on screen
        let mut coordinator = PrCoordinator::new(true);
        coordinator.refresh.current_input = Some(input("old"));

        // The agent committed: same repository, same candidates, new HEAD. The painted
        // model is untouched (no repaint), and the replacement fetch is already queued.
        assert!(!apply_pr_probe_result(&mut app, &mut coordinator, Ok(input("new")), 0));
        assert!(matches!(app.pr, PrView::NoPr), "the snapshot stays painted");
        assert_eq!(
            coordinator.refresh.take_fetch().map(|(_, i)| i),
            Some(input("new")),
            "the refetch starts against the moved head"
        );
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
        assert!(refresh.observed(a.clone(), 0).is_none());
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
        assert!(refresh.observed(a, 0).is_none());

        let (new_generation, _) = refresh.take_fetch().unwrap();
        assert_ne!(new_generation, old_generation);
    }

    #[test]
    fn a_changed_input_supersedes_a_completed_old_snapshot_instead_of_applying_it() {
        let a = input("a");
        let b = input("b");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0);
        let (generation, old_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 0, input: old_input, view: no_pr() },
            0,
            true,
        );

        // A head-only change supersedes the completed old snapshot without blanking: the
        // effect is Refetch, the stale completion is discarded, and the new fetch is queued.
        assert!(matches!(refresh.observed(b.clone(), 0), Some(PrEffect::Refetch)));
        assert_eq!(refresh.take_fetch().map(|(_, input)| input), Some(b.clone()));
        assert!(refresh.observed(b, 0).is_none(), "the old completion never applies");
    }

    #[test]
    fn a_trigger_during_completion_verification_supersedes_before_apply() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0);
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
        assert!(refresh.observed(a, 0).is_none(), "the completed snapshot never applies");
        let (new_generation, _) = refresh.take_fetch().unwrap();
        assert_ne!(new_generation, old_generation);
    }

    #[test]
    fn repository_and_origin_changes_are_identity_boundaries() {
        let original = input("head");
        let changes = [
            input_with("github.com", "upstream", "widgets", "head"),
            input_with("github.enterprise.test", "acme", "widgets", "head"),
            input_with("github.com", "acme", "other-widgets", "head"),
        ];

        for changed in changes {
            let mut refresh = PrRefresh::new(true);
            refresh.observed(original.clone(), 0);
            let (generation, old_input) = refresh.take_fetch().unwrap();
            refresh.completed(
                TaggedPr { generation, config_epoch: 0, input: old_input, view: no_pr() },
                0,
                true,
            );

            assert!(matches!(refresh.observed(changed.clone(), 0), Some(PrEffect::Clear)));
            assert_eq!(refresh.take_fetch().map(|(_, input)| input), Some(changed));
        }
    }

    #[test]
    fn the_hold_gate_promotes_only_a_proven_contained_no_pr() {
        use super::hold_gate;
        use crate::forge::PrView;
        let held = |view, held: Option<&str>, pin: Option<&str>, contained: bool| {
            matches!(
                hold_gate(view, held, pin, |p, o| {
                    assert_eq!((p, o), (pin.unwrap(), held.unwrap()), "pin and oid never swap");
                    Ok(contained)
                }),
                PrView::Held
            )
        };
        assert!(held(PrView::NoPr, Some("oid"), Some("pin"), true));
        assert!(!held(PrView::NoPr, Some("oid"), Some("pin"), false));
        // No painted PR, no pin, or an empty oid never holds; a resolved view passes through.
        let yes = |_: &str, _: &str| Ok(true);
        assert!(!matches!(hold_gate(PrView::NoPr, None, Some("pin"), yes), PrView::Held));
        assert!(!matches!(hold_gate(PrView::NoPr, Some("oid"), None, yes), PrView::Held));
        assert!(!matches!(hold_gate(PrView::NoPr, Some(""), Some("pin"), yes), PrView::Held));
        assert!(matches!(
            hold_gate(PrView::Detached, Some("oid"), Some("pin"), yes),
            PrView::Detached
        ));
    }

    #[test]
    fn a_failed_hold_ancestry_read_is_a_git_error_never_proof_of_absence() {
        // A transient Git failure during the hold's containment read must surface as the
        // retryable Git error, which preserves the same-target snapshot — never read as
        // "not contained", which would blank it (`specs/forge-host.md` failure table).
        use super::hold_gate;
        use crate::forge::PrView;
        let fail = |_: &str, _: &str| Err(crate::git::GitFail("rev-list failed".to_string()));
        assert!(matches!(
            hold_gate(PrView::NoPr, Some("oid"), Some("pin"), fail),
            PrView::GitError(message) if message.contains("rev-list failed")
        ));
        // The read runs only when a NoPr could promote: a resolved view, or nothing
        // held, never pays it and never fails on it.
        assert!(matches!(hold_gate(PrView::NoPr, None, Some("pin"), fail), PrView::NoPr));
        assert!(matches!(
            hold_gate(PrView::Detached, Some("oid"), Some("pin"), fail),
            PrView::Detached
        ));
    }

    #[test]
    fn a_branch_switch_clears_but_a_transient_detach_never_does() {
        // The checked-out branch is identity: a new branch is a new PR story
        // (`specs/forge-host.md` Refresh). A detach in between is freshness.
        let mut on_a = input("head");
        on_a.local.names = vec!["branch-a".to_string()];
        let mut on_b = input("head2");
        on_b.local.names = vec!["branch-b".to_string()];
        let mut detached = input("head");
        detached.local.names = Vec::new();
        detached.local.detached = true;

        let mut refresh = PrRefresh::new(true);
        assert!(refresh.observed(on_a.clone(), 0).is_none());
        assert!(matches!(refresh.observed(on_b.clone(), 0), Some(PrEffect::Clear)));

        // Detach after B: freshness, never a clear.
        assert!(matches!(refresh.observed(detached.clone(), 0), Some(PrEffect::Refetch)));
        // Reattach to the same branch: still the same story.
        assert!(matches!(refresh.observed(on_b.clone(), 0), Some(PrEffect::Refetch)));
        // Reattach to a different branch through a detach: a new story, clears.
        assert!(matches!(refresh.observed(detached, 0), Some(PrEffect::Refetch)));
        assert!(matches!(refresh.observed(on_a, 0), Some(PrEffect::Clear)));
    }

    #[test]
    fn local_state_churn_keeps_the_snapshot_and_refetches_behind_it() {
        // The locally derived state — pins, branch names — moves on a mere commit or
        // push, so it is freshness, not identity (forge-host.md): the snapshot stays
        // painted while the replacement fetch runs.
        let original = input("head");
        let mut renamed = input("head");
        renamed.local.names.push("published".to_string());
        let mut moved_base = input("head");
        moved_base.local.base_oid = Some("advanced".to_string());
        let changes = [input("moved-head"), renamed, moved_base];

        for changed in changes {
            let mut refresh = PrRefresh::new(true);
            refresh.observed(original.clone(), 0);
            let _ = refresh.take_fetch().unwrap();
            assert!(matches!(refresh.observed(changed.clone(), 0), Some(PrEffect::Refetch)));
            assert_eq!(
                refresh.take_fetch().map(|(_, input)| input),
                Some(changed),
                "the replacement fetch starts at once, on or off the tab"
            );
        }
    }

    #[test]
    fn a_stale_config_epoch_discards_the_completion_and_the_input_change_still_refetches() {
        let a = input("a");
        let b = input("b");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 1);
        let (generation, old_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 1, input: old_input, view: no_pr() },
            2,
            false,
        );
        assert!(matches!(refresh.observed(b.clone(), 2), Some(PrEffect::Refetch)));
        assert_eq!(
            refresh.take_fetch().map(|(_, input)| input),
            Some(b),
            "the discarded completion never blocks the replacement fetch"
        );
    }

    #[test]
    fn matching_completion_applies_only_after_the_verification_probe() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 3);
        let (generation, fetch_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 3, input: fetch_input, view: no_pr() },
            3,
            true,
        );

        assert!(matches!(refresh.observed(a, 3), Some(PrEffect::Apply(PrView::NoPr))));
    }

    #[test]
    fn a_failed_verification_probe_discards_the_hidden_completion() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0);
        let (generation, fetch_input) = refresh.take_fetch().unwrap();
        refresh.completed(
            TaggedPr { generation, config_epoch: 0, input: fetch_input, view: no_pr() },
            0,
            true,
        );

        refresh.probe_failed(false);
        assert!(refresh.take_fetch().is_none());
        refresh.trigger();
        assert!(refresh.observed(a, 0).is_none());
        assert!(refresh.take_fetch().is_some(), "the next refresh starts a fresh GitHub fetch");
    }

    #[test]
    fn a_failed_probe_cannot_fetch_the_previous_repository() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0);
        let _ = refresh.take_fetch().unwrap();
        refresh.trigger();

        refresh.probe_failed(false);
        assert!(refresh.take_fetch().is_none());

        refresh.trigger();
        assert!(refresh.observed(a, 0).is_none());
        assert!(refresh.take_fetch().is_some());
    }

    #[test]
    fn a_failed_probe_keeps_a_refresh_that_was_queued_behind_it() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a.clone(), 0);
        let _ = refresh.take_fetch().unwrap();

        refresh.trigger();
        refresh.probe_failed(true);
        assert!(refresh.observed(a, 0).is_none());
        assert!(refresh.take_fetch().is_some(), "the queued refresh still starts GitHub work");
    }

    #[test]
    fn a_failed_probe_keeps_the_ridden_triggers_trailing_fetch() {
        let a = input("a");
        let mut refresh = PrRefresh::new(true);
        refresh.observed(a, 0);
        let _ = refresh.take_fetch().unwrap();

        // An ambient trigger rode the in-flight fetch, then its verifying probe failed with
        // nothing else pending: the trailing fetch survives as a plain request instead of
        // waiting on the fallback timer.
        refresh.trailing = true;
        refresh.probe_failed(false);
        assert!(refresh.take_fetch().is_some(), "the trailing fetch survives the failed probe");
    }

    #[test]
    fn an_unproven_repository_replaces_the_snapshot_and_blocks_a_stale_fetch() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        let mut coordinator = PrCoordinator::new(true);
        coordinator.refresh.observed(input("head"), 0);
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
        coordinator.refresh.observed(input("head"), 0);
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
        coordinator.refresh.observed(input("head"), 0);
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
        refresh.observed(input("a"), 0);
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
        coordinator.active_fetch = Some(ActiveFetch {
            tag: (7, 3),
            cancelled: cancelled.clone(),
            started: Instant::now(),
        });

        coordinator.config_changed(true);

        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(coordinator.active_fetch_tag(), Some((7, 3)));
    }

    #[test]
    fn repository_probe_waits_for_the_active_fetch_to_exit() {
        let mut coordinator = PrCoordinator::new(true);
        coordinator.active_fetch = Some(ActiveFetch {
            tag: (7, 3),
            cancelled: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
        });

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
        coordinator.active_fetch = Some(ActiveFetch {
            tag: (7, 3),
            cancelled: fetch_cancelled.clone(),
            started: Instant::now(),
        });
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

        std::fs::write(
            config_dir.path().join("config.toml"),
            "github_host = \"github.example.com\"\n",
        )
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
    fn default_scope_seeds_a_fresh_pane_and_a_reread_never_switches_it() {
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
    fn only_a_layout_or_theme_change_or_a_block_ends_a_gesture() {
        use crate::config::plugin_config_in;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"gruvbox\"\n").unwrap();
        let previous = plugin_config_in(dir.path()).unwrap();

        // The same config, and a change that reflows nothing, leave the gesture alone.
        assert!(!super::config_ends_gesture(&previous, Some(&previous)));
        std::fs::write(&path, "theme = \"gruvbox\"\ndefault_scope = \"branch\"\n").unwrap();
        let scoped = plugin_config_in(dir.path()).unwrap();
        assert!(!super::config_ends_gesture(&previous, Some(&scoped)));

        // A theme or layout change reflows the frame, and a failed observation blocks the
        // body: each ends the gesture with its copy (specs/text-selection.md).
        std::fs::write(&path, "theme = \"nord\"\n").unwrap();
        let themed = plugin_config_in(dir.path()).unwrap();
        assert!(super::config_ends_gesture(&previous, Some(&themed)));
        std::fs::write(&path, "theme = \"gruvbox\"\nnavigator_position = \"left\"\n").unwrap();
        let moved = plugin_config_in(dir.path()).unwrap();
        assert!(super::config_ends_gesture(&previous, Some(&moved)));
        assert!(super::config_ends_gesture(&previous, None));
    }

    #[test]
    fn a_config_theme_change_ends_a_live_gesture_through_the_observation_boundary() {
        use crate::selection::{Gesture, Point, Surface, TextDrag};
        let repo = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let path = config_dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"gruvbox\"\n").unwrap();
        let mut cfg = Config::parse([repo.path().display().to_string()]);
        cfg.plugin_config_dir = Some(config_dir.path().to_path_buf());
        let mut app = App::new(repo.path().to_path_buf(), Scope::Uncommitted, None);
        app.set_plugin_config(crate::config::plugin_config_in(config_dir.path()).unwrap());
        let (tx, _rx) = mpsc::channel();
        let mut epoch = 0;
        let mut recovery_inflight = false;
        let mut pr = PrCoordinator::new(true);
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        // A pristine press, so the completion copies nothing and no clipboard runs.
        let press = Gesture::Text {
            drag: TextDrag {
                surface: Surface::Read,
                anchor: Point { row: 0, chr: 0 },
                extent: Point { row: 0, chr: 0 },
            },
            count: 1,
        };

        // An unchanged observation leaves the gesture alone.
        app.gesture = press;
        super::reconcile_plugin_config(
            &mut app,
            &cfg,
            area,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            &mut pr,
        );
        assert!(app.gesture_active(), "an unchanged config leaves the gesture alive");

        // The end table's config row: a theme change ends the gesture at the boundary,
        // before the new frame applies (specs/text-selection.md).
        std::fs::write(&path, "theme = \"nord\"\n").unwrap();
        super::reconcile_plugin_config(
            &mut app,
            &cfg,
            area,
            &mut epoch,
            &tx,
            &mut recovery_inflight,
            &mut pr,
        );
        assert!(!app.gesture_active(), "the theme change completes the gesture");
        assert_eq!(app.plugin_config().unwrap().theme(), "nord");
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
