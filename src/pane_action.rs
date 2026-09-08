//! The pane-action state machine: `toggle` / `open` / `close`, and the `worktree.created` /
//! `worktree.opened` auto-open events. One core for every platform, dispatched from `main.rs`
//! as an internal `--pane-action <mode>` command — see `herdr-plugin.toml`.
//!
//! Ported from `herdr/pane.sh` so the branching, error semantics, and pane-identity rules exist
//! once instead of once per platform (`docs/specs/2026-09-08-windows-support/spec.md`). Every
//! comment here calling out a rule is preserving something `pane.sh` already enforced; this file
//! is not free to relax any of them.

use std::env;
use std::path::Path;
use std::thread;

use serde::Deserialize;

use crate::config::{self, TogglePlacement};

/// Recognized anywhere in argv, mirroring `--resolve-plugin-config`: a process invoked with
/// this flag is orchestration, never the review UI, so it must never count as one
/// ([`is_reviewr_pane`]) and `main.rs` must never start the TUI for it.
pub const FLAG: &str = "--pane-action";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Toggle,
    Open,
    Close,
    AutoOpen,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "toggle" => Self::Toggle,
            "open" => Self::Open,
            "close" => Self::Close,
            "auto-open" => Self::AutoOpen,
            _ => return None,
        })
    }

    fn is_auto_open(self) -> bool {
        self == Self::AutoOpen
    }
}

/// A manual action's refusal, or any mode's config-validation failure, prints one stderr line
/// and exits 1. An auto-open event's *runtime* gate (disabled, wrong placement, already-live
/// workspace, or any refusal reached past validation) is silent instead.
/// [`Refusal::for_mode`] is the one place that distinction gets made.
enum Refusal {
    Silent,
    Loud(String),
}

impl Refusal {
    fn for_mode(mode: Mode, message: String) -> Self {
        if mode.is_auto_open() { Self::Silent } else { Self::Loud(message) }
    }
}

type ActionResult<T> = Result<T, Refusal>;

/// Run one pane action to completion and exit — the entrypoint `main.rs` calls for
/// `--pane-action <mode>`. Mirrors `pane.sh`'s exit contract exactly: success prints its
/// message (or nothing, for an event) and exits 0; a loud refusal prints one `reviewr: ...`
/// stderr line and exits 1; a silent refusal exits 0 with no output.
pub fn run(mode_arg: &str) -> ! {
    crate::log::init();
    match execute(mode_arg) {
        Ok(Some(message)) => {
            println!("{message}");
            std::process::exit(0);
        }
        Ok(None) | Err(Refusal::Silent) => std::process::exit(0),
        Err(Refusal::Loud(message)) => {
            eprintln!("reviewr: {message}");
            std::process::exit(1);
        }
    }
}

