---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-21
---

# TUI

The terminal frame: the two-pane layout, the tabs, and how the view stays current.

## Overview

```
┌ 1 Changes  2 All files  3 PR  [uncommitted]  9 changed  +42 −18 [ Send (3) ] ┐
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

- The header shows the three tabs with the active one highlighted, the active scope, the changed-file count with the scope's `+added −removed` line totals, and a clickable `Send` button with the comment count. The totals drop a zero side and vanish when nothing changed, like a file row's stats (`file-list.md`).
- The read pane shows the selected file's diff or content (`diff-view.md`). The navigator pane shows the active tab's choices.
- The comment input opens inline, directly under the last line of the selection (`input.md`). It pushes the diff below it down and grows as you type. It is never a footer band.
- The footer is a live action bar (`input.md`).
- The active tab sets both panes: diff and changed files in `Changes`, content and repo tree in `All files`, checks and comments in `PR` (`pr-tab.md`).
- The review loop is the same in `Changes` and `All files`. `PR` is a read-only mirror. Comments are one set across the authoring tabs and export together.

The navigator has one global position across all tabs.

| position | layout                                      |
| -------- | ------------------------------------------- |
| `right`  | read pane left, navigator right (default)   |
| `left`   | navigator left, read pane right             |
| `top`    | navigator above the read pane               |
| `bottom` | read pane above the navigator               |

The position derives the split direction. Left and right split columns. Top and bottom split rows.

| positions       | default navigator share | allowed share |
| --------------- | ----------------------- | ------------- |
| `left`, `right` | 32% of the body width   | 15–60%        |
| `top`, `bottom` | 25% of the body height  | 15–50%        |

The side and stacked shares are separate session values. Switching position restores the share for that split direction. Restarting reviewr restores both defaults.

Dragging the divider changes the active split direction's share. A resize never crosses that direction's allowed range.

When the split axis has at least six cells, each pane keeps at least three cells along that axis: two border cells and one interior cell. Below six cells, the body divides as evenly as possible. The navigator position does not change, and drawing and hit-testing stay inside the allocated rectangles.

Every layout change preserves the focused pane and each pane's cursor or selection identity. Scroll stays where it is valid in the new viewport and otherwise clamps. Both remembered shares persist.

## Behavior

### Tabs

- Each tab owns its content state: the open file or card, scroll, cursor, fold expansions, and preview choice. Nothing carries between tabs. The footer's shortcut expansion is not tab state — it is one global toggle (`input.md`).
- Switching away and back restores the tab exactly.
- A first visit opens the tab's first file or card. A collapsed tree with the cursor on a directory opens nothing until a pick.
- A tab switch keeps the focused pane. An empty read pane focuses the navigator.

### Refresh

- The view polls the worktree every `N` seconds, default 2, configurable.
- A poll rebuilds the changed set and the file tree off the frame loop. The result reconciles into the view, keeping the selected file and scroll where the file still exists, and refreshes the open diff as it lands.
- A result lands whole: the header counts and the list they head come from one refresh.
- A result lands only when the view it described is still current: the same repository, tab, scope, and scope base. The scope base is the branch base or the turn baseline. A result that no longer matches is discarded, and a newer request supersedes an older one.
- Entering a file tab paints the tab's stashed state in the switch frame, exactly as it was left. A refresh lands behind it — stale until it lands, never wrong (`overview.md` Continuity). A first-ever visit has no stash to paint and loads before its frame, so the header never describes a tab that shows nothing.
- A scope switch rebuilds the changed set before its frame, so the list never shows another scope's files under the new scope's label. A `last-turn` switch diffs against the most recently observed baseline. The `All files` tree re-marks its rows in place and refreshes behind the switch.
- While a comment is being composed, the input and its diff are frozen. A result that lands mid-composition leaves both untouched, however early its refresh began. The file list still updates.
- `r` triggers an immediate refresh. Its result lands like a poll's.
- `r`'s refresh shows a one-cell `⟳` immediately. An ambient refresh — a poll, a tab entry, a scope switch — shows it only after 200ms in flight. Once shown, the glyph holds for at least 300ms, then clears when the result has landed or been discarded. It paints in a reserved cell at the end of the tab strip, so nothing shifts when it appears. Each tab shows only its own refresh: the file tabs the world refresh, the `PR` tab its fetch.
- The `PR` tab fetches on its own cadence (`pr-tab.md`), separate from the worktree poll.
- Refresh uses no herdr events. The same poll samples the agent's status for the `last-turn` baseline (`herdr-host.md`).
- In `last-turn` scope, before a turn start is observed, `Changes` shows `waiting for the agent's next turn`, never a stale or whole-worktree diff. `All files` keeps its content.

## Failure semantics

- A poll never touches the comment input or saved comments. Draft text and caret survive every refresh.
- A config error and its automatic-reload remedy replace the view. Saved comments always survive. An open composer or comments list survives with its tab's state, as does the footer's shortcut expansion. Recovery restores them.
- A poll that finds no change makes no visible update: no flicker, no lost selection or scroll.
- A refresh in flight never delays input or a paint.
- Opening a file builds its diff on the paint path. A first open of a very large file can briefly block.
- Clipboard and agent-send calls run synchronously between frames. A hung send can briefly block input.

## Non-goals

- No editing, staging, or committing from the UI.
- No side-by-side split view. The diff is one unified column, split is roadmap.
- No per-tab navigator position. One position applies to every tab.
- No automatic position or content-sized navigator. Layout changes only through config, `p`, resize keys, or dragging.
- No hidden navigator. Both panes remain present.
- No multi-file review stream. Each read pane shows one selected item.

## Related specs

- [config](./config.md)
- [input](./input.md)
- [diff-view](./diff-view.md)
- [file-list](./file-list.md)
- [pr-tab](./pr-tab.md)
- [review-model](./review-model.md)
- [search](./search.md)
