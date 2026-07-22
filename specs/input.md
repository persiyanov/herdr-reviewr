---
Status: Current
Created: 2026-07-17
Last edited: 2026-07-21
---

# Input

Driving the review: the keymap, the changeset traversal, the live footer, and the comment editor.

## Overview

Every action has a key. The mouse-relevant ones also work by click or drag.

The keymap is rebindable per action through `[keybindings]` in the plugin config (`config.md`):

- The `action` column names the action for `[keybindings]`.
- The keys shown are defaults: a bare character, or a `ctrl+`/`alt+` chord (`config.md`).
- The arrows, `tab`, `esc`, `enter`, and the page keys are structural. They are fixed and never rebind.
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
| `search`                                                 | open the search screen (`search.md`)        | `/`                                         | —                             |
| `find`                                                   | open in-file find (`find-in-file.md`)       | `ctrl+f`                                    | —                             |
| `keys`                                                   | toggle the footer's full shortcut list      | `?`                                         | —                             |
| `send`                                                   | send all comments to the agent              | `s` / `S`                                   | click `Send`                  |
| `copy`                                                   | copy all comments to the clipboard          | `y` / `Y`                                   | —                             |
| `open-pr`                                                | open the PR in the browser (`pr-tab.md`)    | `o`                                         | click the status chip         |
| `refresh`                                                | refresh now                                 | `r`                                         | —                             |
| `quit`                                                   | quit                                        | `q`                                         | —                             |

`navigator-position` cycles `right` → `bottom` → `left` → `top` → `right`.

`navigator-grow` and `navigator-shrink` change the active share by four percentage points. The allowed range clamps every change.

These three navigator actions work from either main pane on every tab. While the comment editor is open, their printable characters are text. In the comments list they are inert. Those local modes omit the navigator actions from the footer.

A divider drag belongs to the navigator position and split axis at mouse-down. A keypress, terminal resize, or config-driven layout change cancels it. After cancellation, drag events are consumed until mouse-up rather than becoming a selection in the read pane.

Writing a comment: select a range or land on a line, press `c`, type into the inline box, `enter` saves and `esc` cancels. A saved comment renders as a read-only card spliced under its line, titled with its location, so written feedback stays on screen. `e` reopens the card as an edit box in place, hiding the card while editing. `d` deletes it. A successful send reports that the comments were added to the agent input; a successful copy reports that they were copied. The transient status shows on the footer, pluralizes `comment`, and fades without covering the primary action.

## Behavior

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

The footer is one row: the primary next step, the cursor's own actions, and `send` once a comment
exists, closing with a `?`. Pressing `?` expands it to every shortcut that works here, and it stays
until `?` or `esc`. It never lists a key that would not work in the current state.

```
 e edit · d delete · n/N jump · s send 2                                      ?
```

Opening it turns the one-row action bar into a labeled grid. Row 1 becomes the `do` band under a
dim `do` label: the primary and the cursor's actions. Two bands follow it, `go` (the always-there
keys) and `move` (cursor movement). Every band's content aligns in one column. The `?` stays at the
right of the `do` row.

```
 do    e edit · d delete · n/N jump · s send 2                                ?
 go    u/b/t scope · / search · ctrl+f find · w wrap · l list · y copy · r refresh · 1·2·3 tabs
       tab files · p position · q quit
 move  j k · ] [ hunk · f F file · PageUp PageDown
```

A band wraps to as many rows as its keys need. The label sits on the first row, and continuation
rows indent under the keys. A cursor action that does not fit row 1 continues under the `do` label
on its own indented row.

Row 1 is always shown:

| slot    | content                                                                           |
| ------- | --------------------------------------------------------------------------------- |
| primary | the most likely next step, in a bright accent, never dropped                      |
| send    | `s send N`, present once any comment is written, after the primary, never dropped |
| actions | the cursor's other actions, in normal text, trimmed to fit                        |
| more    | a `?` at the right, muted but legible — always present, and expands the rest      |

A narrow row drops trailing actions to fit. The primary, `send`, and the `?` never drop. On a pane
too narrow even for those, the primary truncates before the `?` does.

The `?` expansion:

- It lists every shortcut applicable in the current context that is not already on row 1, wrapped
  below row 1 in three labeled bands, each a dim label then its keys. `do`: the cursor's actions.
  `go`: the keys that work anywhere — scope, search, find, wrap, the comments list, copy, refresh, the
  tabs, the pane toggle, the navigator-position key, quit. `move`: down and up, the hunk and file
  steps, the page keys. An empty band is dropped, and a key that would not work in the current state
  never appears. The hunk step shows only where it works, the `Changes` diff and never a preview
  (see Changeset traversal). `PR` has no hunk or file steps.
