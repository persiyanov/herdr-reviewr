//! herdr-reviewr: the terminal lifecycle and the frame loop around a terminal-free [`app`].

pub mod actions;
pub mod app;
pub mod azure_devops;
pub mod browser;
pub mod config;
pub mod diff;
pub mod editor;
pub mod export;
pub mod file_list;
pub mod forge;
pub mod git;
pub mod gitlab;
pub mod herdr;
pub mod herdr_socket;
pub mod highlight;
mod input;
pub mod keymap;
#[macro_use]
pub mod log;
pub mod markdown;
pub(crate) mod marks;
pub mod model;
pub mod proc;
pub(crate) mod rendered;
pub mod roles;
pub mod schedule;
pub mod search;
pub mod selection;
pub mod snippet;
#[cfg(test)]
mod test_support;
mod text;
pub mod theme;
pub mod turn;
pub mod ui;
pub mod wake;
pub mod watch;
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
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::Rect;

use std::process::Stdio;

use crate::app::{App, Focus, Mode};
use crate::config::{Config, PluginConfig};
use crate::export::Clipboard;
use crate::keymap::Keymap;
use crate::model::Scope;

/// Entry point: parse config, set up the terminal, run the loop, restore.
pub fn run() -> Result<()> {
    let mut cfg = Config::from_env();
    log::init();
    // The config settles before the first paint, so the first frame is the final layout; a wedged
    // herdr holds it at most 2s, then the defaults paint (issue #4).
    cfg.plugin_config_dir = config::resolve_config_dir(herdr::plugin_config_dir);
    let initial_config = config::plugin_config(cfg.plugin_config_dir.as_deref());
    let mut app = app_for(&cfg, &initial_config);

    let mut terminal = ratatui::init();
    // `ratatui::init` claimed the screen and raw mode; the input modes are left.
    claim_input_modes();
    // Paint before the first load, so a hung `git` never leaves herdr's blank pane (issue #4).
    if let Err(error) = terminal.draw(|f| ui::render(f, &app)) {
        restore_terminal();
        return Err(error.into());
    }
    // A cosmetic label; identity is the process.
    herdr::label_pane(&app.herdr.ids);
    if initial_config.is_ok()
        && let Err(e) = app.reload()
    {
        logln!("startup reload failed: {e:#}");
        app.status = format!("load failed: {e}");
    }
    // An old command line hears what changed instead of quietly doing something else.
    if let Some(note) = &cfg.removed_flag {
        app.status.clone_from(note);
    }
    let result = event_loop(&mut terminal, &mut app, &cfg);
    herdr::clear_pane_label(&app.herdr.ids);
    git::end_sessions();
    result
}

/// Claim the input modes on a screen something else owns.
fn claim_input_modes() {
    input::claim();
    let _ = execute!(io::stdout(), cursor::Hide);
}

/// Release what [`claim_input_modes`] claimed.
fn release_input_modes() {
    input::release();
    let _ = execute!(io::stdout(), cursor::Show);
}

/// Claim the screen and input modes back from an external program; never at startup.
fn claim_terminal() {
    let _ = enable_raw_mode();
    let _ = execute!(io::stdout(), EnterAlternateScreen);
    claim_input_modes();
}

/// Release everything [`claim_terminal`] claims.
fn release_terminal() {
    release_input_modes();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Leave the alternate screen and release terminal input modes before any bounded worker drain.
fn restore_terminal() {
    release_input_modes();
    ratatui::restore();
}

/// Drop the input an external program left behind, for 50 ms, never 0: its teardown replies
/// are still in flight when it exits. A resize still applies.
fn drain_input(app: &mut App) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(50);
    let mut resized = false;
    while Instant::now() < deadline && input::poll(Duration::from_millis(5))? {
        resized |= matches!(input::read(), Ok(Event::Resize(_, _)));
    }
    if resized {
        handle_resize(app);
    }
    Ok(())
}

/// Open a file in the editor: a terminal one takes the pane, a window one is left running.
fn run_editor(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    configured: Option<&str>,
    open: &mut Vec<std::process::Child>,
) -> Result<()> {
    let Some(target) = app.editor_request.take() else { return Ok(()) };
    // Absolute, so no editor reads it as a flag; symlinks kept, unlike canonicalize.
    let joined = app.repo.join(&target.path);
    let path = std::path::absolute(&joined).unwrap_or(joined);
    let command = match editor::resolve(
        configured,
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
        // Read per press, so a fixed git config is heard.
        || git::core_editor(&app.repo),
        &path,
        target.line,
    ) {
        Ok(command) => command,
        // Two causes, and the second would otherwise be told to set what it set.
        Err(editor::NoEditor::Unset) => {
            app.status =
                "no editor: set `editor` in the config, $EDITOR, or git's core.editor".into();
            return Ok(());
        }
        Err(editor::NoEditor::NamesNoProgram) => {
            app.status = "the editor command names no program".into();
            return Ok(());
        }
    };
    // A tracked row can be gone from disk, and an editor would recreate it on save.
    if !path.is_file() {
        app.status = format!("{} is gone", target.path);
        return Ok(());
    }
    logln!("editor run {} {:?}", command.program, command.args);
    // Resolved before the pane changes hands, so a missing editor never flips the screen.
    let Some(mut cmd) = proc::user_command(&command.program) else {
        app.status = format!("editor not found: {}", command.program);
        return Ok(());
    };
    cmd.args(&command.args).current_dir(&app.repo);

    if !command.wants_terminal {
        // Raw mode stays on, so `ctrl+c` stays a key; exited launchers are reaped here.
        open.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        app.status = match cmd.spawn() {
            Ok(child) => {
                open.push(child);
                format!("opened {}", target.path)
            }
            Err(e) => format!("editor failed: {e}"),
        };
        return Ok(());
    }

    // A terminal editor gets the pane, and the loop waits it out.
    app.forget_pointer();
    release_terminal();
    let launched = cmd.status();
    claim_terminal();
    drain_input(app)?;

    match launched {
        // Any exit may follow a write: `:cq` after a save, say.
        Ok(status) => {
            app.status = if status.success() {
                format!("edited {}", target.path)
            } else {
                format!("editor exited with {status}")
            };
            app.request_world_refresh(false);
            app.refresh_commanded = true;
        }
        Err(e) => app.status = format!("editor failed: {e}"),
    }
    invalidate_screen(terminal)?;
    Ok(())
}

/// Make the next draw full. `resize`, not `clear`: `clear`'s cursor query eats a keypress.
fn invalidate_screen(terminal: &mut DefaultTerminal) -> Result<()> {
    let area = terminal.size()?.into();
    terminal.resize(area)?;
    Ok(())
}

/// The reviewed repository's top level, one spelling for the app and the worker; else as given.
fn repo_root(cfg: &Config) -> std::path::PathBuf {
    git::toplevel(&cfg.repo).unwrap_or_else(|| cfg.repo.clone())
}

/// The startup app for one config snapshot: ready, or blocked on its error.
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
    logln!("start repo={} base={:?} scope={}", repo.display(), cfg.base, scope.name());
    let mut app = App::new(repo, scope, cfg.base.clone());
    app.seed_from_config(&plugin_config);
    app.set_plugin_config(plugin_config);
    app.set_cli_theme(cfg.theme.clone());
    if let Some(wrap) = cfg.wrap {
        app.wrap = wrap;
    }
    app
}

/// A transient status message (e.g. "sent 3 comments") fades after this long idle.
const STATUS_TTL: Duration = Duration::from_secs(4);

/// Stillness on the pane's edge this long completes a gesture whose release went past the pane.
const EXIT_DEADLINE: Duration = Duration::from_secs(1);

