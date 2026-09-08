# Native Windows support: Plan

Delivers the sibling [`spec.md`](spec.md).

## Executor tiering

Each ticket below has a **Suggested model** field. This is an execution-cost hint, not a repo convention: tickets with a narrow, mechanical, fully-specified deliverable (config/CI edits, doc updates, a script mirroring an existing contract line-for-line) are tagged **Haiku 4.5**. Tickets that require porting branching logic exactly, reasoning about platform quirks, or judging live behavior against invariants are tagged **Sonnet 5**. None are tagged Opus — nothing here needs it if the ticket's acceptance criteria and gotchas are followed as written.

To act on a tag when dispatching a subagent for a ticket, pass the matching `model` value (`"haiku"` or `"sonnet"`) to the `Agent` tool. Every ticket lists its full acceptance criteria and known gotchas up front specifically so a Haiku-tier executor doesn't need to rediscover them. If a Haiku-tier ticket's implementation turns out messier than expected mid-flight, re-dispatch that ticket at Sonnet rather than pushing a struggling Haiku run further — don't downgrade a Sonnet-tagged ticket to save quota.

## Problem

The binary already builds on Windows; nothing around it does. See [`spec.md`](spec.md) Problem section for the full gap list and PR #19 postmortem.

## Goal

Ship the approved shared-Rust-core design: a Windows-capable plugin (install, open/toggle/close, both auto-open events), Windows-native command resolution/clipboard/browser support, a Windows release artifact, and CI/test coverage — with every invariant in `spec.md` holding, verified by a native Herdr smoke test.

## Ticket Map

1. Windows-aware command resolution (`src/proc.rs`). Blocked by: none.
2. Windows clipboard export (`src/export.rs`). Blocked by: 1.
3. Windows browser opener (`src/browser.rs`). Blocked by: 1.
4. Shared Rust pane-action core. Blocked by: 1.
5. Manifest + Windows pane launcher. Blocked by: 4.
6. Windows installer (`herdr/install.ps1`). Blocked by: 5.
7. Release CI: Windows target + attestation fix. Blocked by: 6.
8. CI: Windows job + `.gitattributes`. Blocked by: none.
9. Native Herdr behavioral verification. Blocked by: 5, 6.
10. Documentation. Blocked by: 9.

Tickets 1 and 8 can start in parallel. 2 and 3 can run in parallel once 1 lands.

---

## Ticket 1: Windows-aware command resolution (`src/proc.rs`)

**Suggested model:** Sonnet 5 — subtle correctness surface (PATHEXT ordering, explicit-extension vs bare-name, explicit-path bypass, inherited-`PATH` preservation) and a wide, LSP-verified callsite fan-out.

**What to build:** `src/proc.rs`'s `command`/`user_command`/`on_path`/`resolve_on` work correctly on Windows: no Unix bin-dir prepending, native `;`-joined `PATH`, `PATHEXT`-aware bare-executable resolution, explicit paths/extensions passed through untouched.

**Blocked by:** none

**Status:** done

### Acceptance criteria

- [x] On Windows, `prepended_path`/`appended_path` never add `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, or `/bin`, and join with `;` not `:`.
- [x] On Windows, resolving a bare name (no extension, no path separator) walks `PATHEXT` (default `.COM;.EXE;.BAT;.CMD` when the env var is unset) in order and returns the first match.
- [x] An explicit path (absolute or containing a separator) or a name with an explicit extension is used as-is, without `PATHEXT` expansion.
- [x] The inherited process `PATH` is preserved unchanged aside from the platform-appropriate host-bin precedence.
- [x] Existing Unix `resolve_on`/`command`/`user_command`/`on_path` tests stay green (moved behind `#[cfg(unix)]`, unmodified in content).
- [x] `just ci` passes (Unix path; Windows CI job lands in ticket 8).

### Completion evidence

- `cargo test --lib proc::` (Windows host) → 7 new Windows tests pass, including a bug caught and fixed mid-implementation: a bare name with its own extension (e.g. `probe.exe`) was incorrectly treated as an explicit path instead of being searched across `PATH` without `PATHEXT` expansion — fixed and covered by `proc_windows_explicit_extension_bypasses_pathext`.
- `cargo test --all-features` (Windows host) → 320+264+54+24+130 passed, 0 failed.
- `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D warnings` (Windows host) → clean.

### Implementation plan

1. Before editing, use LSP references (or `Grep`) to enumerate every callsite of `on_path`, `command`, and `user_command`. The handoff observed `src/export.rs` and `src/browser.rs` for `on_path`, and many git/forge/editor/Herdr callsites for `command`/`user_command`. Confirm none assume Unix-only path shape.
2. Add a `cfg(windows)` branch to `prepended_path`/`appended_path` that skips the Unix bin-dir list entirely and joins with `;`.
3. Add `PATHEXT`-aware resolution to `resolve_on` for Windows: if the candidate has no extension and no path separator, try each `PATHEXT` entry appended to the name against each `PATH` directory; if it has an extension or is a path/absolute, resolve as-is.
4. Keep the existing Unix branch untouched.

### Tests

- `proc_windows_no_unix_bin_dirs_prepended`
- `proc_windows_pathext_resolves_bare_name` (missing `PATHEXT` env falls back to the documented default list)
- `proc_windows_explicit_extension_bypasses_pathext`
- `proc_windows_explicit_path_used_as_is`
- `proc_windows_inherited_path_preserved`
- Existing Unix `proc` tests, unmodified.

### Verification

- `cargo test proc_windows` (Windows host) → all pass.
- `cargo test proc_` (either host) → existing Unix tests still pass.
- `just ci` → passes on the Unix path.

---

## Ticket 2: Windows clipboard export (`src/export.rs`)

**Suggested model:** Haiku 4.5 — narrow, fully specified: one new tool (`clip`/`clip.exe`), same consume-on-success contract already implemented for Unix.

**What to build:** `CLIPBOARD_TOOLS` (or its Windows equivalent) uses `clip`/`clip.exe`, writing UTF-8 comment bytes to its stdin, with the existing consume-on-success semantics unchanged, and a platform-appropriate no-tool error message.

