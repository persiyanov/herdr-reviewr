//! The plugin actions and the `auto-open` hook, run as `--action <name>`, one workspace lock each.

use std::env;
use std::ffi::OsStr;
use std::fs::{File, TryLockError};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::{PluginConfig, PluginConfigError, TogglePlacement};
use crate::herdr::{self, HerdrError, PaneList, Process, ProcessInfo, var};
use crate::logln;
use crate::proc::program_name;

/// A run that is not the review UI: dispatched by `main`, and never counted as a reviewr pane.
#[derive(Debug, PartialEq, Eq)]
pub enum NonUiRun {
    /// `--action <name>`; a flag that ends argv names the empty action.
    Action(String),
}

impl NonUiRun {
    /// The non-UI run `args` asks for, anywhere in argv, or `None` for the review UI.
    pub fn from_args<S: AsRef<OsStr>>(args: &[S]) -> Option<Self> {
        let args: Vec<&OsStr> = args.iter().map(AsRef::as_ref).collect();
        let at = args.iter().position(|arg| *arg == "--action")?;
        let name = args.get(at + 1).map(|name| name.to_string_lossy().into_owned());
        Some(Self::Action(name.unwrap_or_default()))
    }
}

/// One plugin action. The manifest names each with the same spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Toggle,
    Open,
    Close,
    AutoOpen,
}

impl Action {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "toggle" => Some(Self::Toggle),
            "open" => Some(Self::Open),
            "close" => Some(Self::Close),
            "auto-open" => Some(Self::AutoOpen),
            _ => None,
        }
    }
}

/// Why an action stopped short.
#[derive(Debug)]
enum Stop {
    /// The plugin config is invalid; loud everywhere, so it reaches herdr's plugin log.
    Config(PluginConfigError),
    /// The action cannot proceed. Loud for an explicit action, silent for the event.
    Refused(String),
}

fn refused(why: impl Into<String>) -> Stop {
    Stop::Refused(why.into())
}

/// Run the action `name` and return the process exit code.
pub fn run(name: &str) -> i32 {
    crate::log::init();
    let Some(action) = Action::parse(name) else {
        eprintln!("reviewr: unknown action '{name}' (toggle | open | close | auto-open)");
        return 1;
    };
    match act(action) {
        // The event reports nothing, on success either.
        Ok(None) => 0,
        Ok(Some(line)) if action == Action::AutoOpen => {
            logln!("auto-open: {line}");
            0
        }
        Ok(Some(line)) => {
            println!("reviewr: {line}");
            0
        }
        Err(Stop::Config(error)) => {
            eprintln!("reviewr: {error}");
            1
        }
        Err(Stop::Refused(why)) if action == Action::AutoOpen => {
            logln!("auto-open refused: {why}");
            0
        }
        Err(Stop::Refused(why)) => {
            eprintln!("reviewr: {why}");
            1
        }
    }
}

/// One action, step by step. `Ok` holds the success line, if the action reports one.
fn act(action: Action) -> Result<Option<String>, Stop> {
    // The whole config validates before any workspace read or pane write.
    let config = crate::config::plugin_config_from_herdr().map_err(Stop::Config)?;

    #[cfg(unix)]
    repoint_launch_links();

    // Event policy gates only the event, before any read.
    let event = if action == Action::AutoOpen {
        if !config.auto_open()
            || !matches!(config.toggle_placement(), TogglePlacement::Split | TogglePlacement::Tab)
        {
            return Ok(None);
        }
        // Without a payload the only workspace in reach is the focused one, so the event refuses.
        let Some(json) = var("HERDR_PLUGIN_EVENT_JSON") else {
            return Err(refused("no event payload"));
        };
        let event: Value = serde_json::from_str(&json).unwrap_or_default();
        // `worktree.opened` on a live workspace is no birth: never resurrect a closed pane.
        if event.pointer("/data/already_open") == Some(&Value::Bool(true)) {
            return Ok(None);
        }
        Some(event)
    } else {
        None
    };

    let target = Target::read(event)?;
    let ws = target.ws.as_str();
    // The event gives way at once: a holder is the user's own action in the new workspace.
    let bound = if action == Action::AutoOpen { Duration::ZERO } else { LOCK_BOUND };
    // Held through the close or open, so a concurrent action sees this one's effect.
    let _lock = action_lock(ws, bound)?;

    // One listing serves the run; a failed one never reads as "no reviewr pane".
    let panes =
        PaneList::of(ws).map_err(|_| refused(format!("herdr pane list failed for {ws}")))?;
    let existing = reviewr_panes(&panes)
        .ok_or_else(|| refused(format!("herdr pane process-info failed in {ws}")))?;

    if !existing.is_empty() {
        return match action {
            Action::Close | Action::Toggle => close_all(&existing, ws).map(Some),
            Action::Open | Action::AutoOpen => {
                Ok(Some(format!("already open ({}) in {ws}", existing.join(" "))))
            }
        };
    }
    if action == Action::Close {
        return Ok(Some(format!("nothing open in {ws}")));
    }
    open(action, &config, &target, &panes).map(Some)
}

