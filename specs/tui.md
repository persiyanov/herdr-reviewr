---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-12
---

# TUI

The terminal interface: the layout, the keyboard and mouse, and how the view stays current.

## Overview

```
┌ 1 Changes  2 All files  3 PR  [uncommitted]  9 changed ──── [ Send (3) ] ┐
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
│ enter save · ⇧⏎ newline · esc cancel                                       │
└────────────────────────────────────────────────────────────────────────────┘
```

- The header shows the three tabs with the active one highlighted, the active scope, the changed-file count, and a clickable `Send` button with the comment count.
- The left pane is the selected file's diff (`diff-view.md`). The right pane is the file navigator (`file-list.md`).
- The comment input opens inline, directly under the last line of the selection. It pushes the diff below it down and grows as you type. It is never a footer band.
- The footer is a live action bar.
- The active tab sets both panes: diff and changed files in `Changes`, content and repo tree in `All files`, checks and comments in `PR`.
- The review loop is the same in `Changes` and `All files`. `PR` is a read-only mirror. Comments are one set across the authoring tabs and export together.

## Behavior

### Interaction

Every action has a key. The mouse-relevant ones also work by click or drag. The keys are a provisional v1, not a final keymap.

| action                                        | keyboard                            | mouse                       |
| --------------------------------------------- | ----------------------------------- | --------------------------- |
| move the cursor in the focused pane           | `j` / `k` / `↑` / `↓`               | click a row                 |
| collapse / expand a directory                 | `←` / `→`                           | click the directory row     |
| switch focus between list and diff            | `tab`                               | click a pane                |
| move a page                                   | `PageUp` / `PageDown` / `ctrl+u` / `ctrl+d` | —                   |
| scroll the viewport, selection put            | —                                   | wheel over the pane         |
| scroll the diff horizontally (wrap off)       | `←` / `→`                           | —                           |
| switch scope                                  | `u` uncommitted / `b` branch / `t` last-turn | click the scope chip to cycle |
| switch tab                                    | `1` / `2` / `3`                     | click a tab name            |
| expand the fold under the cursor              | `→`                                 | click the `⋯` row           |
| toggle line wrap                              | `w`                                 | —                           |
| resize the panes                              | `]` / `[`                           | drag the divider            |
| select a line range, removed lines included   | `v` then move                       | click-drag in the diff      |
| clear the selection                           | `esc`                               | —                           |
| comment on the selection                      | `c`, type, `enter`                  | after a drag-select         |
| edit the comment under the cursor             | `e`                                 | —                           |
| delete the comment under the cursor           | `d`                                 | —                           |
| jump to next / previous comment               | `n` / `N`                           | —                           |
| list and manage all comments                  | `l`                                 | —                           |
| resolve / reopen the highlighted comment (comments list only) | `x`                 | —                           |
| toggle hiding resolved comments (comments list only) | `h`                           | —                           |
| send all comments to the agent                | `s` / `S`                           | click `Send`                |
| copy all comments to the clipboard            | `y` / `Y`                           | —                           |
| refresh now                                   | `r`                                 | —                           |
| quit                                          | `q`                                 | —                           |

Writing a comment: select a range or land on a line, press `c`, type into the inline box, `enter` saves and `esc` cancels. A saved comment renders as a read-only card spliced under its line, titled with its location, so written feedback stays on screen. `e` reopens the card as an edit box in place, hiding the card while editing. `d` deletes it. A successful send reports a transient `sent N comments` status that fades.

An agent-authored comment (`review-model.md`) carries an ` agent ` chip in its card's border, tinted with the theme's `mauve` accent, and is read-only from the TUI — `e` on one shows `agent comments are read-only (x to resolve)` instead of opening it for edit. A resolved comment (either side's, `x` in the comments list) renders its whole card dimmed (the theme's `overlay1`) with a `resolved` marker in its title; `h` hides resolved cards in the diff pane entirely rather than only dimming them. The comments list itself (`l`) always shows every comment, resolved or not — it's the one surface that lets a resolved comment be reopened with `x`, so hiding rows there would strand them — and adds an `@agent` / `@you` author column and a `resolved` marker per row, alongside the existing `(stale)` flag.

### Footer

The footer is a live action bar. It shows the actions available right now, the most likely one highlighted, and drops the least relevant when the line fills. It never lists a key that would not work in the current state.