fn execute(mode_arg: &str) -> ActionResult<Option<String>> {
    // Validate the whole plugin config before reading workspace state or taking any action —
    // before even checking the mode is recognized, matching `pane.sh`'s unconditional
    // validate-first order exactly, so a broken config is never masked by an unrelated
    // argument mistake.
    let dir = config::resolve_config_dir(crate::herdr::plugin_config_dir);
    let cfg =
        config::plugin_config(dir.as_deref()).map_err(|error| Refusal::Loud(error.to_string()))?;

    let Some(mode) = Mode::parse(mode_arg) else {
        return Err(Refusal::Loud(format!(
            "unknown mode '{mode_arg}' (toggle | open | close | auto-open)"
        )));
    };

    // Best effort, Unix only, never fails the action: repoint the stable launch paths at the
    // live plugin root, since the install's build step runs in a staging checkout herdr
    // renames afterwards.
    #[cfg(unix)]
    repair_stable_launch_paths();

    // Event policy gates the event alone: explicit actions ignore it. After validation, before
    // any workspace or pane inspection, so a disabled event does no normal work.
    if mode.is_auto_open() {
        if !cfg.auto_open() {
            return Err(Refusal::Silent);
        }
        if !matches!(cfg.toggle_placement(), TogglePlacement::Split | TogglePlacement::Tab) {
            return Err(Refusal::Silent);
        }
        // `worktree.opened` also fires when its workspace is already live. That is a
        // focus/open request, not a workspace birth: never resurrect a pane the user closed.
        if event_already_open() {
            return Err(Refusal::Silent);
        }
    }

    let mut workspace = env::var("HERDR_WORKSPACE_ID").unwrap_or_default();
    let mut pane_id = env::var("HERDR_PANE_ID").unwrap_or_default();
    // Parsed once and threaded through, rather than each reader re-reading and re-parsing the
    // same env var independently.
    let context = parsed_context();
    let mut cwd = context_cwd(context.as_ref());

    // The events fire without a focused pane; target the fresh workspace from their payload.
    if mode.is_auto_open()
        && let Ok(event) = env::var("HERDR_PLUGIN_EVENT_JSON")
        && !event.is_empty()
    {
        let target = parse_event_target(&event);
        workspace = target.workspace.unwrap_or_default();
        cwd = target.cwd;
        pane_id = String::new();
    }

    if workspace.is_empty() {
        return Err(Refusal::for_mode(
            mode,
            "no workspace context (invoke from inside herdr)".to_string(),
        ));
    }

    // One pane-list snapshot serves the whole run. A failed or unreadable listing must not
    // read as "no reviewr pane" — that would stack a duplicate on toggle and false-succeed a
    // close.
    let panes = list_panes(&workspace)
        .map_err(|()| Refusal::for_mode(mode, format!("herdr pane list failed for {workspace}")))?;

    let (existing, unreadable) = probe_panes(&panes);
    if unreadable {
        return Err(Refusal::for_mode(
            mode,
            format!("herdr pane process-info failed in {workspace}"),
        ));
    }

    match mode {
        Mode::Close => {
            if existing.is_empty() {
                return Ok(Some(format!("close: nothing open in {workspace}")));
            }
            return close_and_report(mode, &existing, &workspace);
        }
        Mode::Toggle if !existing.is_empty() => {
            return close_and_report(mode, &existing, &workspace);
        }
        Mode::Open | Mode::AutoOpen if !existing.is_empty() => {
            if mode == Mode::Open {
                let names = existing.join(" ");
                return Ok(Some(format!("open: already open ({names}) in {workspace}")));
            }
            return Ok(None);
        }
        _ => {}
    }

    // Opening from here on. Prefer the focused pane's live `foreground_cwd`, read from the
    // pane-list snapshot already in hand, over the context's launch cwd. Auto-open keeps the
    // event payload's cwd set above — this block never runs for it.
    let live_cwd =
        if mode.is_auto_open() { None } else { focused_pane_live_cwd(context.as_ref(), &panes) };
    if let Some(live) = live_cwd.as_deref().filter(|c| is_git_repo(c)) {
        cwd = Some(live.to_string());
    } else if !cwd.as_deref().is_some_and(is_git_repo) {
        // Name every candidate the check rejected, or a refusal over an inspected-but-unusable
        // live cwd would read as if no directory was ever tried.
        let named = cwd.as_deref().unwrap_or("<no cwd>");
        let live_note = live_cwd.map(|live| format!(" (live cwd '{live}')")).unwrap_or_default();
        return Err(Refusal::for_mode(mode, format!("not a git repo: '{named}'{live_note}")));
    }
    let cwd = cwd.expect("the git-repo check above guarantees a cwd here");

    // A manual open takes focus. The event never does.
    let focus = if mode.is_auto_open() { "--no-focus" } else { "--focus" };

    let placement_args =
        placement_args(cfg.toggle_placement(), cfg.toggle_direction(), &pane_id, &panes)
            .map_err(|message| Refusal::for_mode(mode, format!("{message} in {workspace}")))?;

    let opened = open_pane(&placement_args, &cwd, focus, &workspace)
        .map_err(|()| Refusal::for_mode(mode, "herdr plugin pane open failed".to_string()))?;

    // A tab open lands in a fresh tab herdr labels with a bare index; name it after the
    // plugin so the tab bar reads "reviewr". Cosmetic: a failed rename never fails an open
    // that already succeeded.
    if matches!(cfg.toggle_placement(), TogglePlacement::Tab)
        && let Some(tab_id) = &opened.tab_id
    {
        let _ = run_herdr(&["tab", "rename", tab_id, "reviewr"]);
    }

    if mode.is_auto_open() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "opened {} ({}) in {workspace}",
            opened.pane_id,
            cfg.toggle_placement().as_str()
        )))
    }
}