/// The review binary's name, which identifies a reviewr pane and names its launch links.
const BINARY: &str = env!("CARGO_PKG_NAME");

/// How long an action waits for its workspace's lock: a holder's five herdr calls all wedged, its
/// visibility wait, and three seconds of slack, typically enough for its last poll pause and git reads.
const LOCK_BOUND: Duration = herdr::CALL_BOUND
    .saturating_mul(5)
    .saturating_add(VISIBLE_BOUND)
    .saturating_add(Duration::from_secs(3));

/// The pause between two lock attempts.
const LOCK_POLL: Duration = Duration::from_millis(20);

/// Workspace `ws`'s action lock, waited for up to `bound`.
fn action_lock(ws: &str, bound: Duration) -> Result<File, Stop> {
    let Some(dir) = herdr::var_os("HERDR_PLUGIN_STATE_DIR") else {
        return Err(refused("no plugin state dir (invoke as a herdr plugin action)"));
    };
    // Hex, so any id is a safe file name on every OS.
    let name = hex::encode(ws);
    let path = Path::new(&dir).join(format!("action-{name}.lock"));
    let unusable = |error| refused(format!("cannot lock {}: {error}", path.display()));
    // herdr names the dir but may not have made it yet.
    std::fs::create_dir_all(&dir).map_err(unusable)?;
    // Windows locks need a handle with access; the content is never touched.
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(unusable)?;
    let deadline = Instant::now() + bound;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(LOCK_POLL),
            Err(TryLockError::WouldBlock) => {
                return Err(refused(format!(
                    "another reviewr action in {ws} is still running after {bound:?}"
                )));
            }
            Err(TryLockError::Error(error)) => return Err(unusable(error)),
        }
    }
}

/// The non-empty string at `pointer`; a missing or mistyped field reads as absent.
fn text(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(Value::as_str).filter(|text| !text.is_empty()).map(Into::into)
}

/// Where an action acts.
#[derive(Debug)]
struct Target {
    ws: String,
    /// The pane a split or zoomed open attaches to.
    pane: Option<String>,
    /// The launch cwd to review.
    cwd: Option<String>,
    /// The focused pane, whose live cwd a manual open prefers over `cwd`.
    focused: Option<String>,
}

impl Target {
    /// The event's target from its payload, else the explicit action's.
    fn read(event: Option<Value>) -> Result<Self, Stop> {
        let no_workspace = || refused("no workspace context (invoke from inside herdr)");
        // Target the event's fresh workspace, never a focused pane's cwd.
        if let Some(event) = event {
            return Ok(Self {
                ws: text(&event, "/data/workspace/workspace_id")
                    .or_else(|| text(&event, "/data/worktree/open_workspace_id"))
                    .ok_or_else(no_workspace)?,
                pane: None,
                cwd: text(&event, "/data/workspace/worktree/checkout_path")
                    .or_else(|| text(&event, "/data/worktree/path")),
                focused: None,
            });
        }
        let context: Value = var("HERDR_PLUGIN_CONTEXT_JSON")
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let herdr::PaneIds { workspace: ws, pane } = herdr::PaneIds::from_env();
        Ok(Self {
            ws: ws.ok_or_else(no_workspace)?,
            pane,
            cwd: text(&context, "/focused_pane_cwd").or_else(|| text(&context, "/workspace_cwd")),
            focused: text(&context, "/focused_pane_id"),
        })
    }
}

