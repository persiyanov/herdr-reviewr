---
Status: Current
Created: 2026-07-18
Last edited: 2026-07-20
---

# Search

Full-screen file and code search over the worktree, opened with `/` from any tab.

## Overview

`/` replaces the body with the search screen: an input band, then a full-width results pane
above a full-width live preview. `tab` flips between the two search modes, keeping the query.

```
┌ 1 Changes  2 All files  3 PR  [uncommitted]                                  ┐
│ > registry resolve█                                       files 3 │ code 37+ │
│ ┌ results ────────────────────────────────────────────────────────────────┐ │
│ │ src/llm_registry.py                                                     │ │
│ │ ▌ 41: def resolve(self, name):                                          │ │
│ │   88: return registry.resolve(name, strict=True)                        │ │
│ │ src/app.py                                                              │ │
│ │   12: registry.resolve(x)                                               │ │
│ └─────────────────────────────────────────────────────────────────────────┘ │
│ ┌ preview · src/llm_registry.py ──────────────────────────────────────────┐ │
│ │ 40   def resolve(self, name):                                           │ │
│ │ 41 ▌   from .z import w                                                 │ │
│ │ 42     return registry[name]                                            │ │
│ └─────────────────────────────────────────────────────────────────────────┘ │
│ tab files · ↑↓ pick · enter open · esc close                                 │
└──────────────────────────────────────────────────────────────────────────────┘
```

The reviewer types, watches the preview, and either opens the pick or backs out with
nothing moved.

## Engine

The engine is the `fff-search` library. reviewr passes the query through and renders its
results.

- Matching, ranking, and indexing are engine-owned. reviewr never re-orders, merges, or
  interleaves results.
- The query matches file names fuzzily and code literally, narrowed by globs, `!`
  exclusions, and `git:` filters. The grammar is the engine's.
- Ranking improves with use: opening a result records the access in the engine's frecency
  store. The store lives in reviewr's cache directory, never the worktree.
- Search covers tracked and untracked files. Ignored files and `.git` are not searchable.

## The screen

- `/` opens the search screen from any tab, from either pane. `esc` returns to the tab
  and place it left.
- In the comment editor `/` is text. In the comments list it is inert.
- The header stays. The input band sits under it: the query with its caret, then the mode
  chips `files │ code`, the active one lit like the active header tab, the inactive one
  quiet. An empty query shows a dim placeholder. The footer carries the `tab` flip key, so
  the chips carry no glyph.
- Below the band, the results pane sits above the preview pane, both full width, whatever
  the navigator position. The pane is rarely wide enough to split into readable columns.
- A titled rule tops each pane: `results`, and `preview` with the file.
- The results pane takes half the body by default. Dragging the divider changes the share,
  bounded by the minimum pane sizes (`tui.md`). The share is search's own session value —
  the review layout's shares are untouched, and the position and resize keys are printable
  here. A drag cancels by `input.md`'s divider rules.
- At tiny sizes the input band keeps its one row and the panes divide the rest
  (`tui.md` minimum sizes).
- The screen opens in `Files` mode. `tab` flips the mode, keeping the query, and paints
  the other mode's held results at once.
- Both chips carry a live count, the inactive one dimmed — the hint that the other mode
  has hits. `Files` shows the engine's total. `Code` shows the fetched matches, with `+`
  while the engine holds more. While warming, the count slot is empty.
- The query edits with the comment editor's controls, newlines excluded (`input.md`). A
  paste lands whole, its newlines as spaces.
- Every edit re-queries both modes, off the frame loop. Typing never blocks.
- A result set paints only while it matches the query as typed. A superseded set is
  discarded. While a query is in flight, the previous results stay.
- A result set lands whole: the chips' counts and the rows they head come from one query.
  A landed set resets the pick to the first result row.
- Results describe the worktree when their query ran. Only an edit re-queries — a poll
  never reshapes the list or the counts under the pick.
- Before the engine's first index build completes, the results pane shows `indexing…`.
  Results appear once it is warm.
- While the screen is open, polls keep running underneath. The view behind it may go
  stale, never wrong (`overview.md` Continuity).

## Results

