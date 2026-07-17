# Four-way navigator — Plan

Delivers `specs/tui.md#overview`, `specs/tui.md#interaction`, `specs/tui.md#pr-tab`, and `specs/config.md` (issue #16).

## Goal

Users place and resize the navigator without losing review context. One layout model drives painting, hit-testing, scrolling, and input across every tab.

## Definition of Done

- [x] The config and keybinding surfaces match `specs/config.md`, including normalized output, canonical action names, legacy aliases, and collision diagnostics.
- [x] Every tab paints and hit-tests the four positions and sizing rules from `specs/tui.md#overview`, including tiny terminals.
- [x] Keyboard and divider controls match `specs/tui.md#interaction` in main panes, the comment editor, and the comments list.
- [x] Every layout change preserves or clamps review state as required by `specs/tui.md#overview`.
- [x] PR focus, paging, wheel scrolling, selection reveal, and refetch preservation match `specs/tui.md#pr-tab`.
- [x] Live config changes and recovery use the frame boundary and precedence rules in `specs/config.md`.

## Out of Scope

- Per-tab placement, automatic placement, and content-sized navigation. These remain `specs/tui.md` non-goals.
- A hidden navigator and a multi-file review stream. These remain `specs/tui.md` non-goals.
- Replying to or closing issue #16. The merge workflow owns that.

## Execution Plan

1. [x] `src/config.rs`, `src/app.rs`, and `src/ui.rs`: introduce the four-value navigator position, separate side and stacked shares, and one pane allocator used by rendering and every geometry helper. Add `tests/render.rs` coverage for all positions, both size ranges, divider placement, row hits, pane routing, and axes below six cells.
2. [x] `src/keymap.rs` and `src/config.rs`: add `navigator-position`, `navigator-grow`, and `navigator-shrink`; accept the two legacy resize aliases; reject canonical-plus-alias duplicates before keymap resolution; include the position and canonical bindings in normalized JSON. Extend the config and keymap unit tests for defaults, invalid positions, aliases, duplicate actions, and the new-default `p` collision.
3. [x] `src/app.rs` and `src/lib.rs`: dispatch the three actions in every main pane, preserve modal behavior, and replace the resize boolean with a gesture state that can cancel until mouse-up. Make dragging axis-aware, cancel it on keypress, resize, and config layout changes, and retain both shares through config recovery. Extend `tests/app_flow.rs` for cycling, independent shares, bounds, modes, cancellation, and recovery.
4. [x] `src/lib.rs`: apply sidebar config snapshots before drawing rather than between a painted frame and its input. Preserve a session hotkey override across unchanged reads, apply changed configured positions, and reapply the configured position after recovery. Add refresh tests that exercise unchanged reads, unrelated edits, changed positions, invalid recovery, and painted-frame dispatch.
5. [x] `src/app.rs`, `src/ui.rs`, and `src/lib.rs`: give the PR navigator an independent bounded scroll, reveal selections without making checks selectable, route page keys by focus, and route wheel input by pane. Preserve both PR scroll positions through refetch and expose the position action in the normal footer on every tab. Add focused app-flow and render tests.
6. [x] Run the full verification matrix, delete unexercised surface, and reconcile the implementation against both Draft specs. At the merge gate, run the full-branch review and promote `specs/tui.md` and `specs/config.md` to Current.

## Likely Files

| file                    | change                                                        |
| ----------------------- | ------------------------------------------------------------- |
| `src/config.rs`         | position value, parsing, runtime precedence, normalized output |
| `src/keymap.rs`         | canonical navigator actions, defaults, legacy aliases         |
| `src/app.rs`            | runtime layout state, shares, drag state, PR navigator scroll |
| `src/ui.rs`             | four-way allocation, rendering, hit-testing, footer hint      |
| `src/lib.rs`            | frame-boundary config, action dispatch, mouse and resize input |
| `tests/app_flow.rs`     | interaction, preservation, PR scrolling, recovery             |
| `tests/render.rs`       | layout matrix, tiny terminals, hit-testing, footer             |
| `specs/tui.md`          | promote to Current at the merge gate                           |
| `specs/config.md`       | promote to Current at the merge gate                           |

## Verification

- `cargo test --all-features` → all unit and integration tests pass.
- `just ci` → formatting, Clippy with warnings denied, and the full test suite pass.
- `navigator_layout_rects_cover_every_position` → painting and hit rectangles agree across four positions and tiny axes.
- `navigator_controls_preserve_state_and_cancel_drags` → focus, selection, scroll, shares, and gesture cancellation match the TUI spec.
- `navigator_config_precedence_and_aliases` → startup, reread, changed position, recovery, canonical output, aliases, and collisions match the config spec.
- `pr_navigator_scroll_is_independent_and_preserved` → focused paging, pane-local wheel input, selection reveal, and refetch retention match the PR contract.
- Live: cycle `p` through all positions on each tab, resize by key and drag, edit `navigator_position`, recover from an invalid config, and scroll an overflowing stacked PR navigator.
- Tight: everything the diff adds is exercised by a Definition of Done line. Delete or defer the rest.
- Gate: run an xhigh full-branch code review, fix or assign every finding, then promote both Draft specs to Current.

## Replan

- If Ratatui's percentage allocator cannot satisfy the tiny-axis rule, replace only the shared pane allocator with exact rectangle arithmetic.
- If frame-boundary config application needs explicit captured state, add one frame context shared by render and input instead of duplicating geometry.
- 2026-07-14: initial plan.
- 2026-07-14: implementation, Garfield, xhigh review, and verification complete; specs promoted to Current.
