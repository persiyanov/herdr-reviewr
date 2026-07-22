# Find in file — Plan

Delivers `specs/find-in-file.md` (Draft), with the `input.md`, `config.md`, and `overview.md` weaves.

## Problem

Global search lands the reviewer on a file, but there is no way to search within it. To find
every use of a symbol in the open file, the reviewer scrolls and eyeballs. The read pane shows
the file but marks nothing, so a constant used twenty lines apart is two manual hunts.

## Goal

`ctrl+f` opens a find band over the read pane: a literal query lights every match, and `enter`
and the arrows step the cursor between them. `find` is a rebindable action, so the keymap gains
`ctrl+`/`alt+` chord bindings.

## Definition of Done

- [ ] `ctrl+f` opens the find band over a `Changes` diff and an `All files` file view, from
      either pane, and focuses the read pane. The footer shows the band's keys.
- [ ] Typing lights every matching row. The count reads `k / total` on a match, the total off
      a match, `no matches` when nothing matches, and blank while the query is empty.
- [ ] Matches inside collapsed folds count toward the total, and light once a step reveals them.
- [ ] `enter`/`↓` step to the nearest matching row below the cursor, `↑` above, both wrapping.
      The landing reveals the row and expands a fold around it.
- [ ] Landing from global search on a symbol, then `ctrl+f` on it, reads the cursor's ordinal
      at once and steps through the other uses.
- [ ] `esc` closes the band, clears the highlight, keeps focus and the cursor where the last
      step left it. Reopening starts empty.
- [ ] `ctrl+f` is inert with no searchable rows — a notice, an empty file, a markdown preview,
      the `PR` tab, no open file — and while composing a comment or in the comments list.
- [ ] A refresh re-scans the open file. The current match follows the reconciled cursor. The
      band force-closes when the open file degrades to a notice or reconciles to a different
      file.
- [ ] `find` rebinds through `[keybindings]` to a `ctrl+`/`alt+` chord or a bare character.
      An invalid chord fails `CFG-KEY-FORM`, a collision `CFG-KEY-UNIQUE`. `--resolve-plugin-config`
      prints the chord.
- [ ] `scripts/bench_tui.py` medians match the pre-change baseline within noise.

## Out of Scope

- Regex, glob, fuzzy, cross-file search, replace, a column cursor, commenting from the band,
  and any remembered query. All in `find-in-file.md` Non-goals.
- Modifier bindings past `ctrl+`/`alt+` chords: no `shift`, `cmd`, or key sequences. `input.md`
  Non-goals — a later Kitty-protocol step.

## Execution Plan

The keymap widening is the one cross-cutting change, so it lands and proves out first.

1. [ ] `src/keymap.rs`, `src/config.rs`, `src/lib.rs`, `src/ui.rs`: widen the keymap key from
       `char` to a `Key` (a char with `ctrl`/`alt` flags). Add `Action::Find`, default `ctrl+f`.
       `parse_keybindings` accepts a bare character or a `ctrl+`/`alt+` chord (`CFG-KEY-FORM`);
       `action_for` and the dispatch key on `Key`; `hint` renders `⌃f`/`⌥x`; `--resolve-plugin-config`
       serializes chords. A modifier-less `Key` is today's bare character, so existing bindings
       are unchanged; the widening migrates the current `action_for` call sites and keymap tests.
       Tests: chord parse and resolve, `find` default and rebind, `CFG-KEY-FORM` reject,
       `CFG-KEY-UNIQUE` collision across chords and chars, every prior binding intact.
2. [ ] `src/app.rs`: add `Mode::Find` and a `Find { query, caret }` state. The current match is
       `diff_cursor` when its row matches, so nothing else is stored. The match set walks the full
       `diff.rows` tree, descending into each `Row::Fold`'s `lines`, so folded content is searched
       (`find-in-file.md`). The count is the cursor's ordinal over that set in file order.
       `find_step` finds the nearest match below or above the cursor's line, wrapping; a match in a
       collapsed fold expands it (`expanded_folds`, `rebuild_visible`) before `diff_cursor` lands,
       then `reveal_diff`. The literal smart-case match test is one helper, shared with the render
       highlight. Tests: the scan and spans, a row with several occurrences counted once, a folded
       match found and counted, the four count states, step nearest and wrap, a step onto a folded
       match expands it, step inert with no match.
3. [ ] `src/lib.rs`: `Action::Find` opens the band when the read pane has content rows, inert
       otherwise and while composing or in the comments list. Find-mode keys: `apply_text_edit`
       drives the query, `enter`/`↓`/`↑` step, `esc` closes; every other key is inert. Open
       focuses the read pane; close keeps it. Tests: open and inert contexts, type-step-esc,
       `esc` keeps the cursor, reopen empty.
4. [ ] `src/ui.rs`: render the one-row find band at the read pane's foot (label, query with
       caret, count), the read pane losing that row. Overlay the match highlight on each visible
       row's matched spans, from the step-2 helper, via `emphasized_spans`. Footer: the find-mode
       bar, and `⌃f find` in the review contexts. Tests in `tests/render.rs`: band, highlighted
       rows, current match banded on the cursor, count text, footer.
5. [ ] `src/app.rs` (`reconcile_world`/`reload`): a refresh re-scans, so the highlight and count
       re-derive from the reconciled cursor and query. Force-close the band when the open file
       loses its searchable rows or changes identity. Tests: a poll under find keeps the query
       and reconciles the cursor, a degrade-to-notice and a reconcile-to-different-file each
       force-close.
6. [ ] Bench: rebuild the pre-change binary to a second target dir, interleave
       `scripts/bench_tui.py` runs, compare medians A/B. The highlight scans the visible rows each
       frame; the match set scans the full open file on a query edit.

## Likely Files

| file                | change                                                                     |
| ------------------- | -------------------------------------------------------------------------- |
| `src/keymap.rs`     | `Key` (char + `ctrl`/`alt`), `Action::Find`, chord resolve, `hint` glyph   |
| `src/config.rs`     | `parse_keybindings` chord grammar (`CFG-KEY-FORM`), resolved-config chords |
| `src/app.rs`        | `Mode::Find`, `Find` state, match scan, step, count, forced-close          |
| `src/lib.rs`        | dispatch passes modifiers, find open gate + find-mode keys                 |
| `src/ui.rs`         | find band, per-row match highlight, footer, chord `hint`                   |
| `tests/app_flow.rs` | open/inert contexts, type/step/wrap, cursor-on-match-open, continuity      |
| `tests/render.rs`   | band, highlights, current-match band, count, footer                        |
| `specs/*`           | promote find-in-file/input/overview/config to Current at the gate          |

## Verification

- `just ci` → clean.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` A/B → medians
  within noise of the pre-change baseline.
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- Continuity (`overview.md` O6) → the poll-under-find test: the query holds, the cursor
  reconciles, an identity change force-closes.
- `CFG-KEY-FORM` / `CFG-KEY-UNIQUE` → the keymap chord tests.
- Gate: the merge-gate review loop, then `find-in-file.md`, `input.md`, `overview.md`, and
  `config.md` verified against the code and promoted to Current.

## Replan

- If a scan costs a visible frame on a large open file, then cache the match set by content hash
  like `DiffCache` and note it in `find-in-file.md`.
- If crossterm does not deliver `ctrl+f` in a herdr pane, then revisit the default key in
  brainstorming. The action stays rebindable regardless.
- 2026-07-21: initial plan.