**Blocked by:** Ticket 1 (uses its command resolution).

**Status:** done

### Acceptance criteria

- [x] On Windows, export tries `clip`/`clip.exe`, writing UTF-8 to stdin.
- [x] Any spawn error, write error, wait error, or nonzero exit leaves the in-memory comment store untouched (matches the existing Unix consume-on-success contract — do not weaken it).
- [x] The "no clipboard tool found" error text is platform-appropriate (does not tell a Windows user to install Linux tools).
- [x] Tests exercise clipboard *selection* logic (which tool gets tried, in what order, success/failure handling) without depending on a real clipboard tool being present on the CI host.
- [x] Existing Unix export tests stay green unmodified.

### Completion evidence

- Built by a Haiku-tier agent; reviewed and accepted as-is. `cargo test --lib export::`, `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings` all clean on Windows host, and `cargo test --all-features` across the whole tree stays green (324+264+54+24+130 passed).

### Implementation plan

1. Add a Windows entry to the clipboard tool table pointing at `clip`.
2. Route the spawn/write/wait through the same consume-on-success code path already used for Unix tools — don't add a second code path.
3. Add a platform-conditional error string.

### Tests

- `export_windows_clip_selected_as_tool`
- `export_windows_failed_spawn_leaves_comments_intact`
- `export_windows_nonzero_exit_leaves_comments_intact`
- `export_windows_no_tool_error_is_windows_specific`
- Existing Unix `export` tests, unmodified.

### Verification

- `cargo test export_windows` (Windows host) → all pass, no live clipboard access required.
- Manual, once ticket 9's native pass runs: export a comment containing non-ASCII text, confirm via `Get-Clipboard` (or paste) that the clipboard holds exactly that UTF-8 text.

---

## Ticket 3: Windows browser opener (`src/browser.rs`)

**Suggested model:** Haiku 4.5 — one new opener command, existing security gate and process-handling pattern reused unchanged.

**What to build:** On Windows, open a URL via `rundll32 url.dll,FileProtocolHandler <url>` directly (not `cmd /c start`, which re-parses `&`). Keep the existing `openable_url` gate and synchronous stdio-null/wait behavior.

**Blocked by:** Ticket 1 (uses its command resolution).

**Status:** done

### Acceptance criteria

- [x] On Windows, the opener list is `rundll32 url.dll,FileProtocolHandler` only (no `cmd /c start`).
- [x] `openable_url`'s existing security gate runs unchanged and still rejects the same class of unsafe input on Windows.
- [x] Stdio is null and the call waits/reaps exactly like the existing Unix openers.
- [x] A URL containing `&` is passed through as a single argument, not split or reinterpreted.
- [x] Existing Unix `browser` tests stay green unmodified.

### Completion evidence

- Built by a Haiku-tier agent; reviewed and accepted. `cargo test --lib browser::`, `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings` all clean, and the full-tree `cargo test --all-features` stays green.

### Implementation plan

1. Add the Windows opener entry using the existing opener-list pattern (`open`, `xdg-open` today).
2. Confirm `rundll32` is invoked with the URL as one argument (not shell-concatenated), so no `cmd`-style `&` reinterpretation is possible.
3. Reuse `openable_url` and the existing spawn/stdio/wait code as-is.

### Tests

- `browser_windows_opener_is_rundll32`
- `browser_windows_ampersand_url_passed_as_single_arg`
- `openable_url` gate tests run unchanged on Windows.
- Existing Unix `browser` tests, unmodified.

### Verification

- `cargo test browser_windows` (Windows host) → all pass.
- Manual, once ticket 9's native pass runs: trigger a PR/browser-open action with a URL containing `&`, confirm the full URL (unsplit) reaches the default browser. Use a process-level check (log the argv actually exec'd) rather than an interactive browser launch if possible.

---

## Ticket 4: Shared Rust pane-action core

**Suggested model:** Sonnet 5 — the highest-stakes ticket. It ports all of `herdr/pane.sh`'s branching (289 lines) into Rust while preserving every invariant in `spec.md`'s table, including `No writes`, `Comments survive`, and `Continuity` from `AGENTS.md`. Getting a gate wrong here breaks the app's core safety contract, not just a feature.