- Row 1 wears the `do` label and aligns into the grid only while the panel is open. Collapsed, it is
  the flush action bar with no label. The `?` sits at its right in both states.
- It takes body rows down to the read pane's minimum (`tui.md`). A context that needs more rows than
  that shows only what fits, and row 1 always survives.
- Its open state is place state (`overview.md`). `?` and `esc` move it. A world event only re-derives
  its content in place, reconciled by identity, never the toggle. It opens collapsed, is never saved,
  and config recovery preserves it.
- `?` toggles it. `esc` closes it, one step behind a live selection and an armed crossing — each `esc`
  consumes exactly one.

Row 1's primary and actions follow the cursor:

| cursor on                                | primary                        | also                              |
| ---------------------------------------- | ------------------------------ | --------------------------------- |
| an armed crossing                        | `] next file` / `[ prev file`  | the cursor's own actions, demoted |
| a diff line                              | `c comment`                    | `v select`                        |
| a line of a markdown file that previews  | `c comment`                    | `v select · m preview`            |
| a live selection                         | `c comment`                    | `esc clear`                       |
| a commented line                         | `e edit`                       | `d delete · n/N jump`   |
| a fold                                   | `→ expand fold`                | —                       |
| an open markdown preview                 | `m source`                     | —                       |
| a file (file list)                       | `tab diff`                     | —                       |
| a collapsed directory                    | `→ expand`                     | —                       |
| an expanded directory                    | `← collapse`                   | —                       |
| nothing to review (awaiting turn)        | `u/b/t scope`                  | `r refresh`             |

- An armed crossing outranks the cursor's own action and leads row 1, since only the footer says the next press leaves the file. It is the one movement key on row 1 (see Changeset traversal). While it is armed, the `move` band drops the hunk step, whose key row 1 now shows.
- `scope`, `search`, and `find` are global, not cursor actions, so the `go` band carries them, never row 1 — `search` in every context, `find` wherever the read pane has content (`search.md`, `find-in-file.md`). `scope` leads row 1 only where nothing else does, an empty or notice diff.
- Movement keys never sit on row 1. The `move` band shows them.
- The comment editor, the comments list, the search screen, and the find band show their own one-row footer, without `?`. The expansion's open state is kept and restored when they close.
- `?` (the `keys` action) toggles the expansion in `Normal` mode only. It is text in the comment editor and the search and find inputs, and inert in the comments list.
- The changed-file count and line totals live in the header. The footer carries only the comment count, inside `s send N`.
- On `PR` row 1 carries the PR state line and `o open ↗` per `pr-tab.md`, and `?` expands to the rest.

### Comment editor

A plain-text field that edits at the caret, not only at the end. The search input shares these controls, without the newline inserts (`search.md`). An empty box shows a dim `Leave a comment…` placeholder. `e` preloads the existing text with the caret at the end.

```
┌ comment · llm_registry.py:41 ───────────┐
│ this import path looks wrong█            │
│ and breaks on 3.12                       │
└──────────────────────────────────────────┘
```

| key                                             | does                                             |
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
- A paste outside the comment editor and the search input is ignored. It never starts or mutates a comment.
- Movement, insertion, and deletion are character-wise. Multi-byte and wide characters count as whole characters.
- `↑`/`↓` move by wrapped rows. `Home`/`End` and the kill keys act on the logical line, the run of text between explicit newlines.
- `Alt+b`/`Alt+f` always survive as ESC-prefixed sequences. The modified arrows work where the terminal delivers them. The character arrows, `Home`/`End`, and `Ctrl+A`/`Ctrl+E` always work.

## Non-goals

- No text selection, cut/copy, undo/redo, markdown rendering, or click-to-place-caret in the comment editor.
- No named-key or multi-key sequence bindings. A binding is one key, alone or with a `ctrl+`/`alt+` prefix.
- No `down` / `up` crossing at a file's edges. The line cursor clamps there.
- The `?` expansion omits the navigator-resize keys and the horizontal-diff-scroll keys. Resizing is a divider drag first, and horizontal scroll is one of the `←` / `→` keys' several meanings.

## Related specs

- [tui](./tui.md)
- [config](./config.md)
- [diff-view](./diff-view.md)
- [review-model](./review-model.md)
- [pr-tab](./pr-tab.md)
- [search](./search.md)
- [find-in-file](./find-in-file.md)