/// The workspace's reviewr panes, read concurrently; `None` when any read failed.
fn reviewr_panes(panes: &PaneList) -> Option<Vec<&str>> {
    let ids: Vec<&str> = panes.panes.iter().map(|entry| entry.pane_id.as_str()).collect();
    let mut existing = Vec::new();
    for (pane, probe) in per_pane(&ids, runs_review_ui) {
        // A probe that failed, never ran, or panicked never settled, so the sweep refuses; a pane
        // gone since the list counts as closed.
        if probe?.ok()? == PaneRun::Review {
            existing.push(pane);
        }
    }
    Some(existing)
}

/// `f` on every pane at once; `None` where its thread never ran or panicked.
fn per_pane<'p, R: Send>(
    panes: &[&'p str],
    f: impl Fn(&str) -> Result<R, HerdrError> + Sync,
) -> Vec<(&'p str, Option<Result<R, HerdrError>>)> {
    let f = &f;
    thread::scope(|scope| {
        let runs: Vec<_> = panes
            .iter()
            .map(|&pane| (pane, thread::Builder::new().spawn_scoped(scope, move || f(pane))))
            .collect();
        runs.into_iter()
            .map(|(pane, run)| (pane, run.ok().and_then(|run| run.join().ok())))
            .collect()
    })
}

/// What a pane runs, as one probe reads it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneRun {
    Review,
    Other,
    /// Gone since the list.
    Gone,
}

/// What pane `pane` runs now.
fn runs_review_ui(pane: &str) -> Result<PaneRun, HerdrError> {
    match ProcessInfo::of(pane) {
        Ok(info) if info.foreground_processes.iter().any(is_review_ui) => Ok(PaneRun::Review),
        Ok(_) => Ok(PaneRun::Other),
        Err(HerdrError::PaneGone) => Ok(PaneRun::Gone),
        Err(error) => Err(error),
    }
}

/// Whether `process` is the review UI, by its executable name and a UI argv.
fn is_review_ui(process: &Process) -> bool {
    let argv = process.argv.as_slice();
    // Windows names ignore case, so `HERDR-REVIEWR.EXE` is the same program; unix names don't.
    let same =
        |name: &str| if cfg!(windows) { name.eq_ignore_ascii_case(BINARY) } else { name == BINARY };
    let named = process.argv0.iter().chain(argv.first()).any(|exe| same(program_name(exe)));
    named && NonUiRun::from_args(argv.get(1..).unwrap_or_default()).is_none()
}

/// Close every pane in `existing` concurrently; a pane already gone counts as closed.
fn close_all(existing: &[&str], ws: &str) -> Result<String, Stop> {
    // A pane that closed itself meanwhile is closed all the same.
    let failed: Vec<&str> = per_pane(existing, herdr::close_pane)
        .into_iter()
        .filter(|(_, close)| !matches!(close, Some(Ok(()) | Err(HerdrError::PaneGone))))
        .map(|(pane, _)| pane)
        .collect();
    if !failed.is_empty() {
        return Err(refused(format!("herdr pane close failed for {} in {ws}", failed.join(" "))));
    }
    Ok(format!("closed {} in {ws}", existing.join(" ")))
}

/// How long an open waits for its pane to read as reviewr: a cold Windows open, with room.
const VISIBLE_BOUND: Duration = Duration::from_secs(6);

/// The pause between two reads of the new pane; Windows herdr caches its process snapshot 250 ms.
const VISIBLE_POLL: Duration = Duration::from_millis(if cfg!(windows) { 250 } else { 50 });

