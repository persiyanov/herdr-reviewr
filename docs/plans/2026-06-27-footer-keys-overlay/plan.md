# Context-Aware Footer — Delivery Plan

**Specs:** ../../../specs/tui.md — the living reference this plan delivers

## Milestone Map

1. **Context-aware footer** — single milestone; the footer becomes a live action bar that shows the actions available for the current context, the primary highlighted, the rest dropped to fit one line.

## Goal

The footer teaches by showing: only the actions that work for what the cursor is on, most-relevant first, on every tab — no static legend, no overlay to memorize.

## Definition of Done

- The footer shows context-correct actions: a diff line offers `c comment · v select`; a live selection `c comment · esc clear`; a commented line `e edit · d delete · n/N jump`; a fold `→ expand fold`; a file `⇥ diff · u/b/t scope`; a directory `→ expand` / `← collapse`.
- The primary action is visually highlighted; a dim stable `⇥ pane · 1·2·3 · q` cluster sits at the right; both fit one line, dropping least-relevant with a trailing `…`.
- `s send N` (count folded in) appears whenever a comment is written, wherever the cursor is.
- The PR tab footer leads with the PR state summary, then `o open ↗`, then orientation.
- Approach A is gone: no `Mode::Help`, no keys overlay, no `?` binding.
- `cargo test` green, including pure `footer_actions` context tests and footer render tests.

## Exit State

Closed list — anything not named here is not built.

- `FooterAction` enum + `Tier` enum (`Primary` / `Normal` / `Orient`) in `app.rs`.
- `App::footer_actions(&self) -> Vec<(FooterAction, Tier)>` in `app.rs` — the pure context→actions mapping, the testable brain.
- `render_footer(frame, app, area)` in `ui.rs` — maps actions to key+label, styles by tier, fits one line (orientation dropped first, then trailing `Normal` actions; `…` when clipped); prepends the PR state summary on the PR tab; shows the transient status among the actions.
- Action→(key,label) and tier→style mapping helpers in `ui.rs`.
- `tab_bar_spans` and `Scope::cycle` — unchanged, retained.
- The footer band is one fixed row (`vrows` reserves `Length(1)`); `footer_height` removed.

**Removed** (Approach A, now superseded): `Mode::Help`, `App::open_help`/`close_help`, `render_keys_overlay`, the `?` bindings + Help key-dispatch branch + `Mode::Help` mouse guard, the `Mode::Help` render dispatch, the `?` keymap-table row, and the static `footer_content` hint strings.

## Specs Touched

| Spec | What this plan realizes | At the gate |
| --- | --- | --- |
| `tui.md` | the **Footer** section (the rest already ships) | Draft → Current |

## Out of Scope

- Colorblind diff-gutter cue and the silent-no-op status nudges — separate review items.
- A `?` overflow overlay behind the `…` — explicitly rejected; `…` is a passive marker only.

## Likely Files

- `src/app.rs` — `FooterAction`/`Tier`, `footer_actions`; remove `Mode::Help` + `open_help`/`close_help`
- `src/ui.rs` — `render_footer` rewrite + mapping helpers; remove `render_keys_overlay`, `footer_content`, `footer_height`; `vrows` footer → `Length(1)`
- `src/lib.rs` — remove `?` bindings, Help dispatch, `Mode::Help` mouse guard
- `tests/app_flow.rs` — `footer_actions` context tests; remove the Help-mode tests
- `tests/render.rs` — footer action-bar render tests; remove the keys-overlay test

## Execution Plan

1. Remove Approach A from `app.rs` (`Mode::Help`, `open_help`/`close_help`, the `pending_location` arm) and `lib.rs` (`?` bindings, Help dispatch, mouse guard).
2. Add `FooterAction` + `Tier` and `footer_actions` to `app.rs`.
3. Rewrite `render_footer` in `ui.rs` (mapping helpers, tier styling, one-line fit + `…`, PR state prefix); remove `render_keys_overlay`, `footer_content`, `footer_height`; set `vrows` footer to one row; drop the `Mode::Help` render dispatch.
4. Replace the keys-overlay/footer tests with `footer_actions` context tests and footer render tests.

## Verification

- **Done:** `cargo test`. `tests/app_flow.rs` asserts `footer_actions` returns the right primary per context (content→`Comment`, fold→`ExpandFold`, selection→`Comment`+`ClearSelection`, commented line→`EditComment`, file→`TogglePane`, dir→`Expand`/`Collapse`) and that `Send` appears iff the store is non-empty; `tests/render.rs` asserts the bar paints the primary, the orientation cluster, and a status among the actions.
- **Tight:** the diff equals Exit State — no `Mode::Help`/overlay/`?` residue; `footer_content` and `footer_height` deleted, not left beside `render_footer`.
- **Invariants upheld:**
  - The footer never lists an action that wouldn't work in the state — covered by the per-context `footer_actions` tests (`specs/tui.md` Footer).
  - A comment is never lost to a refresh — untouched.

## Replan Triggers

- If the fit/`…` logic can't keep the primary action visible at a realistic minimum sidebar width, revisit the drop order (not the model) and update `specs/tui.md`.

## Replan Log

- 2026-06-27: initial plan (lean footer + `?` overlay).
- 2026-06-27: replanned to the context-aware action bar; Approach A (overlay + lean hints) superseded and removed.