```
 c comment · v select lines                          │ ⇥ files · 1·2·3 · q
```

The bar fills by priority until the width runs out, and a trailing `…` marks anything clipped:

| slot        | content                                                             |
| ----------- | -------------------------------------------------------------------- |
| primary     | the most likely next step, in a bright accent, always shown           |
| send        | `s send N`, present once any comment is written, just below the primary |
| actions     | what else works here, in normal text                                  |
| orientation | dim, stable: the pane toggle, the tab digits, quit; dropped first     |
| status      | a transient message (`comment added`) that fades, never replacing actions |

The actions follow the cursor:

| cursor on                          | primary          | also                    |
| ---------------------------------- | ---------------- | ----------------------- |
| a diff line                        | `c comment`      | `v select`              |
| a live selection                   | `c comment`      | `esc clear`             |
| a commented line                   | `e edit`         | `d delete · n/N jump`   |
| a fold                             | `→ expand fold`  | —                       |
| a file (file list)                 | `⇥ diff`         | —                       |
| a collapsed directory              | `→ expand`       | —                       |
| an expanded directory              | `← collapse`     | —                       |
| nothing to review (awaiting turn)  | `u/b/t scope`    | `r refresh`             |

- `u/b/t scope` shows in every `Changes` and `All files` context, except where it is itself the primary.
- Movement keys are never shown.
- The comment editor and the comments list show their own actions.
- The changed-file count lives in the header. The footer carries only the comment count, inside `s send N`.
- On `PR` the bar leads with the PR's state, then `o open ↗`, then orientation.

### Tabs

- Each tab owns its state: the open file or card, scroll, cursor, expansions. Nothing carries between tabs.
- Switching away and back restores the tab exactly.
- A first visit opens the tab's first file or card. A collapsed tree with the cursor on a directory opens nothing until a pick.
- A tab switch keeps the focused side. An empty left pane focuses the tree.

### PR tab

A read-only mirror of the pull request in the same two-pane frame: the right pane navigates checks and comments, the left pane reads the selected comment, the header carries the PR's identity and state. It reads GitHub through `forge-host.md` and writes nothing. Its only outward action opens a link in the browser.

```
 1 Changes  2 All files  3 PR    Deep research: GPT-5.5/5.4-mini upgrade…  deep-research  merged #226 ↗
╭─ @codex · manager.py:115 ──────────────────────────╮╭─ Checks & comments ──────────╮
│ -    if primary_result.status == PERM_FAILURE:        ││ checks  ✗ 1 failing          │
│ -        return primary_result                        ││  ✓ build-main-image          │
│                                                       ││  ✓ review                    │
│ Avoid falling back after target permanent failures.   ││  ✗ tests                     │
│ This now attempts a fallback for every non-success…   ││                              │
│                                                       ││ comments · 5                 │
│                                                       ││▍@you    comment          5m  │
│                                                       ││ @codex  manager.py:115   2h  │
│                                                       ││ @claude review           2h  │
│                                                       ││ @claude manager.py:39    2h  │
│                                                       ││ @claude parse.py:187  outdated│
╰───────────────────────────────────────────────────────╯╰─────────────────────────────╯
 ⚠ conflicts with main · ⇡ 2 unpushed · ✗ 1 failing · 5 comments   o open ↗   │ 1·2·3 · r · q
```

- The header right-anchors a clickable `status #226 ↗` chip, status colored by lifecycle: `open` green, `draft` yellow, `merged` mauve, `closed` red. The PR title sits to its left, truncated to fit.
- Between title and chip sits the resolved head branch (`head_ref`, `forge-host.md`), dim, prefixed `⑂ ` when the head lives in a fork. On a narrow bar the branch drops first.
- The footer leads with merge, sync, checks, and comment counts. Merge and sync show only while the PR is open. A capped surface appends `+more ↗` (`forge-host.md`).
- The right pane, titled `Checks & comments`, shows a status-only checks section above the comments list. The cursor walks the comments.
- Comments list newest first, each row `@author anchor age`, with `outdated` or `resolved` markers where GitHub receded the thread.
- The left pane reads the selected comment: a finding shows its `diff_hunk` then the body, a review or plain comment shows its prose.
- A human author is emphasized over the bots.
- `j`/`k` or a click selects a comment. `PageUp`/`PageDown` and the wheel scroll the read pane. `o` or the chip opens the PR in the browser.
- The authoring keys (`s`, `c`, `v`, `d`, `e`) do nothing here.
- A merged or closed PR shows the same mirror, read-only.
- No open PR, or no usable `gh`, shows the matching empty state from `forge-host.md`, naming the command that unblocks it.

