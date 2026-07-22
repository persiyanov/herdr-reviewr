# Footer progressive disclosure — Plan

Delivers `specs/input.md` Footer, with the `overview.md`, `tui.md`, `find-in-file.md`, and
`pr-tab.md` weaves.

Builds on the uncommitted find-in-file branch (`worktree/clear-valley-b57a`). That feature is
complete and fully gated. Commit it as its own PR before this milestone, so the footer branches
off a clean base — otherwise the two features share one PR.

## Problem

The footer is one line that fills by priority and silently drops keys on overflow. As the keymap
grew (search, find), the bright zone crowds and mixes cursor actions with always-available verbs,
so the one most-useful action for the thing under the cursor is buried. On a narrow herdr sidebar
pane, applicable keys vanish with no way to see them. The `↵ next` glyph disappearing in the find
footer was the first symptom the reviewer hit.

## Goal

The footer shows one row — the primary + cursor actions + `send` + a `?`. Pressing `?` expands it
to every applicable shortcut, in labeled `do`/`go`/`move` bands, and stays open until `?` or `esc`.

## Definition of Done

- [x] The footer shows one row by default: the primary, the cursor's actions, `send` once a
      comment exists, and a `?` at the right. The muted-right orientation cluster is gone.
- [x] `?` expands the footer to labeled bands `do` / `go` / `move`, listing every applicable
      shortcut including movement keys. `?` or `esc` collapses it.
- [x] The expansion is sticky: it opens collapsed, is never saved, and survives cursor moves, tab
      and scope switches, file opens, a poll, and config recovery.
- [x] The expansion caps at the read pane's minimum (`tui.md`); a context needing more shows only
      what fits, and row 1 always survives.
- [x] Row 1 trims trailing actions to fit; the primary, `send`, and `?` never drop, and the
      primary truncates before `?` on a pane too narrow for both.
- [x] The `go` band never repeats a row-1 key; a key the tab lacks never appears (no hunk or file
      steps on `PR`).
- [x] `esc` steps through one layer per press: a live selection, then an armed crossing, then the
      expansion.
- [x] `find = ["ctrl+f"]`-style rebinding still holds, and `keys` rebinds through `[keybindings]`
      (default `?`). `?` is inert in the comments list and text in the editors and search/find
      inputs.
- [ ] `scripts/bench_tui.py` medians match the pre-change baseline within noise.

## Out of Scope

- A full static keymap cheatsheet (every action regardless of context). The `?` expansion is
  context-scoped by design (`input.md`).
- Persisting the expansion across app restarts. It is session place state (`overview.md`).

## Execution Plan

The model and toggle land first; the variable-height render — the riskiest, per-frame part — after.

1. [x] `src/keymap.rs`: add `Action::Keys`, default `?`. Tests: `?` resolves to `Keys`, and it
       rebinds.
