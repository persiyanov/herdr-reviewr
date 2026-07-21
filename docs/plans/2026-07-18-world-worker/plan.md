# World worker — Plan

Delivers `specs/tui.md#refresh` (refresh off the frame loop) and the `specs/config.md` baseline-ref clause.

## Problem

A keypress landing during a reload waits for it: the chained tab-then-`f` press carries ~146ms of residue on the landing repo. Every poll tick runs `herdr agent list`, sometimes a worktree snapshot, and a full reload on the UI thread, so input can stutter every 2 seconds. herdr switches spaces in ~7ms — the sidebar should feel the same.

## Goal

World refresh — the changed set, the file tree, turn sampling, snapshots — moves to one worker thread, so no keypress or paint waits on a git spawn. A debounced `⟳` in the tab strip replaces the PR tab's `· refreshing…` note.

## Definition of Done

- [x] `bench_tui.py`: the chained tab-then-`f` press paints in single-frame time (first-byte sub-ms; the painted landing sits at main's sync-reload level). The prior residue was ~146ms.
- [x] A result landing after a scope, tab, or baseline change is discarded, and a newer request supersedes an older one (integration tests).
- [x] A result landing while composing leaves the frozen diff and draft untouched, however early its refresh began (integration test).
- [x] Turn edges observed off-thread behave exactly as today: baseline captured, promoted, persisted (existing `last-turn` suite green).
- [x] The `⟳` glyph paints in its reserved tab-strip cell only when a refresh exceeds the indicator delay, on every tab. The PR `· refreshing…` note is gone (render tests).
- [x] A scope switch shows the new scope's changed set in the switch frame, `last-turn` included (existing tests).
- [x] `reload_pending` and `service_reload` are deleted.
- [x] `just ci` green. A/B bench against main recorded in `scripts/bench-results/`.

## Out of Scope

- Async per-file diff builds. The paint path, per `specs/tui.md`.
- Event-driven refresh. Polling is a `specs/herdr-host.md` non-goal.
- Per-scope changeset stashes. Rejected in brainstorming.

## Execution Plan

1. [x] Commit the approved spec Draft.
2. [x] Extract into `src/world.rs`: a pure build (`WorldInput` → `WorldSnapshot`: changed set, entries) and `reconcile(app, snapshot)` owning every place-state touch — anchor match, fallback, clamp, composing skip. Called synchronously for now. Behavior, suite, and bench unchanged.
3. [x] Worker thread in `src/world.rs`: request/response channels, `WorldInput` tag (repo, tab, scope, base, turn baseline, config epoch, generation), latest-wins. Completions wired into `event_loop` beside `pr_rx`.
4. [x] `TurnTracker` moves into the worker: poll-flagged requests sample status, snapshot + promote + ref write happen worker-side, transitions ride the result. The UI mirrors the baseline sha and sets `pr_pending` on turn end.
5. [x] Keep the sync exceptions: a scope switch rebuilds the changed set inline (`last-turn` via the mirrored baseline), the first-visit gate (`tab_visited`) stays.
6. [x] Delete `reload_pending`, `service_reload`, and the twin-reload block in `event_loop`.
7. [x] `src/ui.rs`: the reserved tab-strip cell with the 150ms-debounced `⟳`, on all tabs. Remove the PR note.
8. [x] Integration tests in `tests/`: tag discard, supersede, mid-compose landing, off-thread turn edge.
9. [x] Bench A/B against a freshly built main binary under the same load. Record JSON baselines.

## Likely Files

| file                    | change                                                  |
| ----------------------- | ------------------------------------------------------- |
| `src/world.rs`          | new: build, reconcile, worker, input tag                 |
| `src/app.rs`            | `reload` split apart, scaffolding removed                |
| `src/lib.rs`            | worker wiring, twin-reload block deleted                 |
| `src/turn.rs`           | moves behind the worker                                  |
| `src/ui.rs`             | glyph cell, PR note removed                              |
| `tests/`                | tag, compose, supersede, turn-edge tests                 |
| `scripts/bench-results/`| new A/B baselines                                        |
| `specs/`                | already Draft, promote at the gate                       |

## Verification

- `just ci` → green.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` → chained medians at frame time, A/B interleaved with the main-built binary.
- Tight: everything the diff adds is exercised by a DoD line.
- Gate: high-effort code review of the branch, then promote `specs/tui.md` and `specs/config.md` to Current.

## Replan

- If step 2 cannot cleanly separate the open-diff rebuild, keep that rebuild inside `reconcile` on the paint path and shrink `WorldSnapshot` to the list and changeset.
- If the bench residue survives step 6, the diff build dominates — reopen the async-diff fork in brainstorming.
- 2026-07-18: initial plan.
- 2026-07-18: indicator reopened in brainstorming → the ⟳ glyph stands, spinner rejected as flicker on subsecond refreshes, landed in `specs/tui.md`.
- 2026-07-18: landings waited on the 100ms in-flight wake → world wake tightened to 15ms, painted medians back at main's sync-reload level. The superseded run1 baseline was deleted.
- 2026-07-18: garfield — the `All files` switch frame showed the old scope's badges under the new header → the tree re-marks in place before the frame (specs/tui.md); `toggled_dirs` left out of the `Changes` tag; `TurnReport` slimmed to `ended`; the indicator and wake decisions extracted pure and unit-tested; the worker's coalescing got a real-channel test.
- 2026-07-18: QA — the indicator delay rose from 150ms to 300ms and its color quieted to the header's secondary gray, so routine subsecond refreshes never blink it (`specs/tui.md`).
- 2026-07-18: distill — `sample` renamed `sample_turn` end to end and the stale module doc fixed; the PR debounce-latch collapse is deferred as its own pass, the scope-conditional base tag declined as not worth the exception.
- 2026-07-18: the glyph gained the conventional two knobs: `r` lights it immediately, an ambient refresh appears at 200ms, and once lit it holds 300ms (`specs/tui.md`).