### Comment editor

A plain-text field that edits at the caret, not only at the end. An empty box shows a dim `Leave a comment…` placeholder. `e` preloads the existing text with the caret at the end.

```
┌ comment · llm_registry.py:41 ───────────┐
│ this import path looks wrong█            │
│ and breaks on 3.12                       │
└──────────────────────────────────────────┘
```

| key                                             | does                                            |
| ----------------------------------------------- | ------------------------------------------------ |
| `←` / `→`                                       | move the caret one character                     |
| `↑` / `↓`                                       | move the caret one wrapped row, keeping column   |
| `Home` / `End`, `Ctrl+A` / `Ctrl+E`             | move to the start / end of the logical line      |
| `Alt+b` / `Alt+f`, `Alt`/`Ctrl` + `←` / `→`     | move by a word                                   |
| `Backspace` / `Delete`                          | delete before / after the caret                  |
| `Ctrl+W`                                        | delete the word before the caret                 |
| `Ctrl+U` / `Ctrl+K`                             | delete to the start / end of the logical line    |
| `Alt+Enter` / `Shift+Enter` / `Ctrl+J`          | insert a newline                                 |
| `Enter` / `Esc`                                 | save / cancel, cancel discards the draft         |

- A paste arrives whole via bracketed paste. A multi-line paste keeps its newlines. `\r\n` and `\r` normalize to `\n`.
- Movement, insertion, and deletion are character-wise. Multi-byte and wide characters count as whole characters.
- `↑`/`↓` move by wrapped rows. `Home`/`End` and the kill keys act on the logical line, the run of text between explicit newlines.
- `Alt+b`/`Alt+f` always survive as ESC-prefixed sequences. The modified arrows work where the terminal delivers them. The character arrows, `Home`/`End`, and `Ctrl+A`/`Ctrl+E` always work.

### Refresh

- The view polls the worktree every `N` seconds, default 2, configurable.
- A poll reloads the file list and the open diff, keeping the selected file and scroll where the file still exists.
- The same poll checks the comment store's change signature (entry names + mtimes) and re-syncs from disk when it moved — an agent's `comment add`/`resolve`/`rm`, or another session's edit, shows up without the user doing anything (`review-model.md`). Disk wins for any id it has; a local id disk doesn't have survives only if this session never persisted it (an `on-send` draft awaiting `s`) — one this session already wrote and no longer finds on disk is treated as removed elsewhere and dropped.
- While a comment is being composed, the input and its diff are frozen. The file list still updates.
- `r` forces an immediate reload.
- The `PR` tab fetches on open, on entering the tab, on `r`, and on the agent's turn-end while active, with a slow fallback timer. Its cadence is separate from the worktree poll.
- A PR refetch keeps your place: the cursor follows the selected comment by identity, the read-pane scroll holds. A vanished comment clamps the cursor and resets the read pane.
- Refresh uses no herdr events. The same poll samples the agent's status for the `last-turn` baseline (`herdr-host.md`).
- In `last-turn` scope, before a turn start is observed, the list and diff show `waiting for the agent's next turn`, never a stale or whole-worktree diff.

## Failure semantics

- A poll never touches the comment input. Draft text and caret survive every refresh.
- A poll's comment-store sync only ever adds, updates, or drops rows to match disk — it never touches the comment editor or a draft awaiting `s`.
- A poll that finds no unchanged file and no comment-store signature change makes no visible update: no flicker, no lost selection or scroll.
- Git, clipboard, and agent-send calls run synchronously between frames. A very large diff or a hung send can briefly block input. Moving them off the draw path is a v1 non-goal.
- A paste outside the comment editor is ignored. It never starts or mutates a comment.

## Non-goals

- No editing, staging, or committing from the UI.
- No side-by-side split view. The diff is one unified column, split is roadmap.
- No text selection, cut/copy, undo/redo, markdown rendering, or click-to-place-caret in the comment editor.

## Related specs

- [diff-view](./diff-view.md)
- [file-list](./file-list.md)
- [review-model](./review-model.md)
