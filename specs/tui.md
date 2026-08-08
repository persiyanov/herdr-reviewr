---
Status: Current
Created: 2026-06-23
Last edited: 2026-08-08
---

# TUI

The terminal frame: the pane layout, the tabs, and how the view stays current.

## Overview

```
┌ 1 Changes  2 Files  3 PR  [uncommitted]                    9 changed  +42 −18 ┐
│ ⋯  11 unmodified lines                       │ M llm_registry.py  +18 -8  │
│ 40    def resolve(self, name):               │ M deep_research.py +155-62 │
│ 41 ▌  from .z import w                         │ D old_runner.py    -47     │
│ 41 ▌  from .x import y                         │ …                          │
│  ┌ comment · llm_registry.py:41 ──────────┐  │                            │
│  │ this import path looks wrong            │  │                            │
│  │ and breaks on 3.12█                     │  │                            │
│  └─────────────────────────────────────────┘ │                            │
│ 42    return registry[name]                   │                            │
├───────────────────────────────────────────────┴──────────────────────────┤
│ enter save · shift+enter newline · esc cancel                              │
└────────────────────────────────────────────────────────────────────────────┘
```

- The header carries the three tabs with the active one highlighted, the active scope, and the changed-file count with the scope's `+added −removed` totals, right-aligned one cell off the pane edge.
- The `All files` tab's header label reads `Files`.
- On the `branch` scope the header names the base after the scope, `vs dev`, the bare branch name however it resolved. Clicking it opens the base picker (`input.md`). With no resolving base it reads `no base`.
- A skipped pick or `--base` shows after the base, `vs main · dev missing` — and after the empty state too, `no base · dev missing`, so a dormant choice never reads as never-chosen.
- A base name the header cannot fit truncates with a trailing `…`. The picker always shows it whole. Too narrow for even one column of the name, the base leaves the header rather than paint a nameless `vs`.
- The header's line totals drop a zero side and vanish when nothing changed, like a file row's stats (`file-list.md`).
- The active tab sets both panes: diff and changed files in `Changes`, content and repo tree in `All files`, checks and comments in `PR` (`diff-view.md`, `pr-tab.md`).
- The comment input opens inline, directly under the last line of the selection, and grows as you type (`input.md`). It is never a footer band.
- The footer is a live action bar (`input.md`).
- The comments list, the agent picker, and the base picker open as popups over the body (`input.md`, `herdr-host.md`). While one is open, every painted color in the header and the body recedes halfway to the theme base. The footer stays bright.
- The review loop is the same in `Changes` and `All files`. `PR` is a read-only mirror. Comments are one set across the authoring tabs and export together.

The navigator has one global position across all tabs, and the position derives the split direction.

| position | layout                                    |
| -------- | ----------------------------------------- |
| `right`  | read pane left, navigator right (default) |
| `left`   | navigator left, read pane right           |
| `top`    | navigator above the read pane             |
| `bottom` | read pane above the navigator             |

| positions       | default navigator share | allowed share |
| --------------- | ----------------------- | ------------- |
| `left`, `right` | 32% of the body width   | 15–60%        |
| `top`, `bottom` | 25% of the body height  | 15–50%        |

The side and stacked shares are separate session values. Switching position restores the share for that split direction. Restarting reviewr restores both defaults.

Dragging the divider changes the active split direction's share. A resize never crosses that direction's allowed range.

When the split axis has at least six cells, each pane keeps at least three cells along it. Below six cells, the body divides as evenly as possible and the navigator position does not change.

A layout change moves nothing else (`overview.md`), and both remembered shares persist across it.

`navigator-hide` hides the navigator, and the read pane takes the whole body. No divider exists while hidden, so nothing drags. The same action shows it again in its kept position and share, and focus stays on the read pane. Hidden is one state across `Changes` and `All files`. On `PR` the navigator always shows: `navigator-hide` is inert there, and a `PR` visit leaves the hidden state untouched. Restarting reviewr shows the navigator. Config recovery preserves the hidden state (`config.md`). Hiding is presence, never a position. The `navigator-position` cycle has no hidden stop.

## Behavior

### Tabs

- Each tab owns its content state: the open file or card, scroll, cursor, fold expansions, and preview choice. Nothing carries between tabs, and switching away and back restores the tab exactly.
- The footer's shortcut expansion is one global toggle, not tab state (`input.md`).
- A first visit opens the tab's first file or card. A collapsed tree with the cursor on a directory opens nothing until a pick.
- A tab switch keeps the focused pane. An empty read pane focuses the navigator.
- While the navigator is hidden, focus is the read pane.

### Refresh

- The view polls the worktree every `N` seconds, default 2, configurable.
- A poll rebuilds the changed set and the file tree, reconciles them into the view (`overview.md`), and refreshes the open diff as it lands.
- A result lands whole: the header counts and the list they head come from one refresh.
- A result lands only when the view it described is still current: the same repository, tab, scope, and scope base. The scope base is the branch base or the turn baseline. A result that no longer matches is discarded, and a newer request supersedes an older one.
- Entering a file tab paints the tab's stashed state in the switch frame, exactly as it was left, and a refresh lands behind it (`overview.md`). A first-ever visit has no stash and loads before its frame.
- A scope switch rebuilds the changed set before its frame. A `last-turn` switch diffs against the most recently observed baseline. The `All files` tree re-marks its rows in place and refreshes behind the switch.
- While a comment is being composed, the input and its diff are frozen. A result that lands mid-composition leaves both untouched, however early its refresh began. The file list still updates.
- `r` triggers an immediate refresh. Its result lands like a poll's.
- `r` shows a one-cell `⟳` immediately. An ambient refresh shows it only after 200ms in flight. Once shown, the glyph holds for at least 300ms. It paints in a reserved cell at the end of the tab strip, so nothing shifts when it appears.
- Each tab shows only its own refresh: the file tabs the world refresh, the `PR` tab its fetch on its own cadence (`pr-tab.md`).
- Refresh uses no herdr events. The same poll samples the agents in the worktree for the `last-turn` baseline (`herdr-host.md`).
- In `last-turn` scope, before a turn start is observed, `Changes` names why it is empty and never shows a stale or whole-worktree diff. Both panes read one message, so they cannot disagree. A poll that found no agent in the worktree reads `no agent works here`; anything else, including membership no poll has observed yet, reads `waiting for the first turn` (`herdr-host.md`). `All files` keeps its content.

## Failure semantics

- A poll never touches the comment input or saved comments. Draft text and caret survive every refresh.
- A config error and its automatic-reload remedy replace the view. Saved comments, an open composer, comments list, or base picker, and the footer's shortcut expansion all survive it (`config.md`).
- A poll that finds no change makes no visible update: no flicker, no lost selection or scroll.
- A refresh in flight never delays input or a paint.
- A first open of a very large file can briefly block.
- A hung clipboard or agent send can briefly block input.

## Non-goals

- No editing, staging, or committing from the UI.
- No side-by-side split view. The diff is one unified column.
- No per-tab navigator position. One position applies to every tab.
- No automatic position or content-sized navigator. Layout changes only through config, `p`, `z`, resize keys, or dragging.
- No configured hidden default. The navigator starts visible, and only `navigator-hide` hides it.
- No multi-file review stream. Each read pane shows one selected item.
- No header `Send` button. Send lives on its keys and the footer (`input.md`).

## Related specs

- [config](./config.md)
- [input](./input.md)
- [diff-view](./diff-view.md)
- [file-list](./file-list.md)
- [pr-tab](./pr-tab.md)
- [review-model](./review-model.md)
- [search](./search.md)
