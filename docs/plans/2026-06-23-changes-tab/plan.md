# herdr-review Changes tab — Delivery Plan

**Specs:** ../../../specs/ — the living reference this plan delivers

## Milestone Map

1. **Changes tab** — single milestone; the review loop end to end, in a herdr pane.

## Goal

In a herdr pane on a git worktree, browse changed files, read diffs, leave/edit/delete line-range comments (added and removed lines), and send them — one or all — to the agent's pane or the clipboard. Comments are in-memory and never lost to a refresh.

## Definition of Done

- `herdr-review` runs in a right split pane and shows the `Changes` view for the worktree.
- Scopes `uncommitted` and `branch` list changed files with stats; selecting one shows its diff.
- You can comment on a line range including removed lines, edit it in place, delete it, jump between comments, and list them.
- Send this / Send all fills the agent pane input via `herdr agent send` and focuses it; Copy this / Copy all writes the same blocks to the clipboard; a successful export consumes what it sent.
- Polling every 2s refreshes files and the open diff without touching the comment input or saved comments.
- `cargo test` green; `cargo clippy -- -D warnings` and `cargo fmt --check` clean.

## Exit State

Closed list — anything not named here is not built.

- The `herdr-review` binary renders the `Changes` tab per `tui.md`: components `TabBar` (Changes only), `FileList`, `DiffView`, `CommentInput`, `CommentsList`, `StatusBar`; keyboard and mouse; the keymap of `tui.md`.
- Scopes `Uncommitted` and `Branch` per `review-model.md`. No `last-turn`.
- `Comment { file, side, start, end, lines, text }` and `ChangedFile { path, kind, additions, deletions }` per `review-model.md`, in an in-memory store with add / edit / delete / consume-on-export / stale-flag. No on-disk store.
- Diff parsing from `git diff`/`git status` into rows and into diff lines carrying their side, per `review-model.md`.
- Export block format per `review-model.md`, behind a target with two impls: `Agent` (resolve via tab→workspace, `herdr agent send` + `herdr agent focus`) and `Clipboard` (`pbcopy`). Consume only on success.
- `herdr-plugin.toml` declaring the right split pane and a toggle key, per `herdr-host.md`.
- Config via flags: poll interval (default 2s) and base branch (default `origin/main`).

## Specs Touched

All promote at the merge gate (the single gate). Each is fully realized — the roadmap sections stay as forward-looking notes, not built surface.

| Spec | What this plan realizes | At the gate |
| --- | --- | --- |
| `overview.md` | the product shape and invariants of the Changes loop | Draft → Current |
| `review-model.md` | scopes, changed files, comments, lifecycle, export | Draft → Current |
| `tui.md` | layout, interaction, refresh | Draft → Current |
| `herdr-host.md` | pane hosting, agent send/focus, clipboard | Draft → Current |

## Out of Scope

Orientation only — the Exit State already excludes these by omission.

- `All files` tree, `Checks`/`gh` tab, `last-turn` scope — roadmap in `overview.md`.
- Durable comment store, agent-status event subscription — roadmap in `review-model.md` / `herdr-host.md`.

## Likely Files

- `Cargo.toml` — add `ratatui` (re-exports crossterm); clipboard/git/herdr via `std::process::Command`.
- `src/main.rs` — terminal lifecycle, the event loop, poll timer.
- `src/lib.rs` — `run()` wiring.
- `src/git.rs` — scope diffs, changed-files, unified-diff parsing into lines + side.
- `src/model.rs` — `Comment`, `ChangedFile`, `Scope`, the in-memory `CommentStore`.
- `src/export.rs` — block formatting; `ExportTarget` with `Agent` and `Clipboard`.
- `src/herdr.rs` — agent resolution from `agent list` (tab → workspace), `send`, `focus`; `HERDR_*` env.
- `src/tui/` — `app.rs`, `tab_bar.rs`, `file_list.rs`, `diff_view.rs`, `comment_input.rs`, `comments_list.rs`, `status_bar.rs`.
- `src/config.rs` — flags + defaults.
- `herdr-plugin.toml` — pane + toggle key.

## Execution Plan

1. `git.rs` + `model.rs`: scopes, changed-files, diff parsing into sided lines; `CommentStore` (add/edit/delete/consume/stale). Unit tests on parsing and store.
2. `herdr.rs` + `export.rs`: agent resolution, `send`/`focus`, `pbcopy`; block formatting; consume-on-success. Tests on formatting; live check `send` lands in a pane.
3. `tui/`: components + event loop + 2s poll; selection/scroll preserved; freeze input and the open diff while composing.
4. Comments: add on selection (incl. removed lines), `e` edit, `d` delete, `n`/`N` jump, `l` list; empty and binary states.
5. Mouse: click a file, click `Send all`, drag-select a range.
6. `herdr-plugin.toml`; wire `main`; run in a real pane and walk the DoD.

## Verification

- **Done:** in a herdr split pane on a repo with changes — browse files, view a diff, comment on an added and a removed line, edit one, `Send all` to a `cc` pane (block appears in its input + pane focuses), `Copy all` then `pbpaste` matches; while composing, a poll leaves the input and saved comments intact.
- **Tight:** the diff equals Exit State — no `All files`/`Checks`/`last-turn`, no on-disk store, no event subscription, no second `TabBar` entry.
- **Invariants upheld:**
  - read-only on git (`overview.md`) → grep the source for `commit`/`add`/`stage`/`checkout`; only `diff`/`status`/`merge-base`/`rev-parse` run.
  - a comment is never lost to a refresh (`overview.md`, `review-model.md`) → test: a poll during an in-progress comment and during a saved comment keeps both.
  - consume only on success (`review-model.md`) → test: a failed export leaves the comment in the store.
  - forbids `unsafe` (`overview.md`) → crate lint already set.

## Replan Triggers

- If `herdr agent send`/`focus` to a live agent's input doesn't behave as documented, revisit the send target before building more atop it.
- If ratatui mouse events don't arrive inside a herdr pane, ship keyboard-only for v1 (the keymap is already provisional) and note it.
- If snippet extraction is fiddly for renames / no-newline / binary, narrow what `lines` captures and record it in `review-model.md`.

## Replan Log

- 2026-06-23: initial plan from approved contract.
- 2026-06-23: implementation adopted a two-pane focus model (`tab` switches focus; `u`/`b` switch scope) instead of `tab`-switches-scope, since anchoring/jumping comments needs an independently focusable diff cursor. `tui.md` keymap updated to match.
