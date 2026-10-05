# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository. AGENTS.md is the primary file. CLAUDE.md is a symlink to it.

herdr-reviewr is a Rust TUI (ratatui) code-review pane: it runs in a [herdr](https://herdr.dev) pane beside a coding agent, shows the agent's diff, takes line comments, and sends them back to the agent's input. One binary, one git worktree per pane. It also runs standalone (`cargo run` in any repo).

## Commands

- `just test` — full test suite, with the herdr environment stripped so no test can reach a live agent pane. Inside herdr, run a single test the same way: `env -u HERDR_WORKSPACE_ID -u HERDR_PANE_ID -u HERDR_SOCKET_PATH HERDR_BIN_PATH=false cargo test <name>` (unit tests live beside the code, integration tests in `tests/`: `cargo test --test app_flow <name>`).
- `just lint` — clippy with warnings as errors. `just fmt` / `just fmt-check` — rustfmt.
- `just ci` — what CI's unix job runs (fmt-check, lint, test, release build). The Windows jobs run on CI only: dispatch them on a branch with `gh workflow run ci.yml --ref <branch>`.
- `just qa-install` — put a local build into the user's real herdr panes. See "QA install" below before using it.
- `just smoke-edit` — PTY smoke test of the editor path (`e`) against a real release binary. Unit tests stop at the argv; everything after it is terminal state, so run this after any change to `run_editor`, the terminal mode stack, or the editor dialects. Not part of `just ci`: it drives a pty and takes about a minute.
- `cargo run --example snapshot -- <out-dir>` — paints every theme's real frames, scene by scene, as HTML fragments for a side-by-side color review. Ad-hoc tooling for theme and color work, not a gate; run it with the herdr environment stripped.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` — perceived-latency benchmark (keypress → painted frame, via PTY). Ad-hoc tooling, not a gate: run it when a change might feel slower. `cargo run --release --example bench_latency -- <repo>` attributes a slow number to its component calls. The one committed baseline is `scripts/bench-results/baseline.json` — replace it when a change moves the numbers, never add per-round runs. For an A/B, rebuild the old binary to a second target dir and interleave runs under the same system load — absolute numbers drift with background load.

## Invariants

New behavior is designed with `/brainstorming` and sequenced with `/planning` in the conversation; the repo keeps no spec tree. The commit message and the changelog carry the decisions.

Code comments are one line. Two need a serious reason; three mean the code should change instead.

Load-bearing invariants. Cite them by name:

- **No writes**: reviewr never mutates the worktree, index, or branches. Its only git writes are private refs under `refs/worktree/reviewr/` (the turn baseline and the base pick), its private index copies under `<git dir>/reviewr/`, and the objects a turn snapshot stores, as `git add` would. Worktree changesets (`changed_from`) and snapshots run on the copies, and every other git call runs with `diff.autoRefreshIndex=false` (`git_command`), so the real index is never refreshed.
- **Comments survive**: comments are never lost to a refresh or the agent's edits, and leave only by explicit export. The comment store is in-memory **by design** — do not propose persisting it.
- **Continuity**: place state (cursor, scroll, tab, scope, folds, selection, layout) moves only under the user's own input. World events (polls, refreshes, fetch results) may only *reconcile* it: match by identity first (path, comment author+anchor — never row index), fall back to the nearest surviving target, clamp last. Derived state on screen may be stale, never wrong: blank a view only when its identity changed, never because the same thing gained newer content.

## Architecture

The runtime is a single-threaded frame loop (`event_loop` in `src/lib.rs`): draw → wait for input or poll deadline → mutate `App` → draw. Clipboard, agent-send, and per-file diff builds run synchronously between frames, and a terminal editor holds the loop for its whole session by design (`policies/ux-responsiveness.md`). Five things run on worker threads: the world worker (`src/world.rs` — the refresh build and turn tracking), the search worker (`src/search.rs` — the fff-search engine, which runs its own scan, watch, and content-index threads), the PR input probe, the PR forge fetch (`gh`/`glab`/`az`/`tea`), and config recovery. World results land through `land_world_completion`: input-tagged, latest-wins, reconciled only while the view still matches (see Continuity above).

- `src/app.rs` — the `App` state machine. Tabs (`Changes`/`AllFiles`/`Pr`), scopes (`Uncommitted`/`Branch`/`LastTurn`), `Focus` (files vs diff pane), `Mode` (`Normal`, the `Composing`/`List` overlays, and the body-replacing `Search` screen). `reconcile_world()` is the one place a world snapshot touches place state; `reload()` is the synchronous build+reconcile pair used at startup, first tab visits, and scope switches. Each file tab stashes its full place state on switch-away (`swap_active_with_stash`). While composing, the open diff is frozen (reconcile skips it) so a draft's anchor can't move. A markdown file shows as `Row::Rendered` rows, each carrying its block's source range.
- `src/world.rs` — the world worker: the pure snapshot build (`WorldInput` → `WorldSnapshot`), the request/completion channels (latest-wins by generation), and `TurnHost` (the turn tracker, worktree snapshots, and the baseline ref write, all worker-side).
- `src/git.rs` — every git subprocess. `changed_from`/`changed_between` (scope changesets, one `git diff --raw --numstat` each), `all_files` (tracked + untracked + ignored via `ls-files` — never use `git status --ignored`, it walks inside ignored trees and costs seconds), `snapshot_worktree` (`add -A` + `write-tree` on a private `IndexCopy` for turn baselines), baseline refs. `diff_sides` reads both sides of a tracked file from one full-context `git diff`, so the diff compares exactly what `git diff` compares under any `core.autocrlf`, eol attribute, or filter. reviewr never replays git's clean step.
- `src/diff.rs` — `FileDiff` build (syntect highlight both sides, similar-line pairing, word emphasis, folds) and `DiffCache`, keyed by path and gated by content hash. Cleared on scope switch and theme change.
- `src/markdown.rs` — the markdown renderer. Every rendered line maps to its block's source range, and `silent` lists the source lines nothing renders.
- `src/rendered.rs` — a file tab's `RenderedView` (choice, content, render, marks) and the `RenderedIndex` over its rows: unit runs, lead rows, and the one landing rule for a source line.
- `src/marks.rs` — rendered change marks. An inserted line belongs to its new block, a deleted line to its block in the old document (the old side renders too), and markers stand for the rest.
- `src/roles.rs` — the color roles. A theme's primitives derive every fill (a layer under text, stacked in `LAYERS`) and every ink (a text or glyph color), each resolved per fill so it reads there. Components paint roles, never a hue; `legible`/`readable` keep content colors (syntax, markdown headings) readable on any fill, hue kept.
- `src/theme.rs` — the theme catalog: each built-in theme's primitives, syntax pairing, and the official fills it sets itself.
- `src/ui.rs` — all rendering. Row heights and wrapping recompute per frame across the visible diff, so render cost scales with open-file size.
- `src/forge.rs` + the `PrRefresh`/`PrCoordinator` state machines in `lib.rs` — the PR snapshot. Fetches are tagged with the input (repository identity, pinned HEAD and base, the branch's published heads and pin) that produced them, and a result paints only if a fresh probe proves the input still matches. This generation/input-tag pattern is the template for moving other derived state off-thread.
- `src/gitlab.rs` / `src/azure_devops.rs` / `src/gitea.rs` — the `glab`, `az`, and `tea` providers behind the forge boundary in `forge.rs`, each mapping its CLI's payloads onto the one `PrSnapshot` shape. `tea api` exits zero on any HTTP answer, so the Gitea provider reads the status from `--include`'s stderr, never the exit code.
- `src/turn.rs` — the pure turn state machine: a resting→working edge starts a turn, and a pending candidate promotes to the `last-turn` baseline once the worktree diverges from it. The world worker's `TurnHost` drives it; `src/herdr.rs` holds the herdr calls: the CLI, and the socket for the send.
- `src/model.rs` — `CommentStore` (in-memory), comment anchoring (`diff_anchored` distinguishes diff comments from All-files content comments — each renders only in its own view).
- `src/editor.rs` — the editor command: the source order (`editor` key, `$VISUAL`, `$EDITOR`, git's `core.editor`), a name-keyed dialect table (how each editor takes a line, and whether it draws in the pane), quote-aware splitting, and the `editor` key's `{file}`/`{line}` template. Pure argv resolution, spawning nowhere. `run_editor` in `lib.rs` owns the spawn, and hands the pane over for a terminal editor, blocking the frame loop for that editor's whole session.
- `src/input.rs` — where the frame loop's input comes from. Unix uses crossterm's `poll`/`read` unchanged. Windows reads the console in VT input mode (`input/windows.rs`, the crate's one `unsafe` call: the console wait) and parses the bytes into the same crossterm events (`input/vt.rs`), so a paste arrives as one `Event::Paste`.
- `src/export.rs` — comment export: format all, send as one `pane.send_text` request over herdr's socket or copy to the clipboard, consume-on-success only.
- `src/config.rs` — plugin config: the whole file validates before every frame/action. An invalid config blocks all review work until recovery, which carries authored state.
- `src/actions.rs` — the plugin actions, run as `herdr-reviewr --action <toggle|open|close|auto-open>`: config validation first, then the workspace's action lock (`$HERDR_PLUGIN_STATE_DIR/action-<workspace id in hex>.lock`, held until an open reads as reviewr, so concurrent actions on one workspace serialize, and the auto-open event gives way to a held lock instead of waiting), then a live sweep of the workspace for panes running the review UI (`pane process-info` per pane, concurrently), then close or open. `NonUiRun` is the one rule for which argv is not the review UI, shared by `main.rs` dispatch and that sweep.
- `herdr-plugin.toml` — plugin packaging: the pane, the toggle/open/close actions, and the worktree workspace-birth auto-open event, each running `bin/herdr-reviewr` directly.

## QA install — putting a local build into the user's herdr panes

The user tests builds in real herdr panes. The panes run the GitHub-installed plugin's binary at `~/.config/herdr/plugins/github/persiyanov.reviewr-<hash>/bin/herdr-reviewr`, NOT anything in this worktree. Full procedure: `docs/qa-install.md`. Short form:

```
just qa-install
```

Then tell the user to close and reopen their reviewr panes with the toggle keybinding. Done.

Three rules. Each one has already burned a session:

1. **Never overwrite that binary in place.** `cp` onto the existing file keeps the inode and macOS SIGKILLs the binary at every launch (exit 137, blank panes, no log). Replace through a fresh inode and re-sign — which is exactly what `just qa-install` does. Do not improvise the swap by hand.
2. **Swapping the file does not restart running panes.** They keep the old binary image until closed and reopened. Refresh inside reviewr does nothing for this.
3. **Never script pane opens.** The plugin's `open`/`toggle` actions act on the currently focused workspace and ignore `HERDR_WORKSPACE_ID`. Automating reopens stacks panes into whatever space the user is looking at. Closing via `herdr plugin action invoke close --plugin persiyanov.reviewr` is safe: it sweeps the focused workspace's reviewr panes. Running `herdr-reviewr --action close` by hand refuses, since actions run only as herdr plugin actions. Reopening is the user's keystroke, always. The one exception is a seat no user sits at: the Windows QA VM and CI's headless herdr, where a script opens the panes it then drives.

Rollback: `just qa-restore` puts back the release binary and manifest that `just qa-install` backed up beside them (or `herdr plugin install` restores the release).