`Files` mode lists the engine's path matches, one row per file, in the file-list row look
(`file-list.md`). An empty query lists the engine's frecency-ranked files, so the screen
is useful before the first keystroke.

`Code` mode lists the engine's content matches, grouped by file:

- The engine returns content matches file by file. The header rows only make that
  visible — nothing is reordered.
- A file's first match emits a header row in the file-list row look, then its match rows.
- A match row is `line:` dimmed, then the matched line, its leading indentation dropped so
  the match text aligns at the left.
- A match row longer than the pane clips around its first matched span, keeping the
  emphasis visible.
- The pick lands only on match rows in `Code` mode, only on file rows in `Files` mode.
- A mode flip lands the pick on the new mode's first result row.
- An empty query lists nothing.

Both modes share the rest:

- Matched characters wear a highlight where the engine reports them. Rows carry no syntax
  color — the highlight leads in the list, the preview carries the color.
- A match hidden in a path's elided head has no cell on the row, so it is not marked.
- The list scrolls to keep the pick visible, so every result is reachable.
- A clipped list ends with a dim `… more`. The engine's full count lives in the chip.
- A layout change keeps the pick and rescrolls it visible.

## Preview

The preview pane renders the picked result's file with the read pane's File view: full
content, syntax highlighted (`diff-view.md`). The pane title names the previewed file.

- The pick's movement retargets the preview. A sweep never waits on it: the preview
  renders once, when the pick settles.
- A `Code` pick centers its hit line in the pane, bands it like a cursor row, and
  emphasizes the match. A `Files` pick previews from the top.
- `PageUp` / `PageDown` scroll the preview. A moved preview re-centers on the next pick.
- A poll that changes the previewed file repaints it in place, the scroll clamped to the
  new length. The hit band shows only while its line still exists (`overview.md` Continuity).
- With nothing to preview — no results, or a deleted file — the pane shows a dim notice.
- A file the File view degrades to a notice (binary, too large) previews as that notice.

## Keys on the screen

Printable keys edit the query. The rest:

| key                     | does                                     | mouse                   |
| ----------------------- | ---------------------------------------- | ----------------------- |
| `tab`                   | flip `Files` / `Code`, keeping the query | click a mode chip       |
| `↓` / `↑`, `ctrl+n/p`   | move the pick                            | click a result, wheel   |
| `PageUp` / `PageDown`   | scroll the preview                       | wheel over the preview  |
| `enter`                 | open the pick                            | click the picked result |
| `esc`                   | close, place untouched                   | —                       |

The footer shows these while the screen is open. With nothing to pick — warming, errored,
or no matches — it shows the mode flip and `esc`.

## Opening a result

| pick             | outcome                                                                          |
| ---------------- | -------------------------------------------------------------------------------- |
| a file result    | `All files` opens on the file, the navigator selection on it, ancestors expanded  |
| a code result    | the same, with the read-pane cursor on the hit line, scrolled visible             |
| a vanished path  | nothing opens, the screen stays                                                   |

Opening is a deliberate leave: it lands in `All files` whatever tab the search left, and
the origin tab keeps its place for `1`/`2`/`3`. A code result's line clamps into the
file's current length. Focus lands on the read pane.

## Failure semantics

- An engine failure shows its message in the results pane. Closing the screen returns to
  an untouched review, so search never blocks reviewing.
- A config error closes the screen when its view takes over (`config.md`). Recovery
  restores the tab beneath it. The query is not restored.

## Non-goals

- No changeset-scoped search. Roadmap (`overview.md`).
- No find within a single open file. That surface is `find-in-file.md`.
- No symbol table or definition classification.
- No regex or fuzzy code search. Code matching is literal.
- No keyboard resize of the search split. The divider drags the share.
- No multi-select or export of results.
- No commenting from the preview. Open the result and comment in the review views.
- No reviewr-side ranking. Result order is always the engine's.
- No search history of reviewr's own. The engine's frecency store is the only persistence.

## Related specs

- [file-list](./file-list.md)
- [diff-view](./diff-view.md)
- [find-in-file](./find-in-file.md)
- [input](./input.md)
- [tui](./tui.md)
- [overview](./overview.md)
