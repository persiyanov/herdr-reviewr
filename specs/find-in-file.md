---
Status: Current
Created: 2026-07-20
Last edited: 2026-07-21
---

# Find in file

Literal search within the open file in the read pane, opened with `ctrl+f`: every match highlighted, `enter` and the arrows stepping the cursor between them.

## Overview

`ctrl+f` opens a find band at the foot of the read pane. The reviewer types a literal query, every matching row lights up, and `enter` or the arrows step the cursor from match to match. `esc` closes the band and leaves the cursor where it stepped.

```
 src/llm_registry.py
  39     def resolve(self, name):
  40 ▌   REGISTRY = load_registry()          ← the cursor, on match 2 of 5
  41       return REGISTRY[name]             ← another match, lit
  ⋯   12 unmodified lines
 find  REGISTRY█                                            2/5
```

## The band

- The `find` action opens the band over the `Changes` diff and the `All files` file view, from either pane. Its default key is `ctrl+f`, rebindable (`input.md`).
- Opening the band focuses the read pane and clears a live selection (`diff-view.md`).
- The band is one row at the foot of the read pane: the label `find`, the query with its caret, and the match count at the right. The read pane loses that row while the band is open.
- The query is the search input's field: the comment editor's controls, newlines excluded, a paste's newlines flattened to spaces (`input.md`, `search.md`).
- An empty query shows a dim placeholder, lights nothing, and leaves the count blank.
- While the band is open it owns the keymap. Every printable key is query text, so the review keys (`n`, `u`, `1`, …) lose their review action. Only the steps and `esc` act. Every other key is inert.
- The find key is inert while composing a comment and in the comments list (`input.md`).

## Matching

- The query matches literally, smart-case: an all-lowercase query ignores case, any uppercase makes it case-sensitive.
- The match unit is a row. A row with several occurrences lights each one and counts once.
- Every occurrence reverses to a bright fill with dark text (`theme.md`), so a match reads over any row tint, red or green.
- Every row is searched, the runs hidden inside folds included.
- Rows keep their syntax color under the highlight.
- The current match is the read-pane cursor's row while that row matches the query, marked by the cursor band. Off a match, no row is current.
- The count reads `k / total`: the cursor's ordinal among the matching rows over their number. It shows the total alone when the cursor is off a match, and `no matches` when nothing matches.

## Stepping

- `enter` and `↓` move the cursor to the nearest matching row below it, `↑` to the nearest above. Both wrap at the file's ends.
- The move reveals the row: the cursor and scroll bring it into view, expanding a fold around it (`diff-view.md`).
- Stepping is inert while nothing matches.
- Typing re-lights the matches and never moves the cursor. Only a step moves it.

## Closing

- `esc` closes the band and clears the highlight. Focus stays on the read pane, the cursor where the last step left it, or unmoved if no step happened.
- Closing drops the query, so reopening starts empty.

## Where it works

| the read pane shows                   | the find key            |
| ------------------------------------- | ----------------------- |
| a diff or file view with content rows | opens the band          |
| a rendered markdown preview           | inert, it has no cursor |
| a notice, or an empty file            | inert, nothing to find  |
| no open file, or an empty changeset   | inert                   |

The `PR` tab has no read-pane file, so it has no find.

## Continuity

While the band is open, polls keep running underneath (`overview.md`).

- A refresh re-scans the open file and re-lights the matches. The highlight is derived: stale, never wrong.
- The cursor reconciles by identity, then nearest, then clamp (`overview.md`), and the current match follows it: it stays current while its reconciled row still matches, and lapses when it does not.
- The band closes when the open file loses its searchable rows or changes identity: a degrade to a notice, or a reconcile to a different file. This is a forced return, like the markdown preview (`diff-view.md`).
- Apart from that forced return, a poll leaves the band and the query untouched: it never types into the query or moves the caret.

## Keys

| key           | does                       |
| ------------- | -------------------------- |
| printable     | edit the query             |
| `enter` / `↓` | step to the next match     |
| `↑`           | step to the previous match |
| `esc`         | close, the cursor stays    |

The band shows its own one-row footer, `↑↓ match` and `esc`, the arrows combined like the search screen's pick (`search.md`). In the review, `ctrl+f find` sits in the footer's `go` band wherever the band would open (`input.md`).

## Non-goals

- No regex, glob, or fuzzy matching. The query is literal, unlike the search screen (`search.md`).
- No cross-file search. That is the search screen (`search.md`).
- No replace. The viewer is read-only (`diff-view.md`).
- No column cursor. The match unit is the row.
- No commenting from the band. Close it, then comment on the landed line.
- No find history, and no query remembered across opens.

## Related specs

- [diff-view](./diff-view.md)
- [input](./input.md)
- [search](./search.md)
- [theme](./theme.md)
- [overview](./overview.md)