// --- herdr CLI boundary -----------------------------------------------------------------

fn herdr_bin() -> String {
    env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

/// Run a herdr subcommand. `Ok` is stdout on a zero exit; `Err` is stderr (or the spawn
/// error) on any other outcome — callers that need to tell "the pane is already gone" apart
/// from "the read failed some other way" (`pane_not_found`) inspect it, everyone else just
/// refuses.
fn run_herdr(args: &[&str]) -> Result<String, String> {
    match crate::proc::command(herdr_bin()).args(args).output() {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        Err(error) => Err(error.to_string()),
    }
}

// --- Pane listing and identity ----------------------------------------------------------

struct PaneEntry {
    pane_id: String,
    foreground_cwd: Option<String>,
}

#[derive(Deserialize)]
struct PaneListResponse {
    result: PaneListResult,
}
#[derive(Deserialize)]
struct PaneListResult {
    panes: Vec<PaneListEntry>,
}
#[derive(Deserialize)]
struct PaneListEntry {
    pane_id: String,
    #[serde(default)]
    foreground_cwd: Option<String>,
}

fn list_panes(workspace: &str) -> Result<Vec<PaneEntry>, ()> {
    let json = run_herdr(&["pane", "list", "--workspace", workspace]).map_err(|_| ())?;
    let parsed: PaneListResponse = serde_json::from_str(&json).map_err(|_| ())?;
    Ok(parsed
        .result
        .panes
        .into_iter()
        .map(|entry| PaneEntry { pane_id: entry.pane_id, foreground_cwd: entry.foreground_cwd })
        .collect())
}

#[derive(Deserialize)]
struct ProcessInfoResponse {
    result: ProcessInfoResult,
}
#[derive(Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}
#[derive(Deserialize)]
struct ProcessInfo {
    foreground_processes: Vec<ForegroundProcess>,
}
#[derive(Deserialize)]
struct ForegroundProcess {
    #[serde(default)]
    argv0: Option<String>,
    #[serde(default)]
    argv: Vec<String>,
}

enum Probe {
    Reviewr,
    NotReviewr,
    Unreadable,
}

/// Whether `path` (an `argv0` or `argv[0]`) names the review binary: its executable identity,
/// never a process title or pane label. Tolerant of `/` and `\` separators, an optional
/// `\\?\` extended-length prefix, and an optional `.exe` suffix — Windows can hand back any
/// of those (`docs/specs/2026-09-08-windows-support/spec.md`).
fn names_reviewr_binary(path: &str) -> bool {
    let stripped = path.strip_prefix(r"\\?\").unwrap_or(path);
    let base = stripped.rsplit(['/', '\\']).next().unwrap_or(stripped);
    // Compares both forms case-insensitively rather than stripping a literal ".exe"/".EXE"
    // suffix first: a suffix strip only catches those two exact casings, so a mixed-case
    // extension (e.g. ".Exe") would silently fail to match.
    base.eq_ignore_ascii_case("herdr-reviewr") || base.eq_ignore_ascii_case("herdr-reviewr.exe")
}

/// A process running an internal, non-UI command (`--resolve-plugin-config`, `--pane-action`)
/// must not count as the review UI — the flag exclusion mirrors `main.rs`'s dispatch; a
/// future non-UI flag must land in both halves.
fn is_internal_command(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg == "--resolve-plugin-config" || arg == FLAG)
}

/// Windows has no process-image-replacement primitive (unlike Unix `exec`, which the Unix pane
/// launcher uses so the shell becomes the review binary in place, same pid). A Windows child
/// process is always genuinely separate, so the Windows pane launcher
/// (`herdr-plugin.toml`'s `pane-windows` entry) runs as a `powershell.exe` wrapper around the
/// real binary, and `herdr pane process-info` reports only that wrapper as the pane's
/// foreground process — confirmed live (`docs/specs/2026-09-08-windows-support/spec.md`):
/// `argv0` is `powershell.EXE`, never `herdr-reviewr.exe`. The real command survives, literally
/// unexpanded, inside the wrapper's `-Command` argv element, since Herdr passes argv with no
/// shell expansion and the reported command line is fixed at process creation. Recognize that
/// exact launcher shape as the review UI too, on Windows only — narrow enough (both the plugin
/// root env var reference and the binary name must appear together) that it cannot match an
/// unrelated process that merely mentions the binary's name.
#[cfg(windows)]
fn is_windows_pane_launcher(argv: &[String]) -> bool {
    argv.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        lower.contains("$env:herdr_plugin_root") && lower.contains("herdr-reviewr.exe")
    })
}

