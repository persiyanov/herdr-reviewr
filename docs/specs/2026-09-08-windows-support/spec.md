# Native Windows support

Status: Approved
Date: 2026-09-08

External reference: [persiyanov/herdr-reviewr#19](https://github.com/persiyanov/herdr-reviewr/pull/19) (stale, `DIRTY`, superseded by this spec)

## Problem

The binary already builds and runs on native Windows (`cargo check --all-targets` succeeds on `x86_64-pc-windows-msvc`). Everything around it is Unix-only:

- `herdr-plugin.toml` declares only `macos`/`linux` platforms and one Bash build command.
- The pane command expands `$HERDR_PLUGIN_ROOT` through `sh -c`; both actions and both auto-open events invoke `bash herdr/pane.sh`.
- `herdr/install.sh` is a Unix release installer with stable-link behavior.
- `src/proc.rs` prepends Unix bin directories and splits `PATH` on `:`.
- `src/export.rs`'s `CLIPBOARD_TOOLS` is Unix-only.
- `src/browser.rs` only tries `open` then `xdg-open`.
- `.github/workflows/ci.yml` runs Ubuntu only; `release.yml` builds four Unix targets and hardcodes `.tar.gz` in provenance attestation.

PR #19 attempted this by porting `pane.sh` to a second, parallel `pane.ps1`. That copy is already stale relative to current `pane.sh` behavior and has concrete defects: broken string interpolation in its installer (`"$Name: ..."` needs `${Name}:`), pane identification by cosmetic label instead of live foreground-process identity, no support for spaces in the plugin root, and it predates current auto-open, close-race, and tab-rename semantics. A second full implementation would need every future pane-action change made and tested twice.

## Proposal

Move the pane-action state machine that currently lives in `herdr/pane.sh` into the Rust binary, as an internal command dispatched in `src/main.rs` before the TUI starts. Both platforms' manifest actions and events invoke the same installed binary; only genuinely platform-specific glue (process launch, clipboard, browser, `PATH`/`PATHEXT` resolution) stays separate.

### Pane-action core

- New focused Rust module (not `src/herdr.rs`, which owns in-TUI Herdr interactions) implementing the same state machine as `herdr/pane.sh`: config validation gate, `auto_open`/overlay/zoomed gates, `already_open` short-circuit, one pane-list snapshot per run, concurrent process probing, foreground-executable identity matching (`argv0`/`argv[0]`, not title or label), close-all-matching with race tolerance, manual-open cwd preference (focused pane's live `foreground_cwd` when it's a git repo, else context cwd), auto-open cwd pinned to the event checkout, focus semantics (manual requests focus, event does not), placement handling (split/zoomed attach to pane, tab targets workspace and renames best-effort to `reviewr`, overlay has no placement target), and `plugin pane close` never used — plain `pane close` only.
- Windows executable identity matching handles both `\` and `/` separators, an optional `\\?\` extended-length-path prefix, and an optional `.exe` suffix. Not label-based, matching current policy on Unix.
- Same action IDs (`toggle`, `open`, `close`) and same event names (`worktree.created`, `worktree.opened`) on both platforms.

### Platform glue

| Area | Unix (unchanged) | Windows |
| --- | --- | --- |
| `src/proc.rs` | `COMMON_BINS` Unix paths, `:`-joined `PATH` | No Unix bin prepends; native `;`-joined `PATH`; bare-executable resolution walks `PATHEXT`; explicit paths/extensions and inherited `PATH` preserved |
| `src/export.rs` | existing `CLIPBOARD_TOOLS` | `clip`/`clip.exe`, UTF-8 stdin; same consume-on-success semantics; platform-appropriate no-tool error |
| `src/browser.rs` | `open`, `xdg-open` | `rundll32 url.dll,FileProtocolHandler <url>` directly (not `cmd /c start`, which re-parses `&`); same `openable_url` gate, same stdio/wait behavior |
| Pane launch | `sh -c` expansion of `$HERDR_PLUGIN_ROOT` | quoting-safe launcher; must not break on spaces in the plugin root (PR #19's known limitation) |

Before touching `on_path` or the `command`/`user_command` call chain in `src/proc.rs`, enumerate callsites with the language server (`src/export.rs`, `src/browser.rs`, and the git/forge/editor/Herdr callsites of `command`/`user_command`) so PATH-construction changes don't silently break a caller.

### Manifest, installer, release

- Add `windows` to the manifest's platform list. Actions and events use one relative `bin/herdr-reviewr` command declaration unchanged on both platforms (confirmed — see Open questions). The pane entrypoint needs a platform-specific command: Unix keeps `sh -c 'exec "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr"'`; Windows gets an equivalent absolute-path, spaces-safe launcher, because the pane (unlike actions) must run with the reviewed repo as process cwd.
- `herdr/install.ps1`: resolve the checkout root from `$PSScriptRoot`; read the manifest version and download the matching `vX.Y.Z` Windows asset; download `herdr-reviewr-x86_64-pc-windows-msvc.zip` plus its `.sha256` sidecar; retry transient download failures; verify the hash before extracting; install `bin\herdr-reviewr.exe`; correct `${Name}:` interpolation throughout; clean temp files in `finally`. No stable-link behavior unless a Windows equivalent is separately designed.
- `.github/workflows/release.yml`: add an `x86_64-pc-windows-msvc` matrix row producing the ZIP above; fix the provenance attestation step (currently hardcodes `.tar.gz`) to use the correct extension per target.
- `.github/workflows/ci.yml`: add a Windows job running the Rust test suite and a PowerShell parser check on every committed `.ps1`.
- `.gitattributes`: enforce LF on `*.sh` so Windows checkouts don't corrupt the retained shell scripts.

## Invariants

False if the named test is red.

| code | Always true | Enforcement |
| --- | --- | --- |
| WIN-CONFIG | Invalid config blocks any action on both platforms before workspace/pane inspection; manual actions and event failures are loud. | `pane_action_invalid_config_blocks_before_inspection` |
| WIN-AUTO-SILENT | A runtime auto-open refusal (`auto_open=false`, overlay, zoomed, existing pane, `already_open=true`) exits 0 before any Herdr call, on both platforms. | `pane_action_auto_open_refusals_silent` |
| WIN-IDENTITY | The review pane is identified by foreground executable identity (`argv0`/`argv[0]`, tolerant of `\\?\` extended-length prefix, `\`/`/` separators, and `.exe` suffix), never by pane label or process title. On Windows, also recognizes the `pane-windows` launcher's own `powershell.exe` wrapper signature, since Windows has no process-image-replacement primitive and the real binary never becomes the reported foreground process (found live, see below). | `pane_action_identifies_by_executable_not_label`, `the_windows_pane_launcher_counts_even_though_only_powershell_is_reported` |
| WIN-SNAPSHOT | Exactly one pane-list read per action invocation; an unreadable pane list or process-info response refuses rather than proceeding as "no review pane." | `pane_action_refuses_on_unreadable_pane_list` |
| WIN-CLOSE | Close targets every matching pane; `pane_not_found` on an already-closing pane is treated as success; other failures are reported after attempting the rest. | `pane_action_close_all_tolerates_race` |
| WIN-CWD | Manual open prefers the focused pane's live `foreground_cwd` when it is a git repo, else context cwd; auto-open always uses the event checkout. | `pane_action_cwd_selection_manual_vs_event` |
| WIN-FOCUS | Manual open requests focus; event-triggered open does not. | `pane_action_focus_manual_vs_event` |
| WIN-NOWRITE | No pane-action path performs a git write; the binary's only git writes remain the existing `refs/worktree/reviewr/*` refs. | existing write-boundary tests, run on Windows |
| WIN-CLIP | Clipboard export is consume-on-success: any spawn/write/wait/nonzero failure on `clip.exe` leaves comments untouched. | `export_clipboard_consume_on_success_windows` |
| WIN-PATH | Command resolution on Windows never prepends Unix bin directories, honors `PATHEXT` for bare executables, and preserves explicit paths/extensions and the inherited `PATH`. | `proc_resolve_windows_pathext_and_inherited_path` |
| WIN-ONECORE | Unix and Windows pane actions run the same core state machine; no parallel PowerShell reimplementation of action/event logic ships. | manifest + integration tests target one Rust entrypoint on both platforms |

Release acceptance: build the Windows ZIP, install it through a disposable Herdr plugin session (not the user's registered `persiyanov.reviewr`), and verify open/toggle-close/toggle-open and both auto-open events, a plugin root containing spaces, `clip.exe` export of a non-ASCII payload, and an HTTP URL containing `&` reaching the default browser unmangled. Final pane reopen into the user's focused workspace is the user's own keystroke, per the repository's QA-install safety rule — never scripted.

## Alternatives

- **Direct `pane.sh` → `pane.ps1` port**, keeping two full implementations with a parallel Windows integration suite. Rejected: PR #19 is the demonstrated failure mode — its PowerShell copy drifted out of sync with `pane.sh` and shipped with bugs a shared core would not have. Every future action/event/identity/cwd/error-semantics change would need to land and be tested twice, indefinitely.
- **WSL-backed Bash reuse.** Rejected: a bare `bash` on Windows may resolve to WSL, which cannot operate on the native Windows worktree the user is reviewing.

## Out of scope

ARM64 Windows target. A Windows equivalent of the Unix installer's stable-link behavior (only added if a concrete need surfaces). Session/workspace restore. Any change to Unix pane-action behavior beyond relocating it out of `pane.sh` into the shared core. Bumping `min_herdr_version` unless the experiment in the open questions below requires it.

## Open questions — resolved 2026-09-08

Resolved with a disposable `herdr plugin link` probe plugin (id `scratch.winprobe`, unlinked after the experiment; the user's `persiyanov.reviewr` registration was never touched). The probe action wrote its own `current_dir()`, `current_exe()`, argv, and every `HERDR_*` env var to a log file, invoked headlessly via `herdr plugin action invoke` (no pane opened).

1. **Can one action/event declaration invoke a relative `bin/herdr-reviewr` command unchanged on both Unix and Windows?** Yes. The probe's action ran with the *plugin root* as process cwd (`C:\...\scratch.winprobe`), not the workspace or context cwd, exactly matching the existing Unix action contract (`bash herdr/pane.sh` already resolves relative to the plugin root). A relative `bin/herdr-reviewr.exe` action/event command needs no platform-specific declaration. The target repo is available to the process only via `HERDR_PLUGIN_CONTEXT_JSON` (which carries `workspace_cwd`, `focused_pane_cwd`, etc.), not via process cwd.
2. **Does the pane command resolve its executable before or after applying the requested repository cwd — and can it use the same relative-path approach as actions?** No — the pane case is not like actions. `git.rs`'s subprocess calls never set an explicit `current_dir()`, so the whole binary depends on its own process cwd already being the repo under review. Since actions run with cwd = plugin root (finding 1), a bare relative pane command would leave the binary running with the wrong cwd. The pane launcher must keep forcing process cwd to the reviewed repo while still resolving the binary from the plugin root — i.e. it needs a Windows equivalent of `sh -c 'exec "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr"'` (absolute path, spaces-safe), not a relative command. This confirms the assumption in the current `[[panes]]` comment and carries it forward unchanged in shape, just needing a Windows-native launcher.

**New finding, not anticipated pre-experiment:** Windows reports the resolved executable to the child process as an extended-length path with a `\\?\` prefix (e.g. `\\?\C:\Users\...\bin\probe.exe`) in both argv and `current_exe()`. WIN-IDENTITY's foreground-process matching must strip this prefix in addition to normalizing `\`/`/` separators and the `.exe` suffix, or Windows pane detection silently fails to match the review pane's own process.

**Second new finding, from ticket 5's implementation work (2026-09-08):** Herdr rejects a manifest with two `[[panes]]` entries sharing the same `id`, even when each is scoped to a disjoint `platforms` list (`duplicate_plugin_pane_id` from `herdr plugin link`). The Unix and Windows pane launchers therefore need distinct ids (`pane-unix` / `pane-windows`), with the caller (`src/pane_action.rs::open_pane`) selecting the right one via `cfg!(windows)`. The chosen Windows launcher — `["powershell", "-NoProfile", "-NonInteractive", "-Command", "& \"$env:HERDR_PLUGIN_ROOT\\bin\\herdr-reviewr.exe\""]` — was verified live against a plugin linked from a path **containing a space** (PR #19's exact known failure), opened into a disposable, unfocused workspace (`herdr workspace create --no-focus` / `herdr plugin pane open --no-focus`, never the user's focused one): the launched process's own `current_dir()` was the reviewed repo, not the plugin root, and the spaced `HERDR_PLUGIN_ROOT` resolved unmangled. Also confirmed: a bare relative `bin/herdr-reviewr` (no `.exe`) resolves via `PATHEXT` when Herdr spawns an *action* directly, settling finding 1 above beyond the earlier `.exe`-qualified test.

**Third new finding, from ticket 9's live QA-install verification with the user (2026-09-08) — a real bug the fake-herdr test suite could not have caught:** the first live end-to-end test (real installed plugin, real user keypress, real pane) surfaced that `toggle` never closed an already-open Windows pane — it stacked a second one instead. Root cause: Windows has no equivalent of Unix's `exec`, which the Unix pane launcher (`sh -c 'exec "..."'`) uses to replace the shell process with `herdr-reviewr` itself, same pid — that's *why* WIN-IDENTITY's plain executable-identity match works there. A Windows child process is always genuinely separate, so `herdr pane process-info` on a real Windows pane reports **only** `powershell.EXE` as the foreground process; `herdr-reviewr.exe` never appears at all. Captured live:
```json
{"argv0":"C:\\WINDOWS\\...\\powershell.EXE","argv":["C:\\WINDOWS\\...\\powershell.EXE","-NoProfile","-NonInteractive","-Command","& \"$env:HERDR_PLUGIN_ROOT\\bin\\herdr-reviewr.exe\""], ...}
```
The real command survives, literally unexpanded, inside the wrapper's own `-Command` argv element — Herdr passes argv with no shell expansion, and a process's reported command line is fixed at creation time regardless of what the interpreter does with it internally. Fixed by additionally recognizing that exact launcher signature (`$env:HERDR_PLUGIN_ROOT` and `herdr-reviewr.exe` both present in one argv element) as the review UI, Windows-only — narrow enough that it cannot false-positive on an unrelated process. Regression test: `the_windows_pane_launcher_counts_even_though_only_powershell_is_reported`, built directly from the captured JSON above. Re-verified live after the fix (re-swapped via QA-install): `toggle` now correctly opens on the first call and closes on the second, in two different workspaces (`wA` and `wB`).
