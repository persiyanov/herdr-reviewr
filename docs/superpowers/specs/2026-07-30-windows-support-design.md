# Windows support for herdr-reviewr

Date: 2026-07-30
Status: approved

## Problem

`herdr plugin install dcieslak19973/herdr-reviewr` fails on native Windows. Two distinct
failures were diagnosed:

1. **The reported error** — `Error { kind: NotFound, message: "program not found" }` — is
   herdr failing to spawn `git` for the clone when git is not on `PATH`. Reproduced exactly
   on Windows by stripping git from `PATH`. This is herdr's error to report better (issue to
   be filed upstream); the user-side fix is installing Git for Windows. The plugin can only
   document it.
2. **The plugin's own gap** — even when install succeeds on Windows, herdr skips the
   `[[build]]` step (`build (skipped on windows)` with the current manifest), so no binary is
   downloaded, and every pane/action/event command routes through `bash`/`sh`, which native
   Windows lacks. The plugin declares `platforms = ["macos", "linux"]` and ships no Windows
   release artifact.

## Goal

`herdr plugin install dcieslak19973/herdr-reviewr` on native Windows (with git installed)
downloads a Windows binary, and the sidebar pane, all four actions, and the
`worktree.created` event work identically to macOS/Linux.

## Verified facts this design rests on

- herdr ships and runs on Windows (`0.7.1-preview` verified locally; `herdr plugin install`
  works end-to-end there).
- herdr's manifest accepts `"windows"` in `platforms`, and **item-level `platforms` override
  the top-level list** — per-platform `[[build]]`/pane/action/event variants are supported.
  Commands are direct argv (no shell); Windows resolves `PATHEXT` shims (herdr.dev docs).
- The crate compiles cleanly on Windows (`cargo build`, zero warnings). Dependencies
  (ratatui, syntect, similar, serde_json, toml) are all portable. Only two `cfg(unix)` sites
  exist (`src/cli.rs:342`, `src/config.rs:613`), both with fallbacks.

## Design

### 1. Release pipeline

Add `x86_64-pc-windows-msvc` (runs-on `windows-latest`, `.zip` archive + `.sha256` sidecar)
to the matrix in `.github/workflows/release.yml`. No Windows-ARM target until requested.

### 2. Windows installer: `herdr/install.ps1`

PowerShell port of `herdr/install.sh`, run by a Windows-only `[[build]]` variant:

- Resolve plugin root from the script location; read `version` from `herdr-plugin.toml`.
- Download `herdr-reviewr-x86_64-pc-windows-msvc.zip` + checksum sidecar from the matching
  GitHub release, with retries (release assets are eventually-consistent, incl. 404s).
- Verify via `Get-FileHash -Algorithm SHA256`; extract to `bin\herdr-reviewr.exe`.
- No PATH mutation (no `~/.local/bin` convention on Windows): print the absolute binary path
  and the same next-steps epilogue as install.sh. Failures in the epilogue never fail the
  install; download/checksum failures do.
- `$ErrorActionPreference = 'Stop'` for `set -euo pipefail` parity.

### 3. Sidebar orchestration moves into the binary

New subcommand `herdr-reviewr sidebar <toggle|open|close|auto-open>`, a 1:1 port of
`sidebar.sh` semantics (spec contracts A3, A5, A7, P5, P6 in `specs/herdr-host.md`):

- Config via the existing `--resolve-plugin-config` code path as a direct function call
  (no subprocess, no jq).
- Context from `HERDR_*` env vars; `HERDR_PLUGIN_CONTEXT_JSON` / `HERDR_PLUGIN_EVENT_JSON`
  parsed with `serde_json`.
- Pane orchestration shells out to `$HERDR_BIN_PATH` (fallback `herdr`) for `pane list`,
  `pane close` (plain, not `plugin pane close` — registry does not survive restart, A7),
  and `plugin pane open`.
