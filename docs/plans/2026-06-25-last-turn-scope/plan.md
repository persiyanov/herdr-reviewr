# last-turn scope — Delivery Plan

**Specs:** ../../../specs/ — the living reference this plan delivers

## Milestone Map

1. **last-turn scope** — single milestone; a third changeset scope showing the agent's most recent change-producing turn, captured by polling agent status and snapshotting the worktree.

## Goal

Add the `last-turn` scope: select it with `t`, and the diff shows what the agent changed in its most recent change-producing turn, live as the turn runs, against a non-disruptive worktree snapshot that persists across a sidebar restart.

## Definition of Done

- `t` (and the header chip click cycle) selects `last-turn`; the file list and diff show the turn baseline against the live worktree.
- The view autopopulates each poll as the agent edits; a question-only turn keeps the previous turn's diff; a permission prompt mid-turn keeps it one turn.
- Before any turn start is observed, and when herdr is unavailable, the scope shows the `waiting for the agent's next turn` empty state.
- The baseline survives a sidebar restart; snapshotting never changes `git status`, the index, or any branch.
- `cargo test` green, including the turn-tracker state machine and a snapshot non-disruptiveness test.

## Exit State

The new surface, closed — anything not named here is not built. Realizes the `last-turn` design across `review-model.md`, `herdr-host.md`, `tui.md`, `overview.md`.

- `model.rs` — `Scope::LastTurn` with `label()` `"last turn"`; `toggled()` cycles `Uncommitted → Branch → LastTurn`.
- `turn.rs` (new) — `Status` (`Idle`/`Working`/`Blocked`/`Done`/`Unknown`) parsed from the herdr string; `TurnTracker { prev, candidate, baseline }` deciding capture (a `Idle|Done → Working` edge) and promote (candidate present and worktree diverged).
- `git.rs` — `snapshot_worktree` (temp index seeded from `.git/index`, `add -A`, `write-tree`); `worktree_differs(tree)`; `read_baseline_ref`/`write_baseline_ref` on `refs/reviewr/turn-base/<key>`; `worktree_key` (FNV-1a hex of the top-level path); `changed_files` takes the turn baseline and runs the untracked block for `LastTurn` as for `Uncommitted`.
- `herdr.rs` — `resolved_agent_status() -> Result<Option<String>>`, reusing the existing agent resolver.
- `app.rs` — `App` holds a `TurnTracker` and the worktree key; `track_turn()` orchestrates status → tracker → snapshot/promote/ref each poll; `content_sides` resolves the `LastTurn` old side from the baseline tree; `reload` passes the baseline to `changed_files`; `App::new` loads the baseline from the ref; `LastTurn` with no baseline yields an empty changeset.
- `lib.rs` — the poll block calls `app.track_turn()`; `t` maps to `set_scope(LastTurn)`.
- `ui.rs` — the scope chip renders `last turn` and cycles `u/b/t` on click; the footer hints `t`; the `LastTurn`-with-no-baseline empty state paints `waiting for the agent's next turn` in both panes.

## Specs Touched

All promote at the merge gate (the single gate); each is fully realized here.

| Spec | What this plan realizes | At the gate |
| --- | --- | --- |
| `review-model.md` | the `last-turn` scope and turn-baseline meaning | Draft → Current |
| `herdr-host.md` | turn tracking, the snapshot, the private ref | Draft → Current |
| `tui.md` | the `t` scope key, chip cycle, empty state | Draft → Current |
| `overview.md` | `last-turn` in scope; the private-ref invariant | Draft → Current |

## Out of Scope

Orientation only — the Exit State already excludes everything by omission.

- Socket subscription to `pane.agent_status_changed` — tracking polls instead (`herdr-host.md` decision).
- A `last-turn` config flag or a tunable status-sample interval — reuses the existing poll.
- Garbage-collecting superseded baseline objects — left to `git gc`.

## Likely Files

- `src/turn.rs` — created: `Status`, `TurnTracker`, state-machine tests.
- `src/model.rs` — `Scope::LastTurn`, label, 3-way `toggled`.
- `src/git.rs` — snapshot, ref, key, divergence helpers; `changed_files` baseline arg.
- `src/herdr.rs` — `resolved_agent_status`.
- `src/app.rs` — tracker state, `track_turn`, `content_sides`, `reload`, `App::new`.
- `src/lib.rs` — poll-loop `track_turn` call; `t` key.
- `src/ui.rs` — scope chip, footer hint, empty state.
- `tests/git_repo.rs`, `tests/app_flow.rs`, `tests/render.rs` — snapshot/diff, scope switch, empty state.

## Execution Plan

1. `model.rs`: add `Scope::LastTurn`, `label`, the 3-way `toggled`; update the scope test.
2. `turn.rs`: `Status` + parse, `TurnTracker` capture/promote logic; unit-test `Idle→Working` and `Done→Working` capture, `Blocked→Working` and `Unknown→Working` continuation, and no-change-keeps-previous.
3. `git.rs`: `snapshot_worktree` (seed temp index from `.git/index` so unchanged files are not re-hashed), `worktree_differs`, ref read/write, `worktree_key`; extend `changed_files` with the baseline and the `LastTurn` untracked block; tests in `tests/git_repo.rs`.
4. `herdr.rs`: `resolved_agent_status`, reusing `resolve_agent_pane`'s resolution to read the chosen agent's `agent_status`.
5. `app.rs`: hold `TurnTracker` + key; load the baseline in `App::new`; `content_sides` `LastTurn` branch; `reload` passes the baseline; `track_turn` runs the status → snapshot → promote → `update-ref` cycle, swallowing herdr/git errors to the log.
6. `lib.rs`: call `app.track_turn()` in the poll block (every poll, any scope); bind `t` to `set_scope(LastTurn)`.
7. `ui.rs`: scope chip label + 3-way click cycle, footer `t` hint, the empty state for `LastTurn` with no baseline.
8. `tests/app_flow.rs`, `tests/render.rs`: scope switch into `last-turn`, the empty state, and send still resolving.

## Verification

- **Done:** `cargo test` green; a live herdr-pane run — agent edit then `t` shows the turn diff; a question-only turn keeps the prior diff; a mid-turn permission approval stays one turn; restart resumes the baseline.
- **Tight:** the diff equals Exit State — one new scope, one new module, the named helpers; no config flag, no socket code, no extra refs.
- **Invariants upheld:** the no-mutation invariant (`overview.md`) — a `git_repo` test asserts `snapshot_worktree` leaves `git status`, `.git/index`, and the branch list unchanged and writes only under `refs/reviewr/`; `#![forbid(unsafe)]` already enforces the unsafe ban.

## Replan Triggers

- If a per-poll `herdr agent list` is material on a slow herdr or large repo, sample status every Nth poll behind a config knob.
- If `git add -A` snapshotting is slow despite the index seed, defer the snapshot or cache by index mtime.
- If `done` or `blocked` behave unlike the docs in practice, revisit the resting-state set in `turn.rs` and the `herdr-host.md` rule.

## Replan Log

- 2026-06-25: initial plan from the approved Draft specs; herdr status enum (`idle`/`working`/`blocked`/`done`/`unknown`), `blocked`/`done` semantics, and the temp-index snapshot cost verified live before planning.
