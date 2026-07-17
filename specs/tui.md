---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-15
---

# TUI

The terminal interface: the layout, the keyboard and mouse, and how the view stays current.

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
│ enter save · ⇧⏎ newline · esc cancel                                       │
└────────────────────────────────────────────────────────────────────────────┘
```

- The header shows the three tabs with the active one highlighted, the active scope, the changed-file count with the scope's `+added −removed` line totals, and a clickable `Send` button with the comment count. The totals drop a zero side and vanish when nothing changed, like a file row's stats (`file-list.md`).
- The read pane shows the selected file's diff or content (`diff-view.md`). The navigator pane shows the active tab's choices.
- The comment input opens inline, directly under the last line of the selection. It pushes the diff below it down and grows as you type. It is never a footer band.
- The footer is a live action bar.
- The active tab sets both panes: diff and changed files in `Changes`, content and repo tree in `All files`, checks and comments in `PR`.
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

### Interaction

Every action has a key. The mouse-relevant ones also work by click or drag.

The keymap is rebindable per action through `[keybindings]` in the plugin config (`config.md`):

- The `action` column names the action for `[keybindings]`.
- The character keys are defaults.
- A key that is not a bare character (the arrows, `tab`, `esc`, `enter`, the page keys) is fixed.
- A key hint in the header or the footer shows its action's first bound key.
- The comments list acts through the same bindings and closes on `esc` and the `comments` binding.
- Prose and mockups elsewhere show the default keys.

| action                                                   | does                                        | keys                                        | mouse                         |
| -------------------------------------------------------- | ------------------------------------------- | ------------------------------------------- | ----------------------------- |
| `down` / `up`                                            | move the cursor in the focused pane         | `j` / `k` / `↓` / `↑`                       | click a row                   |
| `next-hunk` / `prev-hunk`                                | jump to the next / previous hunk            | `]` / `[`                                   | —                             |
| `next-file` / `prev-file`                                | jump to the next / previous file            | `f` / `F`                                   | —                             |
| —                                                        | collapse / expand a directory               | `←` / `→`                                   | click the directory row       |
| —                                                        | switch focus between list and diff          | `tab`                                       | click a pane                  |
| —                                                        | move a page                                 | `PageUp` / `PageDown` / `ctrl+u` / `ctrl+d` | —                             |
| —                                                        | scroll the viewport, selection put          | —                                           | wheel over the pane           |
| —                                                        | scroll the diff horizontally (wrap off)     | `←` / `→`                                   | —                             |
| `scope-uncommitted` / `scope-branch` / `scope-last-turn` | switch scope                                | `u` / `b` / `t`                             | click the scope chip to cycle |
| `tab-changes` / `tab-all-files` / `tab-pr`               | switch tab                                  | `1` / `2` / `3`                             | click a tab name              |
| —                                                        | expand the fold under the cursor            | `→`                                         | click the `⋯` row             |
| —                                                        | open a link in rendered markdown            | —                                           | click the link                |
| `wrap`                                                   | toggle line wrap                            | `w`                                         | —                             |
| `preview`                                                | toggle the markdown preview                 | `m`                                         | —                             |
| `navigator-position`                                     | move the navigator clockwise                | `p`                                         | —                             |
| `navigator-grow` / `navigator-shrink`                    | grow / shrink the navigator                 | `<` / `>`                                   | drag the divider              |
| `select`                                                 | select a line range, removed lines included | `v` then move                               | click-drag in the diff        |
| —                                                        | clear the selection                         | `esc`                                       | —                             |
| `comment`                                                | comment on the selection                    | `c`, type, `enter`                          | after a drag-select           |
| `edit`                                                   | edit the comment under the cursor           | `e`                                         | —                             |
| `delete`                                                 | delete the comment under the cursor         | `d`                                         | —                             |
| `next-comment` / `prev-comment`                          | jump to next / previous comment             | `n` / `N`                                   | —                             |
| `comments`                                               | list and manage all comments                | `l`                                         | —                             |
| `send`                                                   | send all comments to the agent              | `s` / `S`                                   | click `Send`                  |
| `copy`                                                   | copy all comments to the clipboard          | `y` / `Y`                                   | —                             |
| `open-pr`                                                | open the PR in the browser (`PR` tab)       | `o`                                         | click the status chip         |
| `refresh`                                                | refresh now                                 | `r`                                         | —                             |
| `quit`                                                   | quit                                        | `q`                                         | —                             |

`navigator-position` cycles `right` → `bottom` → `left` → `top` → `right`.

`navigator-grow` and `navigator-shrink` change the active share by four percentage points. The allowed range clamps every change.

These three navigator actions work from either main pane on every tab. While the comment editor is open, their printable characters are text. In the comments list they are inert. Those local modes omit the navigator actions from the footer.

A divider drag belongs to the navigator position and split axis at mouse-down. A keypress, terminal resize, or config-driven layout change cancels it. After cancellation, drag events are consumed until mouse-up rather than becoming a selection in the read pane.

Writing a comment: select a range or land on a line, press `c`, type into the inline box, `enter` saves and `esc` cancels. A saved comment renders as a read-only card spliced under its line, titled with its location, so written feedback stays on screen. `e` reopens the card as an edit box in place, hiding the card while editing. `d` deletes it. A successful send reports that the comments were added to the agent input; a successful copy reports that they were copied. The transient status pluralizes `comment` and fades.

### Changeset traversal

`next-hunk` / `prev-hunk` step the diff cursor between changes, from either pane. A step lands on the first row of a run of changed rows. A context line or a fold ends a run, so two edits three lines apart are two stops.

- Each press jumps to the nearest run past the cursor: `next-hunk` below, `prev-hunk` above.
- With no run left that way, the first press arms a crossing and holds the cursor still. The next press the same way opens the adjacent file on its nearest run. A notice diff, which has no runs at all, arms on the first press like any other file.
- The armed crossing leads the footer, keyed to the step that armed it. It is the one movement key the footer names.
- Any other input drops the arm and still does its own work. A background poll keeps it, unless it changes the open file.
- A crossing arms only when a file to cross to exists. At the changeset's ends nothing is offered and nothing moves.
- A file with no changed rows is crossed over, notice diffs (`binary`, `too_large`) included.
- The steps are inert in `All files` and in the markdown preview, which paint no changed rows.

`next-file` / `prev-file` skip a file per press, from either pane, and never arm:

- In the diff, each press opens the next or previous file, cursor on its first row. Focus stays on the diff.
- In the file list, each press moves the cursor to the nearest file row, skipping directories.
- The skips land on every file, notice diffs included. From a preview, the opened file starts in source (`diff-view.md`).

The steps and the skips share the rest:

- Adjacency is the list's visible order, so a collapsed subtree is skipped.
- Opening a file this way moves the list selection onto it.
- With no target in the pressed direction, a press does nothing.
- Both are inert while a line selection is live and while the comments list is open.
- The `PR` tab has neither.

### Footer

The footer is a live action bar. It shows the actions available right now, the most likely one highlighted, and drops the least relevant when the line fills. It never lists a key that would not work in the current state.

```
 c comment · v select lines                 │ ⇥ files · p position · 1·2·3 · q
```

The bar fills by priority until the width runs out, and a trailing `…` marks anything clipped:

| slot       | content                                                                        |
| ---------- | ------------------------------------------------------------------------------ |
| primary    | the most likely next step, in a bright accent, always shown                    |
| send       | `s send N`, present once any comment is written, just below the primary        |
| actions    | what else works here, in normal text                                           |
| navigation | dim, stable: pane toggle, navigator position, tab digits, quit; dropped first |
| status     | a transient message (`comment added`) that fades, never replacing actions      |

The actions follow the cursor:

| cursor on                                | primary                        | also                    |
| ---------------------------------------- | ------------------------------ | ----------------------- |
| an armed crossing                        | `] next file` / `[ prev file`  | the cursor's own actions, demoted |
| a diff line                              | `c comment`                    | `v select`              |
| a line of a markdown file that previews  | `c comment`                    | `v select · m preview`  |
| a live selection                         | `c comment`                    | `esc clear`             |
| a commented line                         | `e edit`                       | `d delete · n/N jump`   |
| a fold                                   | `→ expand fold`                | —                       |
| an open markdown preview                 | `m source`                     | —                       |
| a file (file list)                       | `⇥ diff`                       | —                       |
| a collapsed directory                    | `→ expand`                     | —                       |
| an expanded directory                    | `← collapse`                   | —                       |
| nothing to review (awaiting turn)        | `u/b/t scope`                  | `r refresh`             |

- An armed crossing outranks the cursor's own action, since only the footer says the next press leaves the file.
- `u/b/t scope` shows in every `Changes` and `All files` context, except where it is itself the primary.
- Movement keys are never shown. The armed crossing is the one exception.
- The comment editor and the comments list show their own actions.
- The changed-file count and line totals live in the header. The footer carries only the comment count, inside `s send N`.
- On `PR` the bar leads with the PR's state, then `o open ↗`, then navigation.

### Tabs

- Each tab owns its content state: the open file or card, scroll, cursor, expansions, and preview choice. Nothing carries between tabs.
- Switching away and back restores the tab exactly.
- A first visit opens the tab's first file or card. A collapsed tree with the cursor on a directory opens nothing until a pick.
- A tab switch keeps the focused pane. An empty read pane focuses the navigator.

### PR tab

A read-only mirror of the pull request in the same two-pane frame. The navigator shows checks and selects the description or a comment. The read pane shows that selection. The header carries the PR's identity and state. It reads GitHub through `forge-host.md` and writes nothing. Its only outward action opens a link in the browser.

```
 1 Changes  2 All files  3 PR    Deep research: GPT-5.5/5.4-mini upgrade…  deep-research  merged #226 ↗
╭─ @codex · manager.py:115 ──────────────────────────╮╭─ Checks & comments ──────────╮
│ -    if primary_result.status == PERM_FAILURE:        ││ description                  │
│ -        return primary_result                        ││                              │
│                                                       ││ checks  ✗ 1 failing          │
│ Avoid falling back after target permanent failures.   ││  ✓ build-main-image          │
│ This now attempts a fallback for every non-success…   ││  ✗ tests                     │
│                                                       ││                              │
│                                                       ││ comments · 5                 │
│                                                       ││ @you    comment          5m  │
│                                                       ││▍@codex  manager.py:115   2h  │
│                                                       ││ @claude review           2h  │
│                                                       ││ @claude manager.py:39    2h  │
│                                                       ││ @claude parse.py:187 outdated│
╰───────────────────────────────────────────────────────╯╰─────────────────────────────╯
 ⚠ conflicts with main · ⇡ 2 unpushed · ✗ 1 failing · 5 comments   o open ↗   │ p position · 1·2·3 · r · q
```

- The header right-anchors a clickable `status #226 ↗` chip, status colored by lifecycle: `open` green, `draft` yellow, `merged` mauve, `closed` red. The PR title sits to its left, truncated to fit.
- Between title and chip sits the resolved head branch (`head_ref`, `forge-host.md`), dim, prefixed `⑂ ` when the head lives in a fork. On a narrow bar the branch drops first.
- The ordinary no-PR body says only `No pull request yet. Ready to ship?` A detached HEAD says `No pull request found — HEAD is detached.`
- The footer leads with merge, sync, checks, and comment counts. Merge and sync show only while the PR is open. A capped surface appends `+more on GitHub ↗` (`forge-host.md`).
- The navigator, titled `Checks & comments`, shows a status-only checks section above the comments list. The cursor walks the description row and the comments.
- Comments list newest first, each row `@author anchor age`, with `outdated` or `resolved` markers where GitHub receded the thread.
- A non-empty PR description pins a `description` row at the top of the navigator, above the checks. An emptied description vanishes like a comment: the cursor clamps, the read pane resets.
- The read pane shows the selected comment: a finding shows its `snippet` then the body, a review or plain comment shows its prose, the description row shows the PR description.
- Bodies render as markdown (`markdown.md`). A finding's `snippet` stays plain `+`/`−`-colored lines.
- A human author is emphasized over the bots.
- `j`/`k` or a click selects a description or comment and reveals it in the navigator viewport. Checks are not selectable.
- The wheel over the navigator scrolls its viewport without changing the selection. `PageUp`/`PageDown` scroll the focused pane. The wheel over the read pane scrolls the read pane. Both panes stop with their last line at the bottom edge. `o` or the chip opens the PR in the browser.
- A body taller than the read pane shows a scrollbar on the pane's right border. One that fits shows none.
- A retry notice for a preserved snapshot stays fixed above the read body, so it remains visible
  without resetting the reader's scroll.
- The authoring keys (`s`, `c`, `v`, `d`, `e`) do nothing here.
- A merged or closed PR shows the same mirror, read-only.
- No usable `gh` shows the matching failure state from `forge-host.md`, naming the command that unblocks it.

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
- While a comment is being composed, the input and its diff are frozen. The file list still updates.
- `r` forces an immediate reload.
- The `PR` tab fetches on open, on entering the tab, on `r`, and on the agent's turn-end while active, with a slow fallback timer. Its cadence is separate from the worktree poll.
- A PR refetch keeps your place: the cursor follows the selected comment by identity, and both pane scroll positions hold. A vanished comment clamps the cursor and resets the read pane.
- Refresh uses no herdr events. The same poll samples the agent's status for the `last-turn` baseline (`herdr-host.md`).
- In `last-turn` scope, before a turn start is observed, `Changes` shows `waiting for the agent's next turn`, never a stale or whole-worktree diff. `All files` keeps its content.

## Failure semantics

- A poll never touches the comment input or saved comments. Draft text and caret survive every refresh.
- A config error and its automatic-reload remedy replace the view. Saved comments always survive. An open composer or comments list survives with its tab's state. Recovery restores them.
- A poll that finds no change makes no visible update: no flicker, no lost selection or scroll.
- Git, clipboard, and agent-send calls run synchronously between frames. A very large diff or a hung send can briefly block input. Moving them off the draw path is a v1 non-goal.
- A paste outside the comment editor is ignored. It never starts or mutates a comment.

## Non-goals

- No editing, staging, or committing from the UI.
- No side-by-side split view. The diff is one unified column, split is roadmap.
- No jump from a PR comment's anchor to the code tabs.
- No text selection, cut/copy, undo/redo, markdown rendering, or click-to-place-caret in the comment editor.
- No modifier, named-key, or sequence notation in the keymap. Single characters are the v1 surface.
- No `down` / `up` crossing at a file's edges. The line cursor clamps there.
- No per-tab navigator position. One position applies to every tab.
- No automatic position or content-sized navigator. Layout changes only through config, `p`, resize keys, or dragging.
- No hidden navigator. Both panes remain present.
- No multi-file review stream. Each read pane shows one selected item.

## Related specs

- [config](./config.md)
- [diff-view](./diff-view.md)
- [file-list](./file-list.md)
- [review-model](./review-model.md)
- [markdown](./markdown.md)