- Identical refusal contract: successes on stdout, refusals as one stderr line + exit 1,
  `auto-open` exits 0 silently when gated (auto_open=false, overlay/zoomed placement, or
  missing context).
- `herdr/sidebar.sh` is **deleted**; the `jq` runtime dependency disappears on all platforms.
- Decision logic (mode × existing panes × placement → planned herdr invocations) is
  extracted as pure functions for unit testing, matching the repo's existing test style.

### 4. Manifest: per-platform command variants

- Top-level `platforms = ["macos", "linux", "windows"]`.
- Each of build / pane / four actions / one event gets a Unix item (`platforms =
  ["macos", "linux"]`, `bash`/`sh` one-liners now invoking `… sidebar <mode>`) and a Windows
  twin (`platforms = ["windows"]`) using a PowerShell one-liner that strips the `\\?\`
  verbatim prefix herdr reports in `$HERDR_PLUGIN_ROOT` on Windows, then invokes the exe by
  absolute path. PowerShell over `cmd /c` because it quotes paths containing spaces
  correctly (`C:\Users\Dan Cieslak` is the local proof case).
- herdr rejects duplicate pane/action ids even across disjoint platform filters (verified
  in the herdr-slackr reference implementation), so Windows twins carry `-windows` id
  suffixes (`sidebar-windows`, `toggle-windows`, …) and the runtime selects its own
  platform's entrypoint via `cfg!(windows)`.
- `min_herdr_version` bumps to 0.7.5 — the earliest herdr verified to honor item-level
  `platforms` filters; an older herdr refuses with a version message instead of running
  both `[[build]]` twins on unix.
- `plugin unlink` is broken on Windows (raw NotFound error); link-probe cleanup uses
  `plugin uninstall`.

### 5. Windows clipboard

- Add `("clip", &[])` (System32, reads stdin) to `CLIPBOARD_TOOLS` in `src/export.rs`.
- Fix the unix-only executable probe in `src/proc.rs` under `cfg(windows)`
  (`.exe`/`PATHEXT` awareness).
- Verify both existing `cfg(unix)` fallback sites behave sensibly on Windows.
- Removes the "OSC 52 and Windows are roadmap" caveat for Windows; OSC 52 stays roadmap.

### 6. Documentation

- README: platform section says Windows is supported and names prerequisites (git on
  `PATH`); new troubleshooting entry mapping
  `Error { kind: NotFound, message: "program not found" }` → git missing from `PATH`;
  note that a herdr predating Windows plugin support silently skips the build step
  (binary-less install) and how to tell.
- `docs/herdr-api-notes.md`: record item-level `platforms` overrides and the Windows
  build-skip behavior observed on 0.7.1-preview.
- Upstream (outside this repo): file a herdr issue asking for a readable error when the
  `git` spawn fails during `plugin install`.

### 7. Testing

- Unit tests for the sidebar decision functions and context/event JSON parsing.
- Add a Windows runner to the CI test workflow (if present) so `cargo test` runs on
  `windows-latest`.
- End-to-end: `herdr plugin link` against the local Windows herdr instance — pane opens,
  toggle/open/close actions work, clipboard export works; post-release, a real
  `plugin install` verifies the download path (colleague's scenario).

## Risks

- **Older/stable herdr**: if the installing herdr predates `"windows"` platform support or
  item-level overrides, behavior is unknown (worst case: manifest rejected, or build
  silently skipped). Mitigated by the `min_herdr_version` bump and the README note; not
  fixable from this repo.
- **TUI on Windows terminals**: ratatui's crossterm backend is well-supported in Windows
  Terminal but the pane gets an explicit smoke test in step 7.
- **PowerShell startup latency** on actions (~100–300 ms) is accepted; actions are
  user-initiated and infrequent.

## Out of scope

- Windows-ARM release target.
- OSC 52 clipboard.
- Any change to herdr itself (the git-spawn error message is an upstream issue).
