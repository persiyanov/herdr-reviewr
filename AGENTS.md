# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository. AGENTS.md is the primary file. CLAUDE.md is a symlink to it.

herdr-reviewr is a Rust TUI (ratatui) code-review sidebar: it runs in a [herdr](https://herdr.dev) pane beside a coding agent, shows the agent's diff, takes line comments, and sends them back to the agent's input. One binary, one git worktree per pane. It also runs standalone (`cargo run` in any repo).

## Commands

- `just test` — full test suite. Single test: `cargo test <name>` (unit tests live beside the code, integration tests in `tests/`: `cargo test --test app_flow <name>`).
- `just lint` — clippy with warnings as errors. `just fmt` / `just fmt-check` — rustfmt.
- `just ci` — exactly what CI runs (fmt-check, lint, test, release build).
- `just qa-install` — put a local build into the user's real herdr panes. See "QA install" below before using it.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` — perceived-latency benchmark (keypress → painted frame, via PTY), the acceptance instrument. `cargo run --release --example bench_latency -- <repo>` attributes a slow number to its component calls. Baselines in `scripts/bench-results/`. Run before/after any change to the reload, render, git, or highlight paths, and compare medians A/B under the same system load (rebuild the old binary to a second target dir and interleave runs — absolute numbers drift with background load).

## Spec-first

`specs/` holds the contracts (`overview.md` is the map). Behavior changes land in the spec and the code together, and code comments cite the spec section they implement. Before changing user-visible behavior, read the governing spec file, and treat divergence between spec and code as a finding to raise, not silently fix.

Load-bearing invariants (specs/overview.md):

- O1: the sidebar never mutates the worktree, index, or branches. Its only git write is the private baseline ref under `refs/reviewr/`.
- O3/O4: comments are never lost to a refresh or the agent's edits, and leave only by explicit export. The comment store is in-memory **by design** — do not propose persisting it.
- O6 (Continuity): place state (cursor, scroll, tab, scope, folds, selection, layout) moves only under the user's own input. World events (polls, refreshes, fetch results) may only *reconcile* it: match by identity first (path, comment author+anchor — never row index), fall back to the nearest surviving target, clamp last. Derived state on screen may be stale, never wrong: blank a view only when its identity changed, never because the same thing gained newer content.

## Architecture

The runtime is a single-threaded frame loop (`event_loop` in `src/lib.rs`): draw → wait for input or poll deadline → mutate `App` → draw. Clipboard, agent-send, and per-file diff builds run synchronously between frames. Five things run on worker threads: the world worker (`src/world.rs` — the refresh build and turn tracking), the search worker (`src/search.rs` — the fff-search engine, which runs its own scan, watch, and content-index threads), the PR input probe, the PR GitHub fetch (`gh`), and config recovery. World results land through `land_world_completion`: input-tagged, latest-wins, reconciled only while the view still matches (see Continuity above).

- `src/app.rs` — the `App` state machine. Tabs (`Changes`/`AllFiles`/`Pr`), scopes (`Uncommitted`/`Branch`/`LastTurn`), `Focus` (files vs diff pane), `Mode` (`Normal`, the `Composing`/`List` overlays, and the body-replacing `Search` screen). `reconcile_world()` is the one place a world snapshot touches place state; `reload()` is the synchronous build+reconcile pair used at startup, first tab visits, and scope switches. Each file tab stashes its full place state on switch-away (`swap_active_with_stash`). While composing, the open diff is frozen (reconcile skips it) so a draft's anchor can't move.
- `src/world.rs` — the world worker: the pure snapshot build (`WorldInput` → `WorldSnapshot`), the request/completion channels (latest-wins by generation), and `TurnHost` (the turn tracker, worktree snapshots, and the baseline ref write, all worker-side).
- `src/git.rs` — every git subprocess. `changed_files` (scope changesets), `all_files` (tracked + untracked + ignored via `ls-files` — never use `git status --ignored`, it walks inside ignored trees and costs seconds), `snapshot_worktree` (temp-index `add -A` + `write-tree` for turn baselines), baseline refs.
- `src/diff.rs` — `FileDiff` build (syntect highlight both sides, similar-line pairing, word emphasis, folds) and `DiffCache`, keyed by path and gated by content hash. Cleared on scope switch and theme change.
- `src/ui.rs` — all rendering. Row heights and wrapping recompute per frame across the visible diff, so render cost scales with open-file size.
- `src/forge.rs` + the `PrRefresh`/`PrCoordinator` state machines in `lib.rs` — the PR snapshot. Fetches are tagged with the input (repository identity, pinned HEAD and base, publication points) that produced them, and a result paints only if a fresh probe proves the input still matches. This generation/input-tag pattern is the template for moving other derived state off-thread.
- `src/turn.rs` — the pure turn state machine: a resting→working edge starts a turn, and a pending candidate promotes to the `last-turn` baseline once the worktree diverges from it. The world worker's `TurnHost` drives it; `src/herdr.rs` holds the herdr CLI calls.
- `src/model.rs` — `CommentStore` (in-memory), comment anchoring (`diff_anchored` distinguishes diff comments from All-files content comments — each renders only in its own view).
- `src/export.rs` — comment export: format all, send via `herdr agent send` or clipboard, consume-on-success only.
- `src/config.rs` — plugin config: the whole file validates before every frame/action (invariants C1–C8 in specs/config.md). An invalid config blocks all review work until recovery, which carries authored state.
- `herdr-plugin.toml` + `herdr/sidebar.sh` — plugin packaging: pane, toggle/open/close actions, worktree.created auto-open.

## QA install — putting a local build into the user's herdr panes

The user tests builds in real herdr panes. The panes run the GitHub-installed plugin's binary at `~/.config/herdr/plugins/github/persiyanov.reviewr-<hash>/bin/herdr-reviewr`, NOT anything in this worktree. Full procedure: `docs/qa-install.md`. Short form:

```
just qa-install
```

Then tell the user to close and reopen their reviewr panes with the toggle keybinding. Done.

Three rules. Each one has already burned a session:

1. **Never overwrite that binary in place.** `cp` onto the existing file keeps the inode and macOS SIGKILLs the binary at every launch (exit 137, blank panes, no log). Replace through a fresh inode and re-sign — which is exactly what `just qa-install` does. Do not improvise the swap by hand.
2. **Swapping the file does not restart running panes.** They keep the old binary image until closed and reopened. Refresh inside reviewr does nothing for this.
3. **Never script pane opens.** The plugin's `open`/`toggle` actions act on the currently focused workspace and ignore `HERDR_WORKSPACE_ID`. Automating reopens stacks sidebars into whatever space the user is looking at. Closing via `herdr/sidebar.sh close` is safe. Reopening is the user's keystroke, always.

Rollback: `bin/herdr-reviewr.release-backup` sits beside the installed binary, swap it back the same fresh-inode way (or `herdr plugin install` to restore the release).