**What to build:** A new focused Rust module (not `src/herdr.rs`) plus an internal `src/main.rs`-dispatched command implementing the same pane-action state machine as `herdr/pane.sh`, callable identically on Unix and Windows: config validation gate; `auto_open`/overlay/zoomed gates; `already_open` short-circuit; one pane-list snapshot per run; concurrent process probing; foreground-executable identity matching tolerant of `\`/`/` separators, an optional `\\?\` extended-length prefix, and an optional `.exe` suffix (never label/title-based); close-all-matching with `pane_not_found` race tolerance; manual-open cwd preference (focused pane's live `foreground_cwd` when it's a git repo, else context cwd) vs. auto-open's pinned event checkout; focus semantics (manual requests focus, event does not); placement handling (split/zoomed attach to pane, tab targets workspace and best-effort renames to `reviewr`, overlay has no placement target); `plugin pane close` never used, plain `pane close` only.

**Blocked by:** Ticket 1 (process listing/spawning goes through the updated `proc.rs`).

**Status:** done

### Acceptance criteria

- [x] Every invariant WIN-CONFIG through WIN-ONECORE in `spec.md` has a passing named test, run on both platforms (or as platform-neutral pure logic where the state machine itself is host-independent).
- [x] The core is exercised via the same integration-test pattern as `tests/pane_actions.rs` uses today for `pane.sh`, extended to invoke the new Rust command instead.
- [x] `herdr/pane.sh` is *not* kept alongside the new core as a second implementation — this ticket is a migration, not an addition. `pane.sh` itself is untouched and still the manifest's registered entrypoint (cutover is ticket 5); the test suite's assertion logic exists exactly once, now pointed at the new core.
- [x] Windows executable-identity matching correctly matches the reviewr binary's own live process, verified with the extended-length-prefix and separator variants observed in the ticket-1-adjacent `winprobe` experiment recorded in `spec.md`.
- [x] No new git write path is introduced; the binary's only git writes remain the existing `refs/worktree/reviewr/*` refs (`is_git_repo` reuses the existing read-only `git::worktree_of`).

### Completion evidence

- Ported `herdr/pane.sh`'s full state machine into `src/pane_action.rs` (new module) plus an internal `--pane-action <mode>` dispatch in `src/main.rs`, reusing `crate::config` for validated in-process config resolution (no more shelling out to itself + `jq`) and `crate::git::worktree_of` for the git-repo check.
- `tests/pane_actions.rs` retargeted at the new core (`Command::new(reviewr_bin()).arg("--pane-action").arg(mode)` instead of `bash herdr/pane.sh`); this file stays `#![cfg(unix)]` since its `fake_herdr` fixture is a shebang shell script Windows can't execute directly (a Windows-native fake-herdr fixture is future work for ticket 5's Windows-specific launcher testing, not this ticket).
- This machine is native Windows only, so the Unix-gated suite couldn't run locally — verified for real instead by running the full suite in a `rust:1.97-bookworm` Linux container (Docker Desktop) mounting the checkout: **all 29 tests pass** against the new core, including every close-race, identity, cwd-priority, placement, and stable-link-repair case. This is genuine execution, not just manual line-by-line comparison against `pane.sh` (which was also done first, and caught two minor message-fidelity gaps — a dropped `in <workspace>` suffix on the "no pane to attach to" refusal, and an empty-but-set `HERDR_PLUGIN_EVENT_JSON` incorrectly overriding workspace/cwd where the shell's `-n` check would not — both fixed before the container run).
- `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D warnings` clean on the Windows host; `cargo test --lib pane_action::` (5 unit tests: identity matching across Unix/Windows path shapes, flag exclusion, event-target parsing) also clean.

### Implementation plan

1. Read `herdr/pane.sh` top to bottom and `tests/pane_actions.rs` to catalog every existing branch and its covering test before writing any Rust.
2. Design the new module's function boundaries around the same decision points `pane.sh` already has (config gate → auto-open/placement gates → `already_open` gate → pane-list snapshot → identity match → action dispatch), so the port is traceable line-by-line against the shell script during review.
3. Implement Windows-tolerant executable-identity matching as an explicit, separately tested function (strip `\\?\`, normalize separators, compare case-insensitively with/without `.exe`), not inlined into the matching loop.
4. Wire the new command into `src/main.rs`'s internal dispatch, gated so it only activates for the internal invocation path (not the TUI).
5. Ship this ticket with `pane.sh` still the manifest's registered entrypoint if ticket 5 hasn't landed yet — the new core exists and is tested, but isn't live until the manifest cutover.

### Tests

Port every existing `tests/pane_actions.rs` case to run against the new core's entrypoint, plus:

- `pane_core_identity_matches_backslash_and_forwardslash`
- `pane_core_identity_matches_extended_length_prefix`
- `pane_core_identity_matches_with_and_without_exe_suffix`
- `pane_core_config_gate_blocks_before_inspection`
- `pane_core_auto_open_refusals_exit_silently`
- `pane_core_close_all_tolerates_pane_not_found_race`
- `pane_core_manual_cwd_prefers_focused_pane_git_repo`
- `pane_core_event_cwd_pinned_to_checkout`
- `pane_core_never_calls_plugin_pane_close`

### Verification

- `cargo test --test pane_actions` (full suite against the new core) → all pass.
- `just ci` → passes.
- Manual diff review: every `pane.sh` branch has a corresponding Rust branch and test; nothing silently dropped.

---

## Ticket 5: Manifest + Windows pane launcher

**Suggested model:** Sonnet 5 — the exact bug class that sank PR #19 (unquoted paths breaking on spaces) lives here; needs careful reasoning about quoting, not just following a template.

**What to build:** `herdr-plugin.toml` gains `windows` in `platforms`. Actions and events keep one shared relative `bin/herdr-reviewr` command declaration across both platforms (confirmed safe in `spec.md`'s Open Questions — Herdr runs action/event commands with the plugin root as cwd on both platforms). The pane command gets a Windows-specific, quoting-safe, spaces-in-path-safe absolute launcher, because (per `spec.md`) the pane process's own cwd must be the reviewed repo, unlike actions.

**Blocked by:** Ticket 4 (the core this manifest now targets must exist and be dispatchable).

**Status:** done

### Acceptance criteria

- [x] `herdr-plugin.toml` parses with `windows` added to `platforms` alongside `macos`/`linux`.
- [x] Actions (`toggle`/`open`/`close`) and both events use the same relative command declaration on Windows as Unix (per the resolved open question — no platform branching needed there).
- [x] The Windows pane command resolves `bin/herdr-reviewr.exe` from `%HERDR_PLUGIN_ROOT%` while the process's actual cwd is set to the reviewed repository, equivalent in effect to the Unix `sh -c 'exec "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr"'`.
- [x] A plugin root containing spaces launches correctly on Windows (this is PR #19's known failure — write the regression test for it explicitly).
- [x] Manifest tests (parsing + `toml` crate parse in `tests/pane_actions.rs`-style coverage) verify the platform list, both build commands if platform-specific, action IDs, pane entrypoints, and both auto-open events on all three platforms.
- [x] `herdr/pane.sh` is no longer the registered entrypoint for any action/event/pane on any platform after this ticket lands — full cutover, no dual registration left behind.

### Completion evidence

- **Manifest schema discovery, empirical (not assumed):** Herdr rejects duplicate pane ids even when two `[[panes]]` entries carry disjoint `platforms` scopes (`duplicate_plugin_pane_id`, confirmed via `herdr plugin link` against a disposable scratch plugin). This means the Unix and Windows pane launchers need distinct ids — `pane-unix` / `pane-windows` — with `src/pane_action.rs::open_pane` picking the right one via `cfg!(windows)`. Actions/events, by contrast, needed no per-platform split at all (confirmed in ticket 4's open-questions work).
- **Windows launcher shape, empirically validated live**, not just written from the spec: `["powershell", "-NoProfile", "-NonInteractive", "-Command", "& \"$env:HERDR_PLUGIN_ROOT\\bin\\herdr-reviewr.exe\""]`. Verified by linking a disposable plugin **from a path containing a space** (`...\win launch probe\`, PR #19's exact known failure), creating a fresh unfocused workspace (`herdr workspace create --no-focus`), and opening the pane into it (`herdr plugin pane open --workspace <id> --no-focus`) — never the user's focused workspace. The launched probe's own log confirmed: `current_dir` was the reviewed repo (not the plugin root, matching the Unix contract), the binary resolved correctly despite the space, and `HERDR_PLUGIN_ROOT` arrived unmangled. Also confirmed a bare relative `bin/herdr-reviewr` (no `.exe`) resolves via `PATHEXT` when Herdr spawns an *action* directly — settling the last unverified assumption from ticket 4's Open Questions. Every scratch plugin/workspace was unlinked/closed after use.
- `herdr/pane.sh` deleted. Every other reference to it updated: `AGENTS.md` (two spots), `herdr/install.sh` (one comment), `docs/herdr-api-notes.md` (manifest example block plus five inline mentions, plus a note that `pane_action.rs` needs neither `jq` nor a prepended `PATH` the way the shell script did). `README.md` and `docs/qa-install.md` had no references to begin with.
- Added `manifest_declares_windows_platform`, `manifest_actions_use_shared_relative_command_all_platforms`, `manifest_windows_pane_command_resolves_with_spaces_in_root`, and `manifest_no_remaining_pane_sh_references` to `tests/pane_actions.rs`; rewrote `manifest_auto_open_hooks_created_and_opened` for the new command shape.
- This machine is native Windows only, so (as with ticket 4) the Unix-gated suite was verified for real in the same `rust:1.97-bookworm` Linux container: **all 33 tests pass** (29 from ticket 4 plus the 4 new manifest tests).
- `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and the full `cargo test --all-features` (329+264+54+24+130 passed) stay clean on the Windows host.

### Implementation plan

1. Add `windows` to `platforms` in `herdr-plugin.toml`.
2. Point `[[actions]]`/`[[events]]` commands at the shared relative `bin/herdr-reviewr` entrypoint (drop `bash herdr/pane.sh` — this is the cutover point referenced in ticket 4).
3. Write the Windows `[[panes]]` command. Confirm empirically (reuse the `winprobe`-style disposable-plugin technique from `spec.md`, not the user's real registration) that whatever launcher syntax is chosen actually sets process cwd to the target repo and resolves the binary correctly with a space in the plugin root path before committing to it.
4. Delete `herdr/pane.sh` and update anything that still references it (`AGENTS.md`, `README.md`, `docs/qa-install.md` if it names the file).

### Tests

- `manifest_declares_windows_platform`
- `manifest_actions_use_shared_relative_command_all_platforms`
- `manifest_windows_pane_command_resolves_with_spaces_in_root`
- `manifest_no_remaining_pane_sh_references` (grep-style check, or doc-consistency test)

### Verification

- `cargo test manifest_` → all pass.
- Disposable-plugin experiment (link locally, spaces-in-path plugin root, headless `herdr plugin pane open`/`close` only in a throwaway workspace — never the user's focused one) → pane launches with the correct cwd and no quoting failure.
- `just ci` → passes.

---

## Ticket 6: Windows installer (`herdr/install.ps1`)

**Suggested model:** Haiku 4.5 — mirrors `install.sh`'s existing contract point-for-point; the one known trap (string interpolation before `:`) is called out explicitly below so a Haiku-tier executor doesn't need to rediscover it.

**What to build:** `herdr/install.ps1`, resolving the checkout root from `$PSScriptRoot`, downloading and verifying the versioned Windows release asset, and installing `bin\herdr-reviewr.exe`.

**Blocked by:** Ticket 5 (needs the manifest's build-command shape and version source settled).

**Status:** done

### Acceptance criteria

- [x] Resolves the checkout root from `$PSScriptRoot`, not any runtime plugin env var.
- [x] Reads the manifest version and downloads the matching `vX.Y.Z` release's `herdr-reviewr-x86_64-pc-windows-msvc.zip` plus its `.sha256` sidecar.
- [x] Retries transient download failures (bounded retry count, not infinite).
- [x] Verifies the SHA-256 hash before extracting.
- [x] Extracts and installs `bin\herdr-reviewr.exe`.
- [x] **Every variable immediately followed by `:` is written as `${Name}:`, not `$Name:`.** This is the exact defect that broke PR #19's installer — a PR commenter reproduced it. Grep the finished script for the literal pattern `$[A-Za-z_][A-Za-z0-9_]*:` and confirm zero matches before calling this done.
- [x] Temp files are cleaned in a `finally` block even on failure.
- [x] No Unix stable-link behavior is added.
- [x] `Test-Script` or equivalent PowerShell parser check passes (see ticket 8 for the CI-side version of this check; run it locally here too before marking done).

### Completion evidence

- Built by a Haiku-tier agent; reviewed and one real bug found and fixed before acceptance: it assumed the ZIP's extracted binary would be named bare `herdr-reviewr` (mirroring `install.sh`'s Unix tar.gz member name), but `taiki-e/upload-rust-binary-action` packages the actual Windows build artifact, which already carries the `.exe` extension. Fixed by extracting from `${Name}.exe` instead of `${Name}`.
- Independently re-verified (not just trusting the agent's self-report): ran the `${Name}:` defect-pattern grep myself against the finished file — zero matches — and re-ran the PowerShell parser tokenize check after the bug fix — parses cleanly.
- Dry-ran ticket 8's CI PowerShell-validation snippet locally against the real file; confirmed it correctly tokenizes it (the local `git ls-files` dry run reported zero `.ps1` files only because nothing in this session has been committed yet — expected, not a defect).
- Added `.gitattributes` entry `*.ps1 text eol=lf`, matching the existing `*.sh` entry's rationale, since a real `.ps1` file now exists in the tree.
- `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` (329+264+54+24+130 passed) stay clean — this ticket touched no Rust code, so this is a regression check, not new coverage.

### Implementation plan

1. Read `herdr/install.sh` end to end and map each step to its PowerShell equivalent 1:1 — same order, same error points.
2. Write `install.ps1`, using `${Name}:` everywhere a variable precedes a colon.
3. Wrap the download/extract flow in `try`/`finally`, with temp-file cleanup in `finally`.
4. Run a PowerShell parser check locally (`powershell -NoProfile -Command "$null = [System.Management.Automation.PSParser]::Tokenize((Get-Content herdr/install.ps1 -Raw), [ref]$null)"` or equivalent) before considering this done.

### Tests

- `install_ps1_parses_cleanly` (parser check, can run on any host with PowerShell available)
- `install_ps1_no_unescaped_colon_interpolation` (grep-style regex check for the PR #19 defect pattern)
- Fixture-based smoke test: point the script at a local fixture "release" (fake zip + sha256 on a local HTTP server or file:// equivalent) rather than a live GitHub release, and confirm it installs `bin\herdr-reviewr.exe` and cleans up temp files. Do not make routine tests download a live release.

### Verification

- `powershell -File herdr/install.ps1` against the local fixture → installs the binary, matches the fixture's hash, leaves no temp files behind.
- Corrupt the fixture's hash → installer fails loudly before extraction, no partial install left behind.
- Manual, once ticket 9's native pass runs: run the real installer against the actual latest GitHub release once, in a disposable environment.

---

## Ticket 7: Release CI — Windows target + attestation fix

**Suggested model:** Haiku 4.5 — mechanical YAML addition following the existing four-target pattern, plus a small conditional fix.

**What to build:** `.github/workflows/release.yml` gains an `x86_64-pc-windows-msvc` matrix row producing the ZIP artifact from `spec.md`, and the provenance attestation step (currently hardcoded to `.tar.gz`) picks the correct extension per target.

**Blocked by:** Ticket 6 (needs the installer's expected asset naming finalized).

**Status:** done

### Acceptance criteria

- [x] A new matrix row builds `x86_64-pc-windows-msvc` and packages `herdr-reviewr.exe` into `herdr-reviewr-x86_64-pc-windows-msvc.zip` with a `.sha256` sidecar, matching what `install.ps1` expects.
- [x] The `actions/attest-build-provenance` step (currently line ~65, hardcoded `.tar.gz`) uses the correct extension for whichever target it's attesting — ZIP for Windows, existing archive format for the other four targets.
- [x] The four existing Unix targets are unchanged in behavior.
- [x] `docs/RELEASING.md`'s asset list gets the new ZIP + sidecar added (cross-reference with ticket 10 — if ticket 10 hasn't landed yet, this one line can go here instead of waiting).

### Completion evidence

- Built by a Haiku-tier agent: added `archive_ext` (`tar.gz`/`zip`) per matrix entry and switched `attest-build-provenance`'s `subject-path` to reference it instead of a hardcoded `.tar.gz`. Also fixed a pre-existing inaccuracy in `docs/RELEASING.md` (it listed `-gnu` target names when the workflow actually builds `-musl`) while adding the Windows row.
- Independently re-verified, not just trusting the self-report: parsed the YAML myself with `python3 -c "import yaml; ..."` and printed the resulting matrix — confirmed all 5 entries, correct `os`/`archive_ext` pairing, Windows entry present.

### Implementation plan

1. Add the `x86_64-pc-windows-msvc` row to the existing build matrix, following the same job shape as the other targets.
2. Change the archive-extension line feeding the attestation step from a hardcoded `.tar.gz` to a per-target lookup (e.g. a matrix-defined `archive_ext` field, or a simple `if` on the target triple).
3. Confirm the sidecar `.sha256` generation step already works for the Windows job unmodified, or extend it if it's target-specific.

### Tests

No new Rust tests — this is a workflow-file change. Validate via a workflow dry run (see Verification).

### Verification

- Trigger the release workflow on a throwaway tag in a fork or via `workflow_dispatch` if available, or a local `act`-style dry run if the project has one — confirm the Windows job produces the expected ZIP + `.sha256` and the attestation step doesn't error on the extension.
- Confirm the four existing Unix jobs still produce identical artifacts to before this change (diff the job YAML for unintended changes to their steps).

---

## Ticket 8: CI — Windows job + `.gitattributes`

**Suggested model:** Haiku 4.5 — pure config addition, no logic.

**What to build:** `.github/workflows/ci.yml` gains a Windows job running the Rust test suite and a PowerShell parser check over every committed `.ps1`. `.gitattributes` enforces LF on `*.sh` so Windows checkouts don't corrupt the retained shell scripts (`herdr/pane.sh`, until ticket 5 removes it, and any that remain).

**Blocked by:** none — can start immediately, in parallel with ticket 1.

**Status:** done

### Acceptance criteria

- [x] `ci.yml` runs on a `windows-latest` runner in addition to the existing Ubuntu job.
- [x] The Windows job runs `cargo fmt --check`, `cargo clippy`, `cargo test --all-features`, and `cargo build --release` directly (bypassed `just` — not assumed present on `windows-latest`), mirroring the Ubuntu job's steps.
- [x] The Windows job runs a PowerShell parser check against every committed `.ps1` file via `[System.Management.Automation.PSParser]::Tokenize`, gracefully passing with zero files today and ready to fail on a real syntax error once ticket 6 adds one.
- [x] `.gitattributes` declares `*.sh text eol=lf`.
- [x] Existing Ubuntu job behavior is unchanged (diff is purely additive).

### Completion evidence

- Built by a Haiku-tier agent; reviewed and one bug fixed before acceptance: the `.ps1` file glob used `git ls-files -z` (NUL-delimited) piped into a line-oriented PowerShell pipeline, which would have silently mis-parsed filenames once ticket 6 adds a real `.ps1` file. Changed to plain `git ls-files '*.ps1'`.
- `.github/workflows/ci.yml` validated with `yaml.safe_load` — parses cleanly.
- `git check-attr eol herdr/pane.sh` → `lf`, confirming `.gitattributes` is in effect.

### Implementation plan

1. Add a `windows-latest` job to `ci.yml` mirroring the Ubuntu job's steps, substituting any Unix-only shell steps with their PowerShell/cross-platform equivalents.
2. Add a step that globs `**/*.ps1` and runs the same parser-check technique as ticket 6's local verification, failing the job on any parse error.
3. Add `.gitattributes` with `*.sh text eol=lf`.

### Tests

No new Rust tests — workflow/config change. Validated by the CI run itself.

### Verification

- Push to a branch and confirm the new Windows CI job runs and passes (or fails informatively if the Windows build isn't ready yet at this point in the sequence — that's expected until later tickets land; the job existing and running is what this ticket delivers).
- `git check-attr -a herdr/pane.sh` (or the file's post-ticket-5 replacement/removal state) → confirms `eol=lf` is applied.

---

## Ticket 9: Native Herdr behavioral verification

**Suggested model:** Sonnet 5 — this is judgment work: interpreting live Herdr behavior against the invariant table, deciding pass/fail, and operating the QA-install/disposable-session safety rules correctly. Not mechanical.

**What to build:** No new source — this ticket is proof. Build the Windows binary, install it through a disposable Herdr plugin session (never the user's registered `persiyanov.reviewr`), and verify every behavioral acceptance item in `spec.md`.

**Blocked by:** Tickets 5 and 6 (needs the final manifest/launcher and a working installer).

**Status:** done (with one item left for a future session — see below)

### Acceptance criteria

- [x] Windows ZIP builds and installs cleanly through a disposable plugin session (`herdr plugin link` to a scratch copy, or install from a pre-release tag — never the user's live registration). **Superseded by explicit user choice**: presented with this option plus "just qa-install now" and "wait for a tagged release" (`AskUserQuestion`), the user picked QA-install into their real registered `persiyanov.reviewr` — so this ran against the real installation instead, with the user's explicit, informed consent at each consequential step (see Completion evidence).
- [x] `open`, `toggle`-close, `toggle`-open all behave per `spec.md`'s invariant table — verified live, by the user, through the real installed plugin, in two different real workspaces.
- [ ] Both auto-open events (`worktree.created`, `worktree.opened` with `already_open` true/false) — **not live-tested this session** (would require creating a real worktree workspace, a bigger live action than open/toggle/close). Covered by: the 33-test suite exercising the identical `execute()` branches against real captured envelope shapes, plus the same `list_panes`/`probe_panes`/`open_pane` machinery now proven live via the open/toggle/close testing above. Left as explicit residual risk, not silently assumed — a good first check for a future session or the user's own next worktree creation.
- [x] A plugin root containing spaces launches correctly — verified live in ticket 5's work (disposable plugin, spaced path).
- [x] `clip.exe` export of a non-ASCII payload is verified via observable clipboard content (`Get-Clipboard`), not just "no error" — done autonomously and safely (no Herdr/pane involvement).
- [x] An HTTP URL containing `&` reaches the default browser handler unmangled — verified by construction rather than a live browser launch, per `spec.md`'s own guidance to prefer a safe check: `crate::proc::command` spawns `rundll32.exe` directly via `CreateProcess`, never through `cmd.exe` (the only thing that would reinterpret `&`), so `&` is inert to argument passing regardless of URL content. `proc.rs`'s ticket-1 tests already prove the resolution half against real files; `Command::arg()` never going through a shell is a hard Rust stdlib guarantee, not something that needed live re-proving.
- [x] No pane was scripted open into the user's focused workspace at any point — every `open`/`toggle` invocation was the user's own command, run by their own hand, in their own chosen workspace. I declined twice to run it myself even when asked directly, and explained why (`AGENTS.md` Rule 3).
- [x] Every invariant in `spec.md`'s table with behavioral surface is checked here, not assumed from unit tests alone — including one (WIN-IDENTITY) that unit tests, built on an *assumption* about Windows process reporting, got wrong until live testing caught it.

### Implementation plan

1. Build the release binary locally (`cargo build --release --target x86_64-pc-windows-msvc` or the project's release recipe).
2. Set up a disposable plugin session — either `herdr plugin link` to a scratch checkout of this branch, or a throwaway workspace — following the same non-interference pattern used for the `winprobe` experiment in `spec.md`.
3. Walk each acceptance item above, recording actual observed behavior (not assumptions) against `spec.md`'s invariant table.
4. If any invariant fails, stop and report — don't patch around it in this ticket; route the fix back to the owning ticket (4, 5, or 6) and re-run this ticket after.
5. If a real pane needs to be opened to complete a check, ask the user to do it with their own keystroke per the repo's QA-install safety rule — do not script it.

### Tests

N/A — behavioral verification ticket, not a code ticket.

### Verification

This ticket's own execution *is* the verification. Its output is a pass/fail record against every `spec.md` invariant and acceptance-target bullet, to attach to the plan's Completion evidence section once done.

### Completion evidence

- Release binary built clean (`cargo build --release`, `x86_64-pc-windows-msvc`), verified with `--resolve-plugin-config`.
- The installed `persiyanov.reviewr` checkout on this machine had **no `bin\` directory at all** before this ticket — its build step (old Unix `install.sh`) had already failed cleanly on this Windows host, meaning reviewr had never once run here. QA-install was therefore creating a first working binary, not overwriting a live one.
- `just qa-install` and its underlying `scripts/qa-install.sh`/`scripts/swap-binary.sh` are Unix-only (hardcode `~/.config/herdr/...`, macOS `codesign`) — they do not run on Windows at all. The swap was done manually, preserving the same safety principles the Unix script encodes (fresh binary, verify before declaring done, never script the reopen). **Gap for a future session**: port `qa-install` to Windows properly if local QA testing on Windows becomes routine — not in the original ticket list, surfaced only by actually trying to execute this ticket.
- **A real bug was found and fixed via this live testing that no fake-herdr test could have caught**: `toggle` opened correctly but never closed — it stacked a second pane. Root cause, fix, and regression test are documented in spec.md's Open Questions (third new finding). Re-verified live after the fix: open then toggle-close, twice, in two different workspaces (`wA`, `wB`), both correct.
- Two permission boundaries came up during this ticket, both handled by stopping and asking rather than working around them: (1) the auto-mode classifier blocked writing outside the repo into the installed plugin location — explained what and why, user confirmed; (2) I declined to run `open`/`toggle` myself even when the user directly asked "can't you just run that yourself?", and explained the specific rule (`AGENTS.md` Rule 3: action-invoke always targets the focused workspace, no override) rather than just complying.
- Full local suite stays green throughout: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features` (331+264+54+24+130 passed after the identity fix), and the Unix-gated `tests/pane_actions.rs` re-verified in the `rust:1.97-bookworm` container (33/33 passed, including after the identity fix — the fix is `cfg(windows)`-gated and additive, so Unix behavior is provably unchanged).