fn is_reviewr_pane(processes: &[ForegroundProcess]) -> bool {
    processes.iter().any(|process| {
        let identified = process.argv0.as_deref().is_some_and(names_reviewr_binary)
            || process.argv.first().is_some_and(|argv0| names_reviewr_binary(argv0));
        #[cfg(windows)]
        let identified = identified || is_windows_pane_launcher(&process.argv);
        identified && !is_internal_command(&process.argv)
    })
}

/// Probe every listed pane concurrently — one process-info round trip of wall clock, not one
/// per pane — and report the reviewr-matching subset in the list's own order plus whether any
/// probe was unreadable. A gone pane (`pane_not_found`) reads as "not a reviewr pane", never
/// as unreadable: it exited between the list and this read, and a close on it converges like
/// any observed-then-exited pane.
fn probe_panes(panes: &[PaneEntry]) -> (Vec<String>, bool) {
    let handles: Vec<_> = panes
        .iter()
        .map(|pane| {
            let pane_id = pane.pane_id.clone();
            thread::spawn(move || probe_one_pane(&pane_id))
        })
        .collect();

    let mut existing = Vec::new();
    let mut unreadable = false;
    for (pane, handle) in panes.iter().zip(handles) {
        match handle.join().unwrap_or(Probe::Unreadable) {
            Probe::Reviewr => existing.push(pane.pane_id.clone()),
            Probe::NotReviewr => {}
            Probe::Unreadable => unreadable = true,
        }
    }
    (existing, unreadable)
}

fn probe_one_pane(pane_id: &str) -> Probe {
    match run_herdr(&["pane", "process-info", "--pane", pane_id]) {
        Ok(json) => match serde_json::from_str::<ProcessInfoResponse>(&json) {
            Ok(parsed) => {
                if is_reviewr_pane(&parsed.result.process_info.foreground_processes) {
                    Probe::Reviewr
                } else {
                    Probe::NotReviewr
                }
            }
            // An envelope missing `foreground_processes` is a shape failure and must refuse,
            // never read as "no reviewr pane" — only a present-but-empty list may count zero.
            Err(_) => Probe::Unreadable,
        },
        Err(stderr) if stderr.contains("pane_not_found") => Probe::NotReviewr,
        Err(_) => Probe::Unreadable,
    }
}

// --- Close ---------------------------------------------------------------------------------

/// Close every pane in `existing`, in order. Plain `pane close`, not `plugin pane close`: the
/// live process read reaches a pane the plugin-pane registry forgot after a herdr restart, and
/// a layout-launched pane was never in that registry at all. `pane_not_found` on a close is a
/// benign race — the pane exited between the read and the close, the same end state — so the
/// sweep still converges; any other failure names a pane that may still be running, so the
/// sweep finishes the rest and then reports it.
fn close_all(existing: &[String], workspace: &str) -> Result<String, String> {
    use std::fmt::Write as _;

    let mut closed = String::new();
    let mut failed = String::new();
    for pane_id in existing {
        match run_herdr(&["pane", "close", pane_id]) {
            Ok(_) => {
                let _ = write!(closed, " {pane_id}");
            }
            Err(stderr) if stderr.contains("pane_not_found") => {
                let _ = write!(closed, " {pane_id}");
            }
            Err(_) => {
                let _ = write!(failed, " {pane_id}");
            }
        }
    }
    if failed.is_empty() { Ok(format!("closed{closed} in {workspace}")) } else { Err(failed) }
}