2. [x] `src/app.rs`: add `keys_expanded: bool` (place state, global — not tab-stashed), a
       `toggle_keys`, and preserve it through `reconcile_world` and config recovery. Rewrite
       `footer_actions` into `footer_bands` — a row-1 set (primary, send, the cursor's actions)
       plus the three bands (`do`, `go`, `move`), replacing the `Tier` split. Add the movement
       `FooterAction`s (down/up, hunk, file, page), the global de-dup (a band never repeats a
       row-1 key), and the per-tab applicability filter. Tests: band membership per context,
       de-dup on the armed crossing and the empty-scope primary, `PR` drops hunk/file, an empty
       band vanishes, a poll and recovery keep the toggle.
3. [x] `src/lib.rs`: dispatch `Keys` to `toggle_keys` in `Normal` mode; wire the `esc` ladder
       (selection → armed crossing → expansion, one step per press). `?` stays text in the editors
       and the search and find inputs, inert in the comments list. Tests: `?` toggles from Normal
       and is inert in the list; the `esc` ladder consumes one layer per press.
4. [x] `src/ui.rs`: make the footer height variable in the body split (`vrows`/`body_rect` now take
       `app`) — one row collapsed, one plus the wrapped bands when expanded, capped so the body
       keeps its `Min(3)`. Rewrite `render_footer` (row 1 + wrapped bands), with `footer_lines` the
       one builder shared by the layout height and the paint. Add `action_key_label` entries for
       the movement pairs and the band labels. Tests in `tests/render.rs`: the expanded bands with
       labels, the cap on a short pane, the trim on a narrow pane.
5. [x] Bench: rebuild the pre-change binary (`bbeaac7`) to a second target dir, compare medians
       A/B. The footer-isolating `file_next_all_files` metric (trivial diff, footer repainted every
       keypress) is flat at 0.5ms on both, so the per-frame band build costs ~0. The noisier
       `file_next_changes` overlaps within its syntect-highlight variance.

## Likely Files

| file                | change                                                                                                                                  |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `src/keymap.rs`     | `Action::Keys`, default `?`                                                                                                             |
| `src/app.rs`        | `keys_expanded` place state, `footer_bands` (bands replace `Tier`), movement actions, esc ladder, de-dup, poll/recovery keep the toggle |
| `src/lib.rs`        | dispatch `?`→toggle (Normal only), esc ladder                                                                                           |
| `src/ui.rs`         | variable footer height + read-pane clamp, `render_footer` rewrite (row 1 + labeled bands), labels for movement/`?`                      |
| `tests/app_flow.rs` | band membership, de-dup, PR filter, esc ladder, poll/recovery keep the toggle                                                           |
| `tests/render.rs`   | collapsed row 1, expanded bands + labels, the cap, the trim                                                                             |
| `specs/*`           | promote input/overview/tui/find-in-file/pr-tab to Current at the gate                                                                   |

## Verification

- `just ci` → clean.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` A/B → medians
  within noise of the pre-change baseline.
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- Continuity (`overview.md` O6) → the poll-and-recovery test: the toggle holds, content re-derives.
- Gate: the merge-gate review loop, then promote `input.md`, `overview.md`, `tui.md`,
  `find-in-file.md`, `pr-tab.md` to Current.

## Replan

- If the per-frame band build regresses the bench, then cache the bands by context signature and
  rebuild only on a place change, and note it in `input.md`.
- If the variable-height footer fights the divider-drag or the composer's own row-steal, then
  reconcile the body split in one place and note the ordering in `tui.md`.
- 2026-07-21: initial plan.
- 2026-07-21: build corrected the Draft's `go`/`move` contents → the `go` band excludes the
  navigator-resize keys (drag-first) and the `move` band excludes horizontal scroll (a `←`/`→`
  overload); the pane toggle joins `go`, and the `tabs`/`quit` entries gained labels. Landed in
  `input.md` (band list, mock, a Non-goals line) and `pr-tab.md`.
- 2026-07-21: QA feedback → the expanded footer's row 1 ignored the label gutter, so its content
  and the `go`/`move` keys sat on two left edges. Fixed: when the panel is open, row 1 joins the
  grid under a dim `do` label and its overflow continues under a blank gutter; collapsed it stays
  the flush action bar. Landed in `ui.rs` (`footer_row1` gutter, `footer_lines` overflow) and
  `input.md` (grid mock + prose).
- 2026-07-21: focused review of the grid delta caught a regression → the fixed 7-col `do` gutter
  cannot shed like the primary, so on an expanded pane narrower than the gutter plus `send` plus the
  `?`, both `send` and the `?` clipped. Fixed: the grid engages only when the gutter leaves room for
  the primary's key, `send`, and the `?`; below that row 1 stays flush (which never drops them).
  Pinned by `the_expanded_row_one_never_drops_send_or_the_more_hint_on_a_narrow_pane`.
- 2026-07-21: merge-gate review (code-review + garfield + verification advisor) fixed four real
  defects → the PR-tab `esc` cleared the frozen file-tab selection; `footer_row1` did not reserve
  `send`'s width so `send` and `?` clipped on a narrow pane; the `move` band listed the inert `hunk`
  step on All files and in a preview; modal footers lost the old `…` clip marker. All fixed with
  tests. `move`-band hunk applicability now documented in `input.md`; CHANGELOG/README gained the
  footer-expansion and `keys` entries. Deferred: `footer_lines` rebuilds per `panes()` call while
  expanded (not on the collapsed bench path; the caching Replan above stands if a future expanded
  bench regresses).