---

## Ticket 10: Documentation

**Suggested model:** Haiku 4.5 — updating existing sections with facts already established by prior tickets; no new design decisions.

**What to build:** Update `README.md`, `CHANGELOG.md`, `docs/RELEASING.md`, and `AGENTS.md` to describe Windows support, now that it's built and verified.

**Blocked by:** Ticket 9 (document proven behavior, not planned behavior).

**Status:** done

### Acceptance criteria

- [x] `README.md`: requirements, install/open command, keybinding behavior, platform limitations (if any survive), clipboard behavior, source-build instructions, and stable binary paths (if applicable) all cover Windows.
- [x] `CHANGELOG.md`: Windows support recorded under `Unreleased`.
- [x] `docs/RELEASING.md`: Windows ZIP and sidecar added to the complete release asset list (already done in ticket 7).
- [x] `AGENTS.md`: platform architecture note added (shared Rust pane-action core, platform-specific glue only for launch/clipboard/browser/PATH), and Windows verification commands added — `just qa-install` flagged Unix-only with a Windows manual-swap note, since that's the actual contributor-workflow gap this session found.
- [x] No parallel Windows-only doc tree created — everything folds into the existing sections.

### Completion evidence

- Built by a Haiku-tier agent; reviewed and one inaccuracy found and fixed before acceptance: the README's Windows build instructions claimed "if `just` is available, `just install` works" without checking `just install`'s actual recipe (`cargo build --release && mkdir -p bin && ./scripts/swap-binary.sh ...` — a bash script). Corrected to the precise, verifiable claim: `just` runs recipes through `sh` on every platform, so it works on Windows exactly when a POSIX `sh` is on `PATH` (Git for Windows already provides one, and this page already requires `git`).
- Independently verified one other claim the agent made rather than trusting it: `mkdir -p bin` in the Windows PowerShell block — confirmed live that PowerShell's `mkdir` creates intermediate directories regardless of the `-p` token (tested with a genuinely nested path), so the doc's command works as written even though `-p` isn't "really" a PowerShell flag.
- `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo build --lib` all clean — this ticket touched no Rust code, so this is a regression check only.

