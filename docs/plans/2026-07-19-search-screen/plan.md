# Full-screen search — Plan

Delivers `specs/search.md` (Draft), with the `input.md` and `overview.md` weaves.

## Problem

The v1 search overlay is a small popup with one interleaved list. Code hits sit after all
file hits, so reaching them takes a long pick sweep. A code hit shows one clipped line with
no context and no syntax color, so the reviewer cannot judge which hit they need. The popup
has no room to fix either.

## Goal

`/` from any tab opens a full-screen search: Files and Code modes flipped by `tab`, grouped
code results, and a syntax-highlighted preview centered on the picked hit.

## Definition of Done

- [ ] `/` opens the search screen from every tab and pane. `esc` restores the exact place.
- [ ] The input band shows the query and both mode chips with live counts, inactive dimmed.
- [ ] Results sit full-width above a full-width preview, 50/50, divider-draggable into a
      search-owned session share.
- [ ] `tab` flips the mode, keeps the query, paints the held results in the same frame, and
      lands the pick on the first result row.
- [ ] `Files` mode lists path matches in the file-list row look. An empty query lists the
      engine's frecency-ranked files.
- [ ] `Code` mode groups match rows under file header rows, `line:` dimmed, `def` badge on
      engine-classified definitions, `… more` when clipped.
- [ ] The preview renders the picked file as the syntax-highlighted File view. A `Code`
      pick centers, bands, and emphasizes the hit line. `PageUp`/`PageDown` scroll it.
- [ ] A pick sweep never waits on a preview build. The preview renders when the pick
      settles.
- [ ] A poll that changes the previewed file repaints the preview in place. A deleted
      previewed file previews empty.
- [ ] `enter` lands in `All files` on the file, cursor on the hit line, focus on the read
      pane. The origin tab keeps its place.
- [ ] The footer shows the screen's keys, only flip and `esc` when nothing is pickable.
- [ ] `scripts/bench_tui.py` medians match the pre-change baseline within noise.

## Out of Scope

- Changeset-scoped search and in-diff search. Roadmap (`specs/overview.md`).
- Worker or engine changes beyond what the screen consumes. The v1 worker already returns
  both groups per query.

## Execution Plan

1. [ ] `src/app.rs`: add `SearchMode { Files, Code }` and per-mode pick to `SearchOverlay`.
       Open from any tab in `open_search`, remove the All-files gate. `tab` flip resets the
       pick to the first result row. Tests: flip keeps query and paints held results, `/`
       opens from `Changes` and `Pr`, `esc` restores each origin tab's place.
2. [ ] `src/ui.rs`: replace `render_search`'s popup with the body-filling screen — band
       row (query, chips, counts), results pane, preview pane, search share. Rework
       `search_rows` to per-mode rows: file rows via `file_row_item`, `Code` file headers
       plus `line:` match rows with the `def` badge. Rework `search_hit` for the new
       geometry, chip clicks, and result clicks. Tests in `tests/render.rs`: chip counts
       and dimming, grouped code rows, `def` badge, `… more`, tiny-size band.
3. [ ] `src/app.rs` + `src/ui.rs`: the preview — build the picked file's File view into
       overlay state, render it in the preview pane with the hit line centered, banded,
       and match-emphasized, title naming the file. `PageUp`/`PageDown` scroll it.
       Settle rule: a pick move only marks the preview stale, and the build runs on the
       next frame with no pending input. Tests: centering, scroll, sweep leaves no
       intermediate builds, binary-file notice.
4. [ ] `src/app.rs` (`reconcile_world`): a landed world result rebuilds a stale preview in
       place, hit line clamped, and empties it for a deleted file. Test: poll rewrite of
       the previewed file repaints without moving the pick.
5. [ ] `src/app.rs` (`search_open_pick`): land in `All files` from any origin tab —
       switch tab, select the file, cursor on the clamped hit line, focus the read pane,
       origin tab place preserved. Tests: open from `Changes`, `1` returns to the kept
       place.
6. [ ] `src/lib.rs` + `src/ui.rs`: divider drag inside search adjusts the search share,
       bounded by `tui.md` minimum sizes, review shares untouched. Footer arm updated to
       the screen's keys. Tests: drag changes only the search share, footer with and
       without pickable results.
7. [ ] `specs/input.md`: `/` row and footer bullet already updated — verify against built
       behavior at the gate.
8. [ ] Bench: rebuild the pre-change binary to a second target dir, interleave
       `scripts/bench_tui.py` runs, compare medians A/B.

## Likely Files

| file                 | change                                                      |
| -------------------- | ----------------------------------------------------------- |
| `src/app.rs`         | `SearchMode`, any-tab open, preview state, open-pick reroute |
| `src/ui.rs`          | full-screen render, per-mode rows, preview pane, hit-testing |
| `src/lib.rs`         | divider drag in search, key handling for flip and paging     |
| `tests/app_flow.rs`  | mode flip, any-tab open/close, preview settle, poll repaint  |
| `tests/render.rs`    | band, grouped rows, badges, preview centering, footer        |
| `specs/search.md`    | promote to Current at the gate                               |

## Verification

- `just ci` → clean.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` A/B → medians within noise of the pre-change baseline.
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- Gate: the merge-gate review loop, then `specs/search.md`, `input.md`, `overview.md`
  verified against the code and promoted to Current.

## Replan

- If a settled preview build of a large file still costs a visible frame, then move the
  File view build to a worker with the input-tag pattern and note it in `search.md`.
- If the per-edit dual query regresses the bench, then gate the grep behind a short edit
  debounce and take that to brainstorming first.
- 2026-07-19: initial plan.