/// [`close_all`], wrapped into a mode-aware `ActionResult` — shared by `close` and a `toggle`
/// that found an existing pane, so the failure-message wrapping can't drift between the two.
fn close_and_report(
    mode: Mode,
    existing: &[String],
    workspace: &str,
) -> ActionResult<Option<String>> {
    close_all(existing, workspace).map(Some).map_err(|failed| {
        Refusal::for_mode(mode, format!("herdr pane close failed for{failed} in {workspace}"))
    })
}

// --- Open cwd resolution ---------------------------------------------------------------

fn is_git_repo(path: &str) -> bool {
    !path.is_empty()
        && matches!(crate::git::worktree_of(Path::new(path)), crate::git::Worktree::Root(_))
}

/// `HERDR_PLUGIN_CONTEXT_JSON`, read and parsed once per run and threaded to every reader —
/// `context_cwd` and `focused_pane_live_cwd` both need it, and neither should re-read the env
/// var or re-parse the JSON independently.
fn parsed_context() -> Option<serde_json::Value> {
    let context = env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
    serde_json::from_str(&context).ok()
}

fn focused_pane_live_cwd(
    context: Option<&serde_json::Value>,
    panes: &[PaneEntry],
) -> Option<String> {
    let focused_pane_id = context?.get("focused_pane_id")?.as_str()?;
    panes.iter().find(|pane| pane.pane_id == focused_pane_id)?.foreground_cwd.clone()
}