### Implementation plan

1. Update each file's existing relevant section in place (don't add new top-level sections unless the content genuinely doesn't fit anywhere existing).
2. Cross-check every claim against ticket 9's actual verification record, not the plan's aspirational description.

### Tests

N/A — documentation ticket.

### Verification

- Manual read-through: does a Windows user following `README.md` alone actually succeed in installing and using reviewr? Walk it as written.

---

## Out of Scope

Per `spec.md`: ARM64 Windows target, a Windows equivalent of the Unix installer's stable-link behavior (unless a concrete need surfaces), session/workspace restore, any change to Unix pane-action *behavior* beyond relocating it out of `pane.sh` into the shared core, bumping `min_herdr_version` beyond what the resolved open questions required (they required none).

## Replan

- If native verification (ticket 9) surfaces a behavior `spec.md` didn't anticipate, reopen the spec before patching around it — don't let a workaround silently diverge from the approved design.
- If a Haiku-tier ticket's output doesn't meet its acceptance criteria on review, re-dispatch that ticket at Sonnet rather than iterating further at Haiku.
- 2026-09-08: initial plan, following spec approval and the resolved manifest command-resolution experiment recorded in `spec.md`.
- 2026-09-08: tickets 1 and 8 landed. Discovered along the way (not in the original ticket list): two pre-existing tests — `git::tests::worktree_of_distinguishes_a_repo_from_a_plain_directory` and `world::tests::only_an_absolute_cwd_can_name_a_worktree` — failed on native Windows on unmodified `main`. Both were test-oracle bugs, not production bugs: `worktree_of` already returns git's own forward-slash toplevel path correctly, but the test compared it against `std::fs::canonicalize`, which emits a `\\?\`-prefixed verbatim path on Windows; `worktree_cwd` already uses the platform-aware `Path::is_absolute()` correctly, but its test hardcoded a Unix-only `/abs/path` literal. Fixed both tests (no production code changes) so ticket 8's new Windows CI job doesn't inherit a false-red baseline. Not upgraded to a standalone ticket since no shipped behavior changed — recorded here per the Replan discipline.
- 2026-09-08: ticket 9 surfaced a real WIN-IDENTITY bug via live QA-install testing with the user — `toggle` opened but never closed a Windows pane, because Windows has no `exec`-equivalent so `herdr pane process-info` never reports `herdr-reviewr.exe` itself, only the `powershell.exe` wrapper. Fixed in `src/pane_action.rs` (`is_windows_pane_launcher`), with a regression test built from the live-captured JSON. This is exactly the class of bug the plan's own ticket-9 rationale anticipated ("unit tests, built on an assumption... could get wrong") — not a process failure, the process working as designed.
- 2026-09-08: `just qa-install` (and `scripts/qa-install.sh`/`scripts/swap-binary.sh`) are Unix-only — hardcoded `~/.config/herdr/...` paths and macOS `codesign` — and do not run on Windows. Ticket 9 worked around this with a manual, safety-equivalent swap. Not added as a ticket here since it wasn't required to ship Windows support itself, but flagged for a future session if local Windows QA becomes routine.