/// The `PR` tab's fallback refetch, for forge changes with no local signal.
const PR_POLL: Duration = Duration::from_mins(1);

/// How long a PR fetch may run before a trigger abandons it.
const FETCH_HANG: Duration = Duration::from_mins(1);
/// How long an ambient refresh runs unseen; a commanded one shows at once.
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
    /// The identity changed, so the snapshot blanks while the replacement fetches.
    Clear,
    /// Only freshness moved, so the snapshot stays while the replacement fetches.
    Refetch,
    Apply(crate::forge::PrView),
}

/// PR refresh convergence: a completion paints only if its generation, epoch, and input still match.
#[derive(Debug)]
struct PrRefresh {
    /// The last attached branch; a detach keeps it, so a rebase never reads as a switch.
    last_branch: Option<String>,
    generation: u64,
    current_input: Option<crate::forge::PrFetchInput>,
    pending: Option<TaggedPr>,
    fetch_needed: bool,
    /// An ambient trigger rode the in-flight fetch, so one fresh fetch follows it.
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

/// The config and layout behind the visible frame; input dispatches only while they match.
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

    /// This frame's `editor` command, so one press uses one validated snapshot
    fn editor(&self) -> Option<&str> {
        self.plugin_config.as_ref().and_then(PluginConfig::editor)
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

    /// Start a refresh: a commanded one restarts, an ambient one rides and arms a trailing fetch.
    /// A fetch past [`FETCH_HANG`] is abandoned instead, so a dead reader never wedges the tab.
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

/// Probe the PR's local input again, when the PR tab is the one on screen.
fn schedule_pr_probe(pr: &mut PrCoordinator, tab: crate::app::Tab) {
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
    fn pr_unchanged(self) -> bool {
        !matches!(self, Self::Blocked | Self::Changed { pr_changed: true, .. })
    }
}

/// An empty resolution holds the painted PR while `HEAD` still contains its head.
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
        // Target, origin, or branch changed: identity, so clear. Anything else: freshness, so keep.
        let branch = input.local.branch.clone();
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

    /// Apply the pending completion if it matches the verified input, else ask for a fetch.
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
        // An armed trailing fetch survives a failed probe.
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

/// The world job dispatched last, which a superseded completion's work is measured against.
#[derive(Clone, Copy, Debug, Default)]
pub struct LiveJob {
    pub generation: u64,
    pub full: bool,
    pub reveal: bool,
    /// When it went out, while it has not landed.
    pub running: Option<Instant>,
    /// Whether it builds a snapshot (no `PR` tab), so the glyph may light for it.
    pub builds: bool,
}

/// What landing a world completion did; the paths of one that did not land are still owed.
#[derive(Debug)]
pub enum Landing {
    /// A newer job went out after it; what that job does not read again goes back to the pacer.
    Superseded(crate::world::Refresh),
    Landed,
    /// The view moved on, or the config is invalid: discarded whole, its paths still owed.
    Discarded(crate::world::Refresh),
    /// The build failed: the stale frame stays, retried on a backoff.
    Failed(crate::world::Refresh),
}

impl Landing {
    /// Whether it was the live generation, which clears the in-flight marker.
    pub fn live(&self) -> bool {
        !matches!(self, Self::Superseded(_))
    }
}

/// Land a world completion; its snapshot reconciles only while it matches.
pub fn land_world_completion(
    app: &mut App,
    completion: crate::world::WorldCompletion,
    live: &LiveJob,
) -> Landing {
    // A switch the reviewer made re-reveals once its view lands.
    let owed = |app: &mut App| {
        if completion.reveal {
            app.request_world_refresh(true);
        }
    };
    if completion.generation != live.generation {
        if !live.reveal {
            owed(app);
        }
        let unread = if live.full { crate::world::Refresh::default() } else { completion.refresh };
        return Landing::Superseded(unread);
    }
    match completion.snapshot {
        Some(Ok(snapshot))
            if app.config_error().is_none() && app.world_input() == completion.input =>
        {
            app.reconcile_world(snapshot);
            if completion.reveal {
                // The landing may have moved the cursor.
                app.settle_tab_entry();
                app.reveal_files = true;
            }
            Landing::Landed
        }
        Some(Ok(_)) => {
            owed(app);
            Landing::Discarded(completion.refresh)
        }
        Some(Err(e)) => {
            app.status = format!("refresh failed: {e}");
            Landing::Failed(completion.refresh)
        }
        None => Landing::Landed,
    }
}

/// Land what the watcher reported: the pacer, the PR probe and search follow it. Whether the
/// config must be read again.
fn land_watch_event(
    event: crate::watch::WatchEvent,
    app: &App,
    pacer: &mut crate::schedule::Pacer,
    pr: &mut PrCoordinator,
    search: Option<&mpsc::Sender<crate::search::SearchJob>>,
) -> bool {
    use crate::watch::WatchEvent;
    let now = Instant::now();
    match event {
        // Whatever changed before the stream went live was never seen: catch up once.
        WatchEvent::Ready => {
            pacer.set_watcher_down(false, now);
            pacer.on_batch(crate::world::Refresh::Full, now);
            true
        }
        WatchEvent::Batch(batch) => {
            logln!(
                "watch paths={} git={:?} rescan={}",
                batch.worktree.len(),
                batch.git,
                batch.rescan
            );
            if batch.git.iter().any(|g| g.moves_pr_input()) {
                schedule_pr_probe(pr, app.tab);
            }
            // The search engine has no watcher of its own: new ignore rules rescan it.
            let rules = batch.git.contains(&crate::watch::GitChange::IgnoreRules);
            let job = if batch.rescan || rules {
                Some(crate::search::SearchJob::Rescan)
            } else {
                let paths: Vec<String> = batch.files.iter().cloned().collect();
                (!paths.is_empty()).then_some(crate::search::SearchJob::Changed { paths })
            };
            if let (Some(search), Some(job)) = (search, job) {
                let _ = search.send(job);
            }
            pacer.on_batch(crate::world::Refresh::from_batch(&batch), now);
            batch.config
        }
        WatchEvent::Unavailable(reason) => {
            logln!("watch unavailable: {reason}");
            pacer.set_watcher_down(true, now);
            false
        }
    }
}

/// What landing a herdr event asks of the loop.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Heard {
    /// The visibility the pane moved to, if it moved.
    pub visible: Option<bool>,
    /// `last-turn` on screen needs rebuilding, paced like a watcher batch.
    pub rebuild: bool,
}

/// Land what the herdr connection reported.
pub fn land_herdr_event(app: &mut App, event: crate::herdr_socket::HerdrEvent) -> Heard {
    use crate::herdr_socket::HerdrEvent;
    match event {
        HerdrEvent::Session(session) => {
            let visible = app.sync_herdr_session(session.me.clone(), session.visible);
            if visible.is_some() {
                logln!("herdr session me={:?} visible={}", session.me, session.visible);
            }
            Heard { visible, rebuild: false }
        }
        HerdrEvent::Turn(turn) => {
            let ended = turn.ended;
            let rebuild = app.sync_turn(turn);
            if ended {
                // A turn may have pushed or merged; a hidden pane fetches once shown.
                app.request_pr_refresh(crate::app::RefreshKind::Ambient);
            }
            Heard { visible: None, rebuild }
        }
        // Until herdr is back, assume the pane is on screen: stale work costs less than a stale view.
        HerdrEvent::Lost => Heard { visible: app.sync_herdr_session(None, true), rebuild: false },
        HerdrEvent::TooOld(version) => {
            logln!("herdr {version} is older than reviewr needs");
            app.herdr_too_old();
            Heard::default()
        }
    }
}

/// Land a search completion unless stale. True if it was live.
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

/// Whether an in-flight building job is old enough to show the glyph.
fn world_indicator(inflight: Option<(Duration, bool)>) -> bool {
    inflight.is_some_and(|(elapsed, builds)| builds && elapsed >= INDICATOR_DELAY)
}

/// Whether a lit glyph has shown long enough to go dark.
fn glyph_clears(lit_for: Duration) -> bool {
    lit_for >= INDICATOR_MIN_SHOW
}

/// Draw, then sleep until input, a worker's result, or the nearest armed deadline.
fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, cfg: &Config) -> Result<()> {
    // The config is read at the first frame and again only when the watcher says it changed.
    let mut config_dirty = true;
    // When the last mouse event came, and whether it sat on the pane's edge.
    let mut last_mouse = Instant::now();
    let mut mouse_exited = false;
    let mut last_pr_poll = Instant::now();
    // The loop sleeps on terminal input and this wake, which every worker's result and (on unix)
    // every window resize sets; nothing polls a channel.
    let wake = crate::wake::Wake::new()?;
    #[cfg(unix)]
    wake.sys().watch_resizes()?;
    let waker = wake.waker();
    // Probes and forge reads run on workers.
    let (probe_tx, probe_rx) = crate::wake::channel::<(
        u64,
        Result<crate::forge::PrFetchInput, crate::forge::PrInputError>,
    )>(&waker);
    let (recovery_tx, recovery_rx) = crate::wake::channel::<(u64, PluginConfig, App)>(&waker);
    let mut recovery_inflight = false;
    let (pr_tx, pr_rx) = crate::wake::channel::<TaggedPr>(&waker);
    let mut pr = PrCoordinator::new(app.plugin_config().is_some());
    // The world worker builds input-tagged jobs; the loop reconciles their completions.
    let (world_tx, world_job_rx) = mpsc::channel::<crate::world::WorldJob>();
    let (world_res_tx, world_rx) = crate::wake::channel::<crate::world::WorldCompletion>(&waker);
    let _world_worker = crate::world::spawn(app.repo.clone(), world_job_rx, world_res_tx);
    // herdr pushes focus, this pane's moves, and agent statuses; outside herdr nothing connects.
    let (herdr_tx, herdr_rx) = crate::wake::channel::<crate::herdr_socket::HerdrEvent>(&waker);
    let herdr = herdr::connection_target().and_then(|(socket, pane)| {
        crate::herdr_socket::Connection::start(socket, pane, app.repo.clone(), herdr_tx)
            .inspect_err(|e| logln!("herdr connection did not start: {e}"))
            .ok()
    });
    // What changed in the worktree, its git files and the config.
    let (watch_tx, watch_rx) = crate::wake::channel::<crate::watch::WatchEvent>(&waker);
    // Its writes reach turn tracking directly, so a terminal editor holding the loop delays nothing.
    let feed = herdr.as_ref().map(crate::herdr_socket::Connection::feed);
    let mut watch =
        crate::watch::Watch::start(&app.repo, cfg.plugin_config_dir.as_deref(), watch_tx, feed);
    // Input showed the pane: its edge is handled with herdr's, after the repaint.
    let mut shown_by_input: Option<bool> = None;
    // A resize can leave the screen unlike what was last drawn, even at the same size (herdr
    // reflows the pane): the next frame is drawn whole, not as a difference.
    let mut repaint_whole = false;
    // Input that showed the pane, held for the repaint; clicks aimed at the stale frame dropped.
    let mut held: std::collections::VecDeque<Event> = std::collections::VecDeque::new();
    // Window editors reviewr launched, reaped at the next press.
    let mut open_editors: Vec<std::process::Child> = Vec::new();
    // The last dispatched world job, which completions are tagged against.
    let mut world_live = LiveJob::default();
    // When a background refresh may run: batches gather, then wait out the debounce and the budget.
    let mut pacer = crate::schedule::Pacer::default();
    // Spawned on first search, so the index costs nothing until then.
    let mut search_worker: Option<(
        mpsc::Sender<crate::search::SearchJob>,
        mpsc::Receiver<crate::search::SearchCompletion>,
    )> = None;
    let mut search_generation = 0_u64;
    // When the tab-strip glyph turned on — the minimum-display clock.
    let mut glyph_since: Option<Instant> = None;
    let mut config_epoch = 0_u64;
    let mut status_at = Instant::now();
    let mut last_status = String::new();
    // Fetch the PR at open, so the tab is ready when visited.
    app.pr_pending = None;
    let result: Result<()> = (|| {
        while !app.should_quit {
            if let Ok((epoch, target, mut recovered)) = recovery_rx.try_recv() {
                recovery_inflight = false;
                if epoch == config_epoch {
                    let now = config::plugin_config(cfg.plugin_config_dir.as_deref());
                    match recovery_verdict(&target, now) {
                        Recovery::Swap => {
                            recovered.carry_authored_state_from(app);
                            *app = recovered;
                            pr.recover();
                        }
                        Recovery::Again => config_dirty = true,
                        Recovery::Invalid(error) => {
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

            app.catch_up_held_view();
            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            // A frame reads the config only when it changed; a hidden pane reads it once shown.
            if app.pane_visible() && std::mem::take(&mut config_dirty) {
                reconcile_plugin_config(
                    app,
                    cfg,
                    area,
                    &mut config_epoch,
                    &recovery_tx,
                    &mut recovery_inflight,
                    &mut pr,
                );
            }
            if pr.wait_started.is_some_and(|started| started.elapsed() >= INDICATOR_DELAY) {
                app.set_pr_refreshing(true);
                pr.wait_started = None;
            }
            // The refresh glyph: at once when commanded, past the delay when ambient.
            if std::mem::take(&mut app.refresh_commanded) {
                glyph_since.get_or_insert_with(Instant::now);
            }
            let glyph_due = if app.tab == crate::app::Tab::Pr {
                app.pr_refreshing()
            } else {
                world_indicator(world_live.running.map(|at| (at.elapsed(), world_live.builds)))
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
            // A status line expires its TTL after it last changed.
            if app.status != last_status {
                last_status.clone_from(&app.status);
                status_at = Instant::now();
            }
            if !app.status.is_empty() && status_at.elapsed() >= STATUS_TTL {
                app.status.clear();
                last_status.clear();
            }
            // The preview rebuilds only once input settles, so a pick sweep never waits on it.
            if app.mode == crate::app::Mode::Search && !input::poll(Duration::ZERO)? {
                app.build_search_preview();
            }
            let viewport = ui::diff_viewport_height(area, app);
            let effective = if app.composing() {
                let box_h = ui::composer_height(app, ui::diff_inner_width(area, app));
                viewport.saturating_sub(box_h).max(1)
            } else {
                viewport
            };
            // Rewrap rendered markdown before the heights measure it.
            app.sync_rendered_width(ui::rendered_width(area, app));
            let heights = ui::diff_row_heights(app, area);
            app.settle_diff_scroll(&heights, effective);
            let file_vp = ui::file_viewport_height(area, app);
            // A hidden navigator keeps its reveal pending, since its viewport is zero.
            if !app.navigator_hidden_here() && std::mem::take(&mut app.reveal_files) {
                app.reveal_file_cursor(file_vp);
            }
            app.bound_file_scroll(file_vp);
            let painted_frame = PaintedFrameSnapshot::capture(app);
            // A hidden pane paints nothing: herdr keeps its last frame, and showing it draws again.
            // A painted age repaints when it rolls over, while on screen.
            let mut age_due = None;
            if app.pane_visible() {
                if std::mem::take(&mut repaint_whole) {
                    invalidate_screen(terminal)?;
                }
                terminal.draw(|f| age_due = ui::render_frame(f, app))?;
            }
            let age_due = age_due.map(|due| Instant::now() + due);

            // A navigator drag holds the drain; the channel is the queue.
            if !app.gates_world_drain() {
                let mut landed = false;
                loop {
                    let completion = match world_rx.try_recv() {
                        Ok(completion) => completion,
                        // A worker that died mid-job never lands it: say so, and stop waiting.
                        Err(mpsc::TryRecvError::Disconnected) if world_live.running.is_some() => {
                            world_live.running = None;
                            app.status = "refresh worker unavailable".to_string();
                            break;
                        }
                        Err(_) => break,
                    };
                    let took = completion.took;
                    let landing = land_world_completion(app, completion, &world_live);
                    if landing.live() {
                        world_live.running = None;
                    }
                    let now = Instant::now();
                    match landing {
                        Landing::Superseded(owed) | Landing::Discarded(owed) => {
                            pacer.on_batch(owed, now);
                        }
                        Landing::Failed(owed) => pacer.on_failed(owed, now),
                        // Only a background build spends the budget; any landing ends a backoff.
                        Landing::Landed => pacer.on_landed(took, now),
                    }
                    landed = true;
                }
                if landed {
                    continue;
                }
            }

            while let Ok(event) = watch_rx.try_recv() {
                let search = search_worker.as_ref().map(|(tx, _)| tx);
                config_dirty |= land_watch_event(event, app, &mut pacer, &mut pr, search);
            }
            let mut heard = false;
            let mut moved = shown_by_input.take();
            while let Ok(event) = herdr_rx.try_recv() {
                let landed = land_herdr_event(app, event);
                moved = landed.visible.or(moved);
                // A new baseline rebuilds `last-turn` like a watcher batch.
                if landed.rebuild {
                    pacer.on_batch(crate::world::Refresh::Full, Instant::now());
                }
                heard = true;
            }
            // A hidden pane holds no search index; the next search builds it again.
            if moved == Some(false) {
                search_worker = None;
            }
            if let Some(visible) = moved {
                logln!("visible={visible}");
                pacer.set_visible(visible);
                watch.set_visible(visible);
            }
            // What the screen shows beyond the watcher's defaults: ignored entries, a local base.
            watch.show(app.shown_ignored());
            watch.set_refs(app.watched_refs());
            if heard {
                continue;
            }

            if let Some((_, rx)) = &search_worker
                && let Ok(completion) = rx.try_recv()
            {
                land_search_completion(app, completion, search_generation);
                // Repaint at once, not at the next wake.
                continue;
            }

            // Query after the paint, so typing paints at input speed; a hidden pane queries nothing.
            if app.pane_visible()
                && std::mem::take(&mut app.search_dirty)
                && app.mode == crate::app::Mode::Search
                && app.config_error().is_none()
            {
                let (tx, _) = search_worker.get_or_insert_with(|| {
                    let (job_tx, job_rx) = mpsc::channel();
                    let (res_tx, res_rx) = crate::wake::channel(&waker);
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
                let sent = tx
                    .send(crate::search::SearchJob::Query { generation: search_generation, query })
                    .is_ok();
                if !sent
                    && let Some(s) = app.search.as_mut()
                    && !matches!(s.phase, crate::app::SearchPhase::Error(_))
                {
                    // A specific error already shown stays up.
                    s.phase = crate::app::SearchPhase::Error("search worker unavailable".into());
                    // No stale file under the error.
                    s.preview = None;
                }
            }
            if let Some(path) = app.search_track.take()
                && let Some((tx, _)) = &search_worker
            {
                let _ = tx.send(crate::search::SearchJob::Track { path });
            }

            // The reviewer's refresh carries the paced paths along; a background one waits for its
            // deadline and for the last one to land.
            let due =
                world_live.running.is_none().then(|| pacer.take_due(Instant::now())).flatten();
            // While the watcher is down, the stand-in reads the config too, even with a refresh
            // held by an invalid one.
            config_dirty |= due.is_some() && pacer.watcher_down();
            let gathered =
                if app.world_request.is_some() { due.or_else(|| pacer.take_now()) } else { due };
            if let Some(refresh) = gathered {
                app.request_paced_refresh(refresh);
            }
            // Refresh after the paint, so a switch stays instant.
            if app.world_request.is_some() && app.config_error().is_none() {
                let request = app.world_request.take().expect("checked above");
                let input = app.world_input();
                world_live = LiveJob {
                    generation: world_live.generation.wrapping_add(1),
                    full: request.refresh.is_full(),
                    reveal: request.reveal,
                    running: Some(Instant::now()),
                    // The `PR` tab builds nothing.
                    builds: input.tab.is_file_tab(),
                };
                let job = crate::world::WorldJob {
                    generation: world_live.generation,
                    input,
                    reveal: request.reveal,
                    refresh: request.refresh,
                };
                logln!("refresh {:?} background={}", job.refresh, request.background);
                pacer.on_dispatched(request.background);
                if world_tx.send(job).is_err() {
                    // A dead worker must not pin the in-flight marker.
                    app.status = "refresh worker unavailable".to_string();
                    world_live.running = None;
                }
            }

            // Triggers first, so a commanded one supersedes a completion before it paints.
            // A hidden pane fetches nothing: a trigger waits for it to be shown.
            let fallback_poll = app.pane_visible()
                && app.tab == crate::app::Tab::Pr
                && last_pr_poll.elapsed() >= PR_POLL;
            let pending = if app.pane_visible() { app.pr_pending.take() } else { None };
            let refresh = pending.or(fallback_poll.then_some(crate::app::RefreshKind::Ambient));
            if let Some(kind) = refresh {
                last_pr_poll = Instant::now();
                pr.request_refresh(kind);
            }

            // A fetch waits for a fresh probe; a gesture holds both PR drains.
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

            // The probe is the authority on the current input.
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

            if app.pane_visible() && pr.can_start_probe(app.plugin_config().is_some()) {
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

            if app.pane_visible()
                && pr.active_fetch.is_none()
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
                    // On the worker, so the event loop never runs git.
                    let view = hold_gate(
                        view,
                        held.as_deref(),
                        input.local.head_oid.as_deref(),
                        |pin, oid| crate::git::contains_commit(&repo, pin, oid),
                    );
                    let _ = tx.send(TaggedPr { generation, config_epoch: epoch, input, view });
                });
            }
            // Only armed one-shot deadlines wake an idle loop; with none, it sleeps until something
            // happens. A status line fades on time.
            let mut timeout = Duration::MAX;
            if !app.status.is_empty() {
                timeout = timeout.min(STATUS_TTL.saturating_sub(status_at.elapsed()));
            }
            if let Some(due) = age_due {
                timeout = timeout.min(due.saturating_duration_since(Instant::now()));
            }
            // The PR tab refetches every minute while it is on screen.
            if app.pane_visible() && app.tab == crate::app::Tab::Pr {
                timeout = timeout.min(PR_POLL.saturating_sub(last_pr_poll.elapsed()));
            }
            // Workers wake the loop when their results land, so nothing in flight needs a timer,
            // except the glyph lighting a building job past its delay.
            if let Some(due) = pacer.next_deadline().filter(|_| world_live.running.is_none()) {
                timeout = timeout.min(due.saturating_duration_since(Instant::now()));
            }
            if let Some(started) = world_live.running.filter(|_| world_live.builds) {
                let left = INDICATOR_DELAY.saturating_sub(started.elapsed());
                if !left.is_zero() {
                    timeout = timeout.min(left);
                }
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
            // crossterm may hold input it already read, so ask it first and sleep only when it
            // has none.
            let ready = !held.is_empty() || input::poll(Duration::ZERO)? || {
                let reason = input::wait(&wake, Some(timeout).filter(|t| *t != Duration::MAX))?;
                logln!("wake {reason:?}");
                reason == crate::wake::Woke::Input
            };
            if ready {
                if !painted_frame.still_current(app) {
                    continue;
                }
                let event = match held.pop_front() {
                    Some(event) => event,
                    None => input::read()?,
                };
                // The reviewer's input reaching a hidden pane shows it; a resize is only the terminal.
                if !matches!(event, Event::Resize(..)) && app.note_input() {
                    shown_by_input = Some(true);
                    // Paint first: keys act on what is seen, clicks aimed at the stale frame drop.
                    let mut buffered = vec![event];
                    while input::poll(Duration::ZERO)? {
                        buffered.push(input::read()?);
                    }
                    held.extend(buffered.into_iter().filter(|e| !matches!(e, Event::Mouse(_))));
                    continue;
                }
                repaint_whole |= matches!(event, Event::Resize(..));
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
                        // This frame's heights, so a drag never re-measures the diff.
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
            if app.editor_request.is_some() {
                run_editor(terminal, app, painted_frame.editor(), &mut open_editors)?;
            }
            if app.should_quit {
                break;
            }
            // Stillness inside the pane is a held button; only an edge exit completes.
            if app.gesture_active() && mouse_exited && last_mouse.elapsed() >= EXIT_DEADLINE {
                complete_gesture(app, area, &Clipboard);
            }
        }
        Ok(())
    })();
    restore_terminal();
    drain_pr_shutdown(&mut pr, &probe_rx, &pr_rx);
    result
}

/// A click or a wheel turn: the mouse events that answer the quit question.
fn answers_question(kind: MouseEventKind) -> bool {
    matches!(kind, MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
}

/// A blocked frame's input: quit and cleanup only.
fn handle_blocked_event(app: &mut App, event: &Event) {
    match event {
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            // `q` quits whatever the modifiers, asking first when comments are queued.
            let action = match k.code {
                KeyCode::Char(c) => keymap::default_keymap().action_for(keymap::Key::plain(c)),
                _ => None,
            };
            match (app.confirming_quit, action) {
                (true, Some(keymap::Action::QuitDiscard)) => app.should_quit = true,
                (true, Some(keymap::Action::Quit)) => {}
                (true, _) => app.confirming_quit = false,
                (false, Some(keymap::Action::Quit)) => app.request_quit(),
                (false, _) => {}
            }
        }
        Event::Mouse(m) if app.confirming_quit && answers_question(m.kind) => {
            app.confirming_quit = false;
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

/// Apply a verified probe result; true means repaint before more input.
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
            // The noun follows the forge; a forge change clears the snapshot in the same step.
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
                    // The snapshot stays; nothing repaints now.
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
    recovery_tx: &crate::wake::Sender<(u64, PluginConfig, App)>,
    recovery_inflight: &mut bool,
    pr: &mut PrCoordinator,
) -> ConfigGate {
    let previous = app.plugin_config().cloned();
    let observed = config::plugin_config(cfg.plugin_config_dir.as_deref());
    // A reflowing config change completes the live gesture's copy first.
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
        // A theme change rebuilds the highlighted diffs before anything mixes states.
        if let Err(error) = app.reload() {
            app.status = format!("config refresh failed: {error}");
        }
    }
    ConfigGate::Changed { pr_changed }
}

/// What a finished recovery does, by the config on disk now.
#[derive(Debug)]
enum Recovery {
    /// It still reads as the recovery's target: the recovered app takes over.
    Swap,
    /// It was saved again meanwhile: read it once more, which starts a fresh recovery.
    Again,
    Invalid(config::PluginConfigError),
}

fn recovery_verdict(
    target: &PluginConfig,
    now: Result<PluginConfig, config::PluginConfigError>,
) -> Recovery {
    match now {
        Ok(now) if now == *target => Recovery::Swap,
        Ok(_) => Recovery::Again,
        Err(error) => Recovery::Invalid(error),
    }
}

/// Whether a config observation ends a gesture: a reflow, or a failure that blocks the body.
#[must_use]
fn config_ends_gesture(previous: &PluginConfig, observed: Option<&PluginConfig>) -> bool {
    match observed {
        Some(c) => {
            previous.navigator_position() != c.navigator_position() || previous.theme() != c.theme()
        }
        None => true,
    }
}

/// Apply one config observation: invalid blocks, recovery swaps in a fresh app carrying the review.
fn apply_plugin_config_observation(
    app: &mut App,
    cfg: &Config,
    epoch: &mut u64,
    recovery_tx: &crate::wake::Sender<(u64, PluginConfig, App)>,
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

/// Apply one readline-style key to the active text field; `word` moves by word.
fn apply_text_edit(app: &mut App, code: KeyCode, ctrl: bool, alt: bool, word: bool) {
    use KeyCode::{Backspace, Char, Delete, End, Home, Left, Right};
    match code {
        Char('w') if ctrl => app.input_delete_word(),
        Char('a') if ctrl => app.caret_home(),
        Char('e') if ctrl => app.caret_end(),
        Char('u') if ctrl => app.input_kill_to_start(),
        Char('k') if ctrl => app.input_kill_to_end(),
        // `Alt+b`/`Alt+f` survive multiplexers that strip modified arrows.
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

/// Map one key press onto `App` through the painted frame's `keymap`.
pub fn handle_key(app: &mut App, key: KeyEvent, area: Rect, keymap: &Keymap) -> Result<()> {
    let done = dispatch_key(app, key, area, keymap);
    app.settle_pick();
    done
}

/// [`handle_key`]'s dispatch: the key's own action, its many early returns ahead of the tail.
fn dispatch_key(app: &mut App, key: KeyEvent, area: Rect, keymap: &Keymap) -> Result<()> {
    use crate::keymap::Action as K;
    use KeyCode::{Char, Down, Enter, Esc, Left, PageDown, PageUp, Right, Tab, Up};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // A keypress cancels the gesture but keeps consuming its drag events until mouse-up.
    app.cancel_divider_drag();
    // A key cancels a live gesture without copying, then does its own work.
    app.cancel_gesture();
    // Any keypress is the user doing something else: the settled highlight clears
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

    // Search: the query edits like a draft; `tab` flips mode, page keys scroll the preview.
    if app.mode == Mode::Search {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let word = alt || ctrl;
        match key.code {
            Esc => app.close_search(),
            Enter => app.search_open_pick()?,
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

    // Line field: digits or `$` edit, Enter jumps, Esc closes, the rest is inert.
    if app.line_open() {
        let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            Esc => app.close_find(),
            Enter => app.line_go(),
            Char(c @ ('0'..='9' | '$')) if plain => app.line_type(c),
            code @ (KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End)
                if plain =>
            {
                apply_text_edit(app, code, false, false, false);
            }
            _ => {}
        }
        return Ok(());
    }
    // Find: printables edit, `↑`/`↓` step matches, `esc` closes, the rest is inert.
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

    // Bound keys resolve through the keymap; the rest fall to the fixed `tab` and `esc`.
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

    // The quit question owns the keyboard; `q` leaves it open, so auto-repeat never answers it.
    if app.confirming_quit {
        match action {
            Some(K::QuitDiscard) => app.should_quit = true,
            Some(K::Quit) => {}
            Some(K::Send) => {
                app.confirming_quit = false;
                app.send_to_agent();
            }
            Some(K::Copy) => {
                app.confirming_quit = false;
                app.export(&Clipboard);
            }
            _ => app.confirming_quit = false,
        }
        return Ok(());
    }

    // Any other key drops an armed crossing; `esc` drops it as its own ladder step.
    if !matches!(action, Some(K::NextHunk | K::PrevHunk)) && key.code != Esc {
        app.disarm_cross();
    }

    // The agent picker is strictly modal, so a habitual `q` or `y` can't lose the review.
    if app.mode == Mode::Picker {
        // Only bare `enter` sends and bare digits move; `esc` cancels with any modifier.
        let bare = key.modifiers.is_empty();
        match (action, key.code) {
            (_, Esc) => app.close_picker(),
            (_, Enter) if bare => app.picker_pick(),
            // Digits outrank movement bindings.
            (_, Char(c @ '1'..='9')) if bare => {
                app.picker_goto(c as usize - '1' as usize);
            }
            (Some(K::Down), _) => app.picker_move(1),
            (Some(K::Up), _) => app.picker_move(-1),
            _ => {}
        }
        return Ok(());
    }

    // Base picker: every printable filters, so a branch `qa` is typable; vertical keys move.
    if app.mode == Mode::BasePick {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let word = alt || ctrl;
        match key.code {
            Esc => app.close_base_picker(),
            Enter => app.base_picker_pick()?,
            Down => app.base_picker_move(1),
            Up => app.base_picker_move(-1),
            PageDown => app.base_picker_move(PAGE),
            PageUp => app.base_picker_move(-PAGE),
            Char('n') if ctrl => app.base_picker_move(1),
            Char('p') if ctrl => app.base_picker_move(-1),
            code => apply_text_edit(app, code, ctrl, alt, word),
        }
        return Ok(());
    }

    // Commit picker: move, `select` anchors, `enter` picks, `esc` unanchors or closes.
    if app.mode == Mode::CommitPick {
        match (action, key.code) {
            (_, Esc) => app.commit_picker_escape(),
            (_, Enter) => app.commit_picker_pick()?,
            (Some(K::Down), _) => app.commit_picker_move(1),
            (Some(K::Up), _) => app.commit_picker_move(-1),
            (Some(K::PageDown), _) => app.commit_picker_move(PAGE),
            (Some(K::PageUp), _) => app.commit_picker_move(-PAGE),
            (Some(K::HalfDown), _) => app.commit_picker_move(HALF_PAGE),
            (Some(K::HalfUp), _) => app.commit_picker_move(-HALF_PAGE),
            (Some(K::Select), _) => app.commit_picker_anchor(),
            _ => {}
        }
        return Ok(());
    }

    // The read-only PR tab: navigate the snapshot and open links; authoring actions are inert.
    if app.tab == crate::app::Tab::Pr {
        match (action, key.code) {
            (Some(K::Quit), _) => app.request_quit(),
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
            (Some(K::Expand), _) => app.expand_pr_details(),
            (Some(K::Collapse), _) => app.collapse_pr_details(),
            _ => {}
        }
        return Ok(());
    }

    // The comments list closes on `esc` or `comments`.
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
            (Some(K::Delete), _) => app.delete_comment(),
            _ => {}
        }
        return Ok(());
    }

    if let Some(action) = action {
        match action {
            K::Quit => app.request_quit(),
            K::Refresh => {
                app.request_world_refresh(false);
                app.refresh_commanded = true;
            }
            K::TabChanges => app.set_tab(crate::app::Tab::Changes)?,
            K::TabAllFiles => app.set_tab(crate::app::Tab::AllFiles)?,
            K::TabPr => app.set_tab(crate::app::Tab::Pr)?,
            K::Down => app.move_cursor(1)?,
            K::Up => app.move_cursor(-1)?,
            // `expand`/`collapse` act on a directory or fold, else scroll sideways.
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
            K::Rendered => app.toggle_rendered(),
            K::NavigatorPosition => app.cycle_navigator_position(),
            K::NavigatorHide => app.toggle_navigator_hidden(),
            K::NavigatorGrow => app.resize_navigator(4),
            K::NavigatorShrink => app.resize_navigator(-4),
            K::ScopeUncommitted => app.set_scope(Scope::Uncommitted)?,
            K::ScopeBranch => app.set_scope(Scope::Branch)?,
            K::ScopeLastTurn => app.set_scope(Scope::LastTurn)?,
            K::ScopeCommits => app.set_scope(Scope::Commits)?,
            K::BasePick => app.open_base_picker(),
            K::CommitPick => app.open_commit_picker(),
            K::Select => app.toggle_select(),
            K::ToggleReviewed => app.toggle_current_file_reviewed(),
            K::Comment => app.start_comment(),
            // `delete` needs the diff focused, never an off-screen cursor; `edit` works anywhere.
            K::Edit => app.start_edit(),
            K::Delete if app.focus == Focus::Diff => app.delete_comment(),
            K::Send => app.send_to_agent(),
            K::Copy => {
                app.export(&Clipboard);
            }
            K::NextComment => app.jump_comment(1),
            K::PrevComment => app.jump_comment(-1),
            K::Comments => app.open_list(),
            K::Search => app.open_search(),
            K::Find => app.open_find(),
            K::GotoLine => app.open_line(),
            K::Keys => app.toggle_keys(),
            // Inert here; `quit-discard` only answers the quit question.
            K::Delete | K::OpenPr | K::QuitDiscard => {}
        }
        return Ok(());
    }

    match key.code {
        Tab => app.toggle_focus(),
        // `esc` peels one layer: selection, armed crossing, footer expansion.
        Esc => app.escape(),
        _ => {}
    }
    Ok(())
}

/// Cancel pointer state whose coordinates belonged to the old terminal geometry.
fn handle_resize(app: &mut App) {
    app.cancel_divider_drag();
    // A resize rewraps rows, so it cancels a gesture and clears a settled span.
    app.cancel_gesture();
    app.clear_settled_selection();
    app.hover = None;
}

/// Arm a text gesture on a mouse-down over text; its click count acts at release.
fn handle_text_down(app: &mut App, m: MouseEvent, area: Rect) -> bool {
    use crate::selection::{Gesture, Point, Surface, TextDrag};
    let file_tab = app.tab != crate::app::Tab::Pr;
    let arm = |app: &mut App, surface: Surface, point: Point| {
        let count = app.note_click(m.column, m.row, point.row, surface);
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

/// Extend the text drag to the pointer, edge-scrolling first.
fn text_drag_extend(app: &mut App, m: MouseEvent, area: Rect) {
    text_drag_edge_scroll(app, m, area);
    text_drag_set_extent(app, m, area);
}

/// A drag's vertical scroll: only past `inner`'s rows, so its edge rows stay selectable.
fn edge_delta(row: u16, inner: Rect) -> isize {
    if row < inner.y {
        return -1;
    }
    isize::from(row >= inner.y + inner.height)
}

/// Whether the pointer sits on the pane's edge, the only place a release can get lost.
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
        // Card text ignores h-scroll (TS-ONE-SURFACE).
        Surface::Card { .. } => read_edge_scroll(app, m, area, false),
        Surface::Files => {
            let inner = ui::files_inner_rect(area, app);
            let delta = edge_delta(m.row, inner);
            if inner.height > 0 && delta != 0 {
                app.wheel_files(delta);
            }
        }
        Surface::Painted => {
            // The painted rect, so a `PR` notice above it scrolls instead of dead-zoning.
            let Some(rect) = ui::painted_sel(app, area).map(|s| s.rect) else { return };
            let delta = edge_delta(m.row, rect);
            if rect.height > 0 && delta != 0 {
                app.pr_scroll_read(delta);
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

/// Move the drag's extent to the pointer, clamped to its surface; the wheel skips edge scroll.
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

/// Scroll the read pane while a drag is past its content, and sideways when `horizontal`.
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
    // Rendered markdown never scrolls sideways, so its drag leaves the source's offset alone.
    if horizontal && !app.wrap && !app.rendered_active() {
        if m.column < content.x {
            app.h_scroll = app.h_scroll.saturating_sub(2);
        } else if m.column >= content.x + content.width {
            // Capped at the widest row, never pulled back from a keyboard scroll past it.
            let cap = ui::widest_visible_row(app, area).saturating_sub(1);
            if app.h_scroll < cap {
                app.h_scroll = (app.h_scroll + 2).min(cap);
            }
        }
    }
}

/// Finish a text drag: an unmoved release clicks or multi-click copies, a drag copies.
/// `clicks_act` is false while composing, where only the copies fire.
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
    // A release on the anchor's point is a click: many cells map to one point, so slop is free.
    text_drag_set_extent(app, m, area);
    let drag = app.text_drag().expect("matched above");
    if drag.anchor == drag.extent {
        app.gesture = Gesture::None;
        match drag.surface {
            // A navigator double or triple copies the row; an empty row clicks.
            Surface::Files | Surface::PrNav if count >= 2 => {
                if !multi_click_copy(app, area, drag, target) && clicks_act {
                    perform_click(app, m, area, heights, drag)?;
                }
            }
            // A double copies the word; a wordless cell clicks.
            Surface::Read | Surface::Painted | Surface::Card { .. } if count == 2 => {
                if !word_click_copy(app, area, drag, target) && clicks_act {
                    perform_click(app, m, area, heights, drag)?;
                }
            }
            // A triple copies the source line; an empty line clicks.
            Surface::Read | Surface::Painted | Surface::Card { .. } if count >= 3 => {
                if !line_click_copy(app, area, drag, target) && clicks_act {
                    perform_click(app, m, area, heights, drag)?;
                }
            }
            _ if clicks_act => perform_click(app, m, area, heights, drag)?,
            _ => {}
        }
        // The release continues the multi-click chain; only a non-release end resets it.
        app.catch_up_held_view();
    } else {
        complete_gesture(app, area, target);
    }
    Ok(())
}

/// Copy a navigator row's path or text; whether anything copied.
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
        Some(t) if !t.is_empty() => {
            app.copy_selection_text(target, &t);
            app.settle_selection(drag, t);
            true
        }
        _ => false,
    }
}

/// Copy the word under the cell and settle its highlight; `false` off a word.
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

/// Copy the row's source line and settle its highlight; `false` on an empty line.
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

/// Complete the live gesture: a visible selection copies (`TS-NO-SILENT-LOSS`), else nothing.
pub fn complete_gesture(app: &mut App, area: Rect, target: &dyn crate::export::ExportTarget) {
    if let Some(drag) = app.text_drag()
        && drag.anchor != drag.extent
    {
        let text = drag_text(app, area).unwrap_or_default();
        app.copy_selection_text(target, &text);
        // The copy leaves its span highlighted as feedback.
        app.settle_selection(drag, text);
    }
    app.cancel_gesture();
}

/// The text a span on `surface` copies, for drags and multi-clicks alike.
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
fn perform_click(
    app: &mut App,
    m: MouseEvent,
    area: Rect,
    heights: &[usize],
    drag: crate::selection::TextDrag,
) -> Result<()> {
    use crate::selection::Surface;
    match drag.surface {
        // The clamped row, so the slop that made it a click also delivers it.
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
            } else if let Some(key) = app.painted_details_at(m.column, m.row) {
                app.focus = Focus::Diff;
                app.toggle_details(&key);
            } else if app.tab == crate::app::Tab::Pr {
                // The painted surface has no cursor: a click only focuses the pane.
                if ui::in_diff_pane(area, app, m.column, m.row) {
                    app.focus = Focus::Diff;
                }
            } else if let Some(i) =
                ui::hit_diff(area, app, m.column, m.row, heights, app.diff_scroll)
            {
                app.focus = Focus::Diff;
                app.diff_cursor = i;
                app.select_anchor = None;
                // A click on a card picks its comment, for the `edit`/`delete` that follow.
                if let Surface::Card { comment } = drag.surface {
                    app.target_comment_card(comment);
                }
                app.expand_fold(heights, ui::diff_viewport_height(area, app));
            }
        }
    }
    Ok(())
}

/// Map one mouse event onto `App`, hit-testing the painted frame's `keymap`.
pub fn handle_mouse(
    app: &mut App,
    m: MouseEvent,
    area: Rect,
    heights: &[usize],
    keymap: &Keymap,
    target: &dyn crate::export::ExportTarget,
) -> Result<()> {
    let done = dispatch_mouse(app, m, area, heights, keymap, target);
    app.settle_pick();
    done
}

/// [`handle_mouse`]'s dispatch.
fn dispatch_mouse(
    app: &mut App,
    m: MouseEvent,
    area: Rect,
    heights: &[usize],
    keymap: &Keymap,
    target: &dyn crate::export::ExportTarget,
) -> Result<()> {
    app.hover = Some((m.column, m.row));
    // A click or wheel only answers the quit question; a gesture under way finishes.
    if app.confirming_quit && answers_question(m.kind) {
        app.confirming_quit = false;
        return Ok(());
    }
    // Buttonless motion or a new down proves the release was lost; complete it first.
    if app.gesture_active() && matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Down(_)) {
        complete_gesture(app, area, target);
    }
    // A mouse-down clears the settled highlight, after the proof above.
    if matches!(m.kind, MouseEventKind::Down(_)) {
        app.clear_settled_selection();
    }
    // Search: chips flip, a click picks and a second opens, the divider drags search's share.
    if app.mode == Mode::Search {
        use ui::SearchTarget as T;
        match m.kind {
            MouseEventKind::Drag(MouseButton::Left) if app.divider_drag_active() => {
                // The span the two panes divide, as `search_layout` lays it out.
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

    // A modal captures new gestures; a cancelled divider drag still owns its tail.
    if app.mode.is_modal() {
        // While composing, text still selects; clicks stay inert.
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
            // A click highlights; a click on the highlighted row sends.
            MouseEventKind::Down(MouseButton::Left) if app.mode == Mode::Picker => {
                match ui::hit_picker_row(area, app, m.column, m.row) {
                    Some(i) if i == app.picker_cursor => app.picker_pick(),
                    Some(i) => app.picker_goto(i),
                    None => {}
                }
            }
            // Same shape in the base picker: click to highlight, click the highlight to pick
            MouseEventKind::Down(MouseButton::Left) if app.mode == Mode::BasePick => {
                match ui::hit_base_picker_row(area, app, m.column, m.row) {
                    Some(i) if app.base_picker.as_ref().is_some_and(|bp| bp.cursor == i) => {
                        app.base_picker_pick()?;
                    }
                    Some(i) => app.base_picker_goto(i),
                    None => {}
                }
            }
            // And in the commit picker, the run included.
            MouseEventKind::Down(MouseButton::Left) if app.mode == Mode::CommitPick => {
                match ui::hit_commit_picker_row(area, app, m.column, m.row) {
                    Some(i) if app.commit_picker.as_ref().is_some_and(|cp| cp.cursor == i) => {
                        app.commit_picker_pick()?;
                    }
                    Some(i) => app.commit_picker_goto(i),
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
    // Any mouse gesture drops an armed crossing; bare motion does not.
    if !matches!(m.kind, MouseEventKind::Moved) {
        app.disarm_cross();
    }

    // A cancelled divider drag stays consumed to its mouse-up, never a selection.
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

    // The `PR` tab: click, select and copy, wheel; nothing edits.
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
                    // Armed; the release performs it.
                } else if ui::in_files_pane(area, app, m.column, m.row) {
                    // Blank space below the rows: focus only.
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
            // The wheel mid-drag scrolls its pane and extends the selection.
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
                    ui::HeaderHit::Scope => app.set_scope(app.next_chip_scope())?,
                    // Inert under a `--base` flag.
                    ui::HeaderHit::Base => app.open_base_picker(),
                    ui::HeaderHit::Pick => app.open_commit_picker(),
                }
            } else if let Some(row) = ui::gutter_row_at(area, app, m.column, m.row) {
                // The gutter comments: click or drag, and the composer opens on release.
                app.start_gutter_drag(row);
            } else if handle_text_down(app, m, area) {
                // Armed; the release performs it.
            } else if let Some(i) =
                ui::hit_diff(area, app, m.column, m.row, heights, app.diff_scroll)
            {
                // Only non-text display lines reach here: a fold or a comment card's line.
                app.focus = Focus::Diff;
                app.diff_cursor = i;
                app.select_anchor = None;
                if let Some(comment) = ui::card_at(area, app, m.column, m.row) {
                    app.target_comment_card(comment);
                }
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
        // The wheel scrolls the pane under it, never the cursor; h-scroll is keyboard-only.
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
        glyph_clears, handle_blocked_event, handle_resize, ready_app, schedule_pr_probe,
        world_indicator,
    };
    use crate::app::{App, Tab};

    #[test]
    fn a_recovery_saved_over_twice_recovers_to_the_newest_config() {
        use super::{Recovery, recovery_verdict};
        let dir = tempfile::tempdir().unwrap();
        let read = |body: &str| {
            std::fs::write(dir.path().join("config.toml"), body).unwrap();
            crate::config::plugin_config(Some(dir.path()))
        };
        let target = read("theme = \"nord\"\n").unwrap();
        let same = read("theme = \"nord\"\n");
        assert!(matches!(recovery_verdict(&target, same), Recovery::Swap), "the fix landed");
        let resaved = read("theme = \"gruvbox\"\n");
        assert!(matches!(recovery_verdict(&target, resaved), Recovery::Again), "read again");
        let broken = read("theme = 3\n");
        assert!(matches!(recovery_verdict(&target, broken), Recovery::Invalid(_)), "still broken");
    }

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
                branch: Some("feature".to_string()),
                heads: Vec::new(),
                pin: None,
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

        // A resize copies nothing and clears the settled span.
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

        // Tab entry joins the startup fetch.
        pr.request_refresh(crate::app::RefreshKind::Ambient);
        assert_eq!(pr.refresh.generation, generation, "the ambient trigger joins");
        assert!(pr.refresh.take_fetch().is_none(), "no fetch starts while the ride is on");

        // The ridden completion paints, then exactly one trailing fetch follows.
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

        // The refresh key cancels the in-flight fetch for a fresh one.
        let generation = trailing_generation;
        pr.request_refresh(crate::app::RefreshKind::Forced);
        assert_ne!(pr.refresh.generation, generation);
        let cancelled = pr.active_fetch.as_ref().unwrap().cancelled.clone();
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire), "the old fetch cancels");
        assert!(pr.refresh.take_fetch().is_some(), "the commanded refresh starts fresh work");

        // A hung fetch is abandoned even by an ambient trigger.
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

        // Same path on another forge is another target, so the view clears.
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

        // A new HEAD keeps the painted model and queues a fetch.
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
        let (tx, _rx) = crate::wake::channel(&crate::wake::Waker::detached());
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

        // The frozen draft is unsent, so `q` asks and `Q` quits.
        handle_blocked_event(&mut app, &Event::Key(KeyEvent::from(KeyCode::Char('q'))));
        assert!(app.confirming_quit && !app.should_quit);
        handle_blocked_event(&mut app, &Event::Key(KeyEvent::from(KeyCode::Char('Q'))));
        assert!(app.should_quit);
    }

    #[test]
    fn the_blocked_screen_asks_before_dropping_unsent_comments() {
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        app.store.add(crate::model::Comment {
            file: "a.rs".into(),
            side: crate::model::Side::New,
            start: 1,
            end: 1,
            lines: "+a".into(),
            text: "keep".into(),
            diff_anchored: true,
            rev: crate::model::Rev::Worktree,
        });
        app.set_config_error("invalid config".to_string());
        let q = Event::Key(KeyEvent::from(KeyCode::Char('q')));
        handle_blocked_event(&mut app, &q);
        assert!(app.confirming_quit && !app.should_quit, "the first `q` asks");
        handle_blocked_event(&mut app, &q);
        assert!(app.confirming_quit && !app.should_quit, "a repeated `q` never answers");
        handle_blocked_event(&mut app, &Event::Key(KeyEvent::from(KeyCode::Esc)));
        assert!(!app.confirming_quit && !app.should_quit, "any other key answers and stays");
        handle_blocked_event(&mut app, &q);
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        handle_blocked_event(&mut app, &click);
        assert!(!app.confirming_quit && !app.should_quit, "a click answers and stays");
        handle_blocked_event(&mut app, &q);
        handle_blocked_event(&mut app, &Event::Key(KeyEvent::from(KeyCode::Char('Q'))));
        assert!(app.should_quit, "asked, `Q` quits");

        // A draft frozen under the error screen is unsent too, so its quit asks first.
        let mut app = App::new(std::path::PathBuf::from("."), Scope::Uncommitted, None);
        app.mode = crate::app::Mode::Composing { editing: None };
        app.input = "half a thought".to_string();
        app.set_config_error("invalid config".to_string());
        handle_blocked_event(&mut app, &q);
        assert!(app.confirming_quit && !app.should_quit, "a draft makes `q` ask");
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

        // A head-only change refetches without blanking.
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
        // A failed containment read is a retryable error, never "not contained".
        use super::hold_gate;
        use crate::forge::PrView;
        let fail = |_: &str, _: &str| Err(crate::git::GitFail("rev-list failed".to_string()));
        assert!(matches!(
            hold_gate(PrView::NoPr, Some("oid"), Some("pin"), fail),
            PrView::GitError(message) if message.contains("rev-list failed")
        ));
        // The read runs only when a NoPr could promote.
        assert!(matches!(hold_gate(PrView::NoPr, None, Some("pin"), fail), PrView::NoPr));
        assert!(matches!(
            hold_gate(PrView::Detached, Some("oid"), Some("pin"), fail),
            PrView::Detached
        ));
    }

    #[test]
    fn a_branch_switch_clears_but_a_transient_detach_never_does() {
        // A new branch is identity; a detach in between is freshness.
        let mut on_a = input("head");
        on_a.local.branch = Some("branch-a".to_string());
        let mut on_b = input("head2");
        on_b.local.branch = Some("branch-b".to_string());
        let mut detached = input("head");
        detached.local.branch = None;

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
        // Pins and published heads are freshness: the snapshot stays.
        let original = input("head");
        let mut renamed = input("head");
        renamed.local.heads.push(crate::git::Head {
            repo: crate::git::RepoTarget::new("github.com", "owner", "repo").unwrap(),
            name: "published".to_string(),
        });
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

        // A trailing fetch survives a failed probe as a plain request.
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
        schedule_pr_probe(&mut coordinator, Tab::Changes);
        assert!(!coordinator.probe_pending);

        schedule_pr_probe(&mut coordinator, Tab::Pr);
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
        let (tx, _rx) = crate::wake::channel(&crate::wake::Waker::detached());
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
        let (tx, _rx) = crate::wake::channel(&crate::wake::Waker::detached());
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

        // A reflow or a failed observation ends the gesture with its copy.
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
        let (tx, _rx) = crate::wake::channel(&crate::wake::Waker::detached());
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

        // A theme change ends the gesture before the new frame applies.
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
        let (tx, rx) = crate::wake::channel(&crate::wake::Waker::detached());
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