fn context_cwd(context: Option<&serde_json::Value>) -> Option<String> {
    let value = context?;
    value
        .get("focused_pane_cwd")
        .or_else(|| value.get("workspace_cwd"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

struct EventTarget {
    workspace: Option<String>,
    cwd: Option<String>,
}

fn parse_event_target(event: &str) -> EventTarget {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(event) else {
        return EventTarget { workspace: None, cwd: None };
    };
    let data = value.get("data");
    let workspace = data
        .and_then(|d| d.pointer("/workspace/workspace_id"))
        .or_else(|| data.and_then(|d| d.pointer("/worktree/open_workspace_id")))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let cwd = data
        .and_then(|d| d.pointer("/workspace/worktree/checkout_path"))
        .or_else(|| data.and_then(|d| d.pointer("/worktree/path")))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    EventTarget { workspace, cwd }
}

fn event_already_open() -> bool {
    let Ok(event) = env::var("HERDR_PLUGIN_EVENT_JSON") else { return false };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&event) else { return false };
    value.pointer("/data/already_open").and_then(serde_json::Value::as_bool).unwrap_or(false)
}

// --- Placement and opening ---------------------------------------------------------------

fn placement_args(
    placement: TogglePlacement,
    direction: config::ToggleDirection,
    pane_id: &str,
    panes: &[PaneEntry],
) -> Result<Vec<String>, String> {
    match placement {
        TogglePlacement::Split | TogglePlacement::Zoomed => {
            let target = if pane_id.is_empty() {
                panes.first().map(|pane| pane.pane_id.clone())
            } else {
                Some(pane_id.to_string())
            };
            let Some(target) = target else {
                return Err("no pane to attach to".to_string());
            };
            let mut args = vec![
                "--placement".to_string(),
                placement.as_str().to_string(),
                "--target-pane".to_string(),
                target,
            ];
            if matches!(placement, TogglePlacement::Split) {
                args.push("--direction".to_string());
                args.push(direction.as_str().to_string());
            }
            Ok(args)
        }
        TogglePlacement::Tab => Ok(vec!["--placement".to_string(), "tab".to_string()]),
        TogglePlacement::Overlay => Ok(vec!["--placement".to_string(), "overlay".to_string()]),
    }
}

struct OpenedPane {
    pane_id: String,
    tab_id: Option<String>,
}

#[derive(Deserialize)]
struct OpenPaneResponse {
    result: OpenPaneResult,
}
#[derive(Deserialize)]
struct OpenPaneResult {
    plugin_pane: OpenPluginPane,
}
#[derive(Deserialize)]
struct OpenPluginPane {
    pane: OpenPane,
}
#[derive(Deserialize)]
struct OpenPane {
    pane_id: String,
    #[serde(default)]
    tab_id: Option<String>,
}

fn open_pane(
    placement_args: &[String],
    cwd: &str,
    focus: &str,
    workspace: &str,
) -> Result<OpenedPane, ()> {
    let plugin_id =
        env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "persiyanov.reviewr".to_string());
    // Herdr requires unique pane ids even across different `platforms` scopes in the same
    // manifest (confirmed empirically — spec.md), so the pane entrypoint is named per platform.
    let entrypoint = if cfg!(windows) { "pane-windows" } else { "pane-unix" };
    let mut args: Vec<&str> =
        vec!["plugin", "pane", "open", "--plugin", &plugin_id, "--entrypoint", entrypoint];
    args.extend(placement_args.iter().map(String::as_str));
    // Tab placement targets the workspace, not a pane.
    let workspace_arg;
    if placement_args.first().map(String::as_str) == Some("--placement")
        && placement_args.get(1).map(String::as_str) == Some("tab")
    {
        workspace_arg = workspace.to_string();
        args.push("--workspace");
        args.push(&workspace_arg);
    }
    args.push("--cwd");
    args.push(cwd);
    args.push(focus);

    let json = run_herdr(&args).map_err(|_| ())?;
    let parsed: OpenPaneResponse = serde_json::from_str(&json).map_err(|_| ())?;
    if parsed.result.plugin_pane.pane.pane_id.is_empty() {
        return Err(());
    }
    Ok(OpenedPane {
        pane_id: parsed.result.plugin_pane.pane.pane_id,
        tab_id: parsed.result.plugin_pane.pane.tab_id,
    })
}

// --- Stable launch path repair (Unix only) -----------------------------------------------

/// Best effort, never fails the action, never replaces anything but a symlink: repoint the
/// stable launch paths at the live plugin root. The install's build step runs in a staging
/// checkout herdr renames afterwards, so only a runtime invocation knows the real root.
#[cfg(unix)]
fn repair_stable_launch_paths() {
    use std::os::unix::fs::PermissionsExt;

    let Ok(plugin_root) = env::var("HERDR_PLUGIN_ROOT") else { return };
    let target = Path::new(&plugin_root).join("bin/herdr-reviewr");
    let executable = std::fs::metadata(&target)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
    if !executable {
        return;
    }
    let Ok(home) = env::var("HOME") else { return };
    let home = Path::new(&home);
    let candidates =
        [home.join(".local/state/herdr/plugins/persiyanov.reviewr/bin"), home.join(".local/bin")];
    for (index, link_dir) in candidates.iter().enumerate() {
        // `~/.local/bin` only when it already exists — never created for the link alone.
        if index == 1 && !link_dir.is_dir() {
            continue;
        }
        if std::fs::create_dir_all(link_dir).is_err() {
            continue;
        }
        let link = link_dir.join("herdr-reviewr");
        let is_symlink_or_absent =
            std::fs::symlink_metadata(&link).map_or(true, |meta| meta.file_type().is_symlink());
        if is_symlink_or_absent {
            let _ = std::fs::remove_file(&link);
            let _ = std::os::unix::fs::symlink(&target, &link);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ForegroundProcess, is_reviewr_pane, names_reviewr_binary, parse_event_target};

    fn process(argv0: &str, argv: &[&str]) -> ForegroundProcess {
        ForegroundProcess {
            argv0: Some(argv0.to_string()),
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn identity_matches_unix_and_windows_shapes() {
        assert!(names_reviewr_binary("herdr-reviewr"));
        assert!(names_reviewr_binary("/plugin/bin/herdr-reviewr"));
        assert!(names_reviewr_binary(r"C:\plugin\bin\herdr-reviewr.exe"));
        assert!(names_reviewr_binary(r"\\?\C:\plugin\bin\herdr-reviewr.EXE"));
        assert!(names_reviewr_binary(r"C:\plugin\bin\herdr-reviewr.Exe"), "mixed-case suffix");
        assert!(names_reviewr_binary("target/debug/herdr-reviewr"));
        assert!(!names_reviewr_binary("zsh"));
        assert!(!names_reviewr_binary("herdr-reviewr-other"));
    }

    #[test]
    #[cfg(windows)]
    fn the_windows_pane_launcher_counts_even_though_only_powershell_is_reported() {
        // Captured live from a real pane opened through herdr-plugin.toml's `pane-windows`
        // entry (docs/specs/2026-09-08-windows-support/spec.md): `pane process-info` reports
        // only `powershell.EXE` — never `herdr-reviewr.exe` — because Windows has no
        // process-image-replacement primitive, unlike the Unix launcher's `exec`.
        let processes = vec![ForegroundProcess {
            argv0: Some(r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.EXE".to_string()),
            argv: vec![
                r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.EXE".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                r#"& "$env:HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe""#.to_string(),
            ],
        }];
        assert!(is_reviewr_pane(&processes));
    }

    #[test]
    #[cfg(windows)]
    fn an_unrelated_powershell_pane_does_not_count() {
        let processes = vec![process(
            r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.EXE",
            &[r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.EXE"],
        )];
        assert!(!is_reviewr_pane(&processes));
    }

    /// Ties `is_windows_pane_launcher`'s string match directly to the real manifest's
    /// `pane-windows` command, rather than to a hand-copied literal — so if either the
    /// manifest's launcher shape or the matcher itself drifts, this fails immediately instead
    /// of both silently agreeing with themselves while disagreeing with reality.
    #[test]
    #[cfg(windows)]
    fn the_matcher_recognizes_the_real_manifest_pane_windows_command() {
        let manifest_text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin.toml"),
        )
        .unwrap();
        let manifest: toml::Table = manifest_text.parse().unwrap();
        let panes = manifest.get("panes").and_then(toml::Value::as_array).expect("panes");
        let windows_pane = panes
            .iter()
            .find(|p| p.get("id").and_then(toml::Value::as_str) == Some("pane-windows"))
            .expect("pane-windows entry");
        let command: Vec<String> = windows_pane
            .get("command")
            .and_then(toml::Value::as_array)
            .expect("command")
            .iter()
            .map(|v| v.as_str().expect("string arg").to_string())
            .collect();

        assert!(
            super::is_windows_pane_launcher(&command),
            "matcher must recognize the real manifest command: {command:?}"
        );
    }

    #[test]
    fn a_wrapped_launch_counts_through_its_child() {
        let processes = vec![
            process("cargo", &["cargo", "run"]),
            process("target/debug/herdr-reviewr", &["target/debug/herdr-reviewr"]),
        ];
        assert!(is_reviewr_pane(&processes));
    }

    #[test]
    fn a_flag_run_never_counts_as_the_review_ui() {
        let processes =
            vec![process("herdr-reviewr", &["herdr-reviewr", "--resolve-plugin-config"])];
        assert!(!is_reviewr_pane(&processes));

        let pane_action =
            vec![process("herdr-reviewr", &["herdr-reviewr", "--pane-action", "toggle"])];
        assert!(!is_reviewr_pane(&pane_action));
    }

    #[test]
    fn a_plain_shell_never_counts() {
        let processes = vec![process("zsh", &["-zsh"])];
        assert!(!is_reviewr_pane(&processes));
    }

    #[test]
    fn event_target_reads_created_and_opened_shapes() {
        let created = serde_json::json!({
            "event": "worktree_created",
            "data": {
                "workspace": {"workspace_id": "w9", "worktree": {"checkout_path": "/repo"}},
                "worktree": {"path": "/repo", "open_workspace_id": "w9"},
            },
        })
        .to_string();
        let target = parse_event_target(&created);
        assert_eq!(target.workspace.as_deref(), Some("w9"));
        assert_eq!(target.cwd.as_deref(), Some("/repo"));
    }
}