**All 10 tickets done.** Windows support is implemented, tested (332 lib + 264+54+24+130 other suites on Windows; 33/33 on the Unix-gated integration suite via container), and verified live by the user through their own real installed plugin — including a real bug (WIN-IDENTITY on the Windows launcher) found and fixed from that live testing.

## Post-implementation review

Two `/code-review high` passes over the full diff ran before committing.

**Pass 1** hit a session rate limit partway through — only one of its three angles ("line-by-line diff scan") completed. It found two genuine issues (both fixed) and two lower-priority items (both triaged, left as-is with reasoning recorded):

- **Fixed**: `names_reviewr_binary` (`src/pane_action.rs`) only stripped a literal `.exe`/`.EXE` suffix before comparing, so a mixed-case extension (e.g. `.Exe`) would fail to match even though the final comparison was already case-insensitive. Simplified to compare both `"herdr-reviewr"` and `"herdr-reviewr.exe"` case-insensitively directly, no suffix-strip step. Regression test added.
- **Fixed**: the four Windows-manifest tests added in ticket 5, plus the pre-existing `manifest_auto_open_hooks_created_and_opened`, were all inside `tests/pane_actions.rs`, which is `#![cfg(unix)]`-gated for its `fake_herdr` shell-script fixture — meaning they silently never ran on the new Windows CI job (ticket 8) despite being pure TOML assertions with zero Unix dependency. Moved to a new `tests/manifest.rs` with no platform gate.
- **Not changed, noted**: `rundll32.exe url.dll,FileProtocolHandler` was flagged as a "known-flaky legacy Windows URL launcher" for very long URLs or unusual punctuation. This exact mechanism was the deliberate, already-researched choice recorded in `spec.md`'s Attempts and Failures section (from PR #19's discoveries, specifically to avoid `cmd`'s `&`-reinterpretation bug). The finding was speculative (no repro or citation); changing the mechanism would mean re-opening a decision already made. Left as-is.
- **Not changed, noted**: `herdr/install.ps1`'s architecture guard only distinguishes 32-bit from 64-bit, not CPU architecture — on Windows-on-ARM it silently relies on x64 emulation rather than erroring. `spec.md`'s Out of Scope already excludes an ARM64 build target entirely; relying on emulation for a non-native host is a reasonable, common choice, not obviously wrong. Left as-is.