/// Open a reviewr pane in the target's workspace and return the success line.
fn open(
    action: Action,
    config: &PluginConfig,
    target: &Target,
    panes: &PaneList,
) -> Result<String, Stop> {
    let ws = target.ws.as_str();
    // The focused pane's live cwd wins over the launch cwd, when it is inside a repo.
    let live = target
        .focused
        .as_deref()
        .and_then(|focused| panes.pane(focused))
        .and_then(|entry| entry.foreground_cwd.clone());
    let cwd = match (&live, &target.cwd) {
        (Some(live), _) if has_worktree(live) => live,
        (_, Some(cwd)) if has_worktree(cwd) => cwd,
        // Name every rejected candidate.
        _ => {
            let live = live.map(|live| format!(" (live cwd '{live}')")).unwrap_or_default();
            let cwd = target.cwd.as_deref().unwrap_or("<no cwd>");
            return Err(refused(format!("not a git repo: '{cwd}'{live}")));
        }
    };

    let plugin = herdr::plugin_id();
    let placement = config.toggle_placement();
    // A split or zoomed open attaches to the focused pane, else the workspace's first pane.
    let attach = || {
        target
            .pane
            .as_deref()
            .or_else(|| panes.panes.first().map(|entry| entry.pane_id.as_str()))
            .ok_or_else(|| refused(format!("no pane to attach to in {ws}")))
    };
    let spot = match placement {
        TogglePlacement::Split => {
            herdr::Spot::Split { target: attach()?, direction: config.toggle_direction() }
        }
        TogglePlacement::Zoomed => herdr::Spot::Zoomed { target: attach()? },
        TogglePlacement::Tab => herdr::Spot::Tab { workspace: ws },
        TogglePlacement::Overlay => herdr::Spot::Overlay,
    };
    // A manual open takes focus. The event never does.
    let open = herdr::PaneOpen { plugin: &plugin, spot, cwd, focus: action != Action::AutoOpen };
    let opened =
        herdr::open_plugin_pane(&open).map_err(|_| refused("herdr plugin pane open failed"))?;

    // Name a fresh tab after the plugin; cosmetic, so its failure is ignored.
    if placement == TogglePlacement::Tab
        && let Some(tab) = opened.tab_id.as_deref()
    {
        let _ = herdr::rename_tab(tab, herdr::LABEL);
        if open.focus && herdr::focus_tab(tab).is_err() {
            logln!("focusing tab {tab} failed");
        }
    }

    let success = format!("opened {} ({}) in {ws}", opened.pane_id, placement.as_str());
    match launch(&opened.pane_id) {
        Launch::Running => Ok(success),
        Launch::Unseen => Ok(format!("{success}, not yet seen running")),
        Launch::Exited => Err(refused(format!("pane {} exited at launch in {ws}", opened.pane_id))),
    }
}

/// Whether `dir` sits in a worktree; a `.git` dir or bare repository does not.
fn has_worktree(dir: &str) -> bool {
    crate::git::toplevel(Path::new(dir)).is_some()
}

/// What an opened pane showed within [`VISIBLE_BOUND`].
enum Launch {
    Running,
    /// Never read as reviewr in time, nor gone.
    Unseen,
    Exited,
}

/// Read pane `pane` until it runs reviewr, is gone, or [`VISIBLE_BOUND`] passes.
fn launch(pane: &str) -> Launch {
    let deadline = Instant::now() + VISIBLE_BOUND;
    loop {
        match runs_review_ui(pane) {
            Ok(PaneRun::Review) => return Launch::Running,
            Ok(PaneRun::Gone) => return Launch::Exited,
            _ => {}
        }
        if Instant::now() >= deadline {
            logln!("opened pane {pane} not seen as reviewr after {VISIBLE_BOUND:?}");
            return Launch::Unseen;
        }
        thread::sleep(VISIBLE_POLL);
    }
}

/// Re-point the stable launch links at the live plugin root; best effort, symlinks only.
#[cfg(unix)]
fn repoint_launch_links() {
    let (Some(root), Some(home)) = (herdr::var_os("HERDR_PLUGIN_ROOT"), dirs::home_dir()) else {
        return;
    };
    let binary = Path::new(&root).join("bin").join(BINARY);
    if which::which(&binary).is_err() {
        return;
    }
    // The installer's own path, which it writes without herdr's environment.
    let state_bin = home.join(".local/state/herdr/plugins").join(herdr::PLUGIN_ID).join("bin");
    let local_bin = home.join(".local/bin");
    // `~/.local/bin` only when it already exists: reviewr never creates a PATH directory.
    let dirs = [Some(state_bin), local_bin.is_dir().then_some(local_bin)];
    for dir in dirs.into_iter().flatten() {
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let link = dir.join(BINARY);
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if std::fs::read_link(&link).is_ok_and(|target| target == binary) {
                    continue;
                }
            }
            Ok(_) => continue,
            Err(_) => {}
        }
        // A fresh link renamed over the old one, so the path is never missing.
        let fresh = dir.join(format!(".{BINARY}.{}", std::process::id()));
        let _ = std::fs::remove_file(&fresh);
        if std::os::unix::fs::symlink(&binary, &fresh).is_ok()
            && std::fs::rename(&fresh, &link).is_err()
        {
            let _ = std::fs::remove_file(&fresh);
        }
    }
}