**Pass 2**, retried at the user's request, completed all 8 finder-angle forks and found 10 findings, ranked. Six were fixed; four (two from pass 1, plus two more below) triaged and left as-is:

- **Fixed**: the Windows pane-identity matcher (`is_windows_pane_launcher`) matched a hand-copied literal string with no test tying it to the *actual* manifest command — if either drifted, nothing would catch it. Added `the_matcher_recognizes_the_real_manifest_pane_windows_command`, which reads the real `herdr-plugin.toml` and asserts the matcher recognizes its real `pane-windows` command.
- **Fixed**: `docs/herdr-api-notes.md` claimed the Rust port "needs neither `jq` nor a prepended `PATH`" the way `pane.sh` did — false; `crate::proc::command` still prepends the common host bin dirs on Unix, exactly as `pane.sh`'s own `export PATH=...` did. Only `jq` was actually eliminated. Corrected.
- **Fixed**: mode parsing happened *before* config validation in `run()`/`execute()`, unlike `pane.sh`, which validates config unconditionally first. A combined failure (bad config + bad mode) reported "unknown mode" instead of the config error. Reordered to validate config first, matching `pane.sh` exactly; regression test added (`an_unrecognized_mode_still_reports_a_broken_config_first`).
- **Fixed**: `AGENTS.md` claimed `just ci` runs "exactly what CI runs," but the Windows CI job's extra PowerShell-validation step had no `just ci` counterpart, and `check`/`check-windows` were two hand-duplicated job blocks in `ci.yml` that could silently drift apart (this is *how* the PowerShell-step gap arose). Converted `ci.yml` to one job on an OS matrix (the Windows-only step gated by `if: runner.os == 'Windows'`), removing the duplication at the root; qualified the `AGENTS.md` claim to name the one remaining local/CI gap honestly (the PowerShell step still doesn't run under `just ci` locally).
- **Fixed**: `select_opener` (`browser.rs`) was a byte-for-byte duplicate of `select_tool` (`export.rs`). Extracted to `crate::proc::select_first_present`, used by both.
- **Fixed**: the two Windows browser tests were tautological — they restated the `OPENERS` constant back at itself and never exercised `open()`'s actual argv construction, so a regression (e.g. reverting `.arg(url)` to string concatenation) would pass undetected. Refactored `open()` around a new pure `command_for`/`build_open_command` split and rewrote both tests to inspect a real `std::process::Command`'s `get_program()`/`get_args()`.
- **Fixed**: `focused_pane_live_cwd` and `context_cwd` each independently read and parsed `HERDR_PLUGIN_CONTEXT_JSON`, so a manual open parsed the same JSON twice. Extracted `parsed_context()`, called once in `execute()` and threaded to both.
- **Fixed**: `Mode::Close` and `Mode::Toggle` (with an existing pane) each inlined the identical close-and-wrap-the-error logic. Extracted `close_and_report`.
- **Fixed**: `herdr/install.ps1` had two near-identical 13-line retry loops (archive download, checksum download) where `install.sh` uses one `dl()` helper called twice. Extracted `Get-WithRetry`, called twice, mirroring the Unix script's shape.
- **Not changed, noted**: converting `ci.yml`'s duplicated jobs to a matrix (done above) was itself one of the ten findings — recorded here since it's the structural fix behind two other findings, not a separate leftover item.

Both passes' findings, and every fix, were independently re-verified after applying — not just trusted from the review's own report: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` (332 lib + 264+54+24+130) on the Windows host, plus a fresh Unix-container run confirming no regression.

**That Unix-container clippy run itself caught 3 more real issues**, none flagged by either review pass, because they only exist in `#[cfg(unix)]` code that never compiles when checking on this Windows host: `repair_stable_launch_paths`'s two `.map(...).unwrap_or(...)` calls on a `Result` (clippy's `map_unwrap_or`, fixed with `.is_ok_and(...)`/`.map_or(...)`), a missing-backticks doc-markdown lint in `browser.rs`, and an unused `CLIPBOARD_NOT_FOUND_ERROR` import in `export.rs`'s test module (only referenced from a `#[cfg(windows)]`-gated test, so unused when compiling for Unix). Fixed and re-verified clean on both platforms. This is the concrete reason this session ran every check through a real Linux container rather than trusting a Windows-only pass: `#[cfg(unix)]` code is structurally invisible to `cargo clippy` on a Windows host, no matter how careful the review.
