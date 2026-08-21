---
Status: Current
Created: 2026-07-17
Last edited: 2026-08-19
---

# Input

Driving the review: the keymap, the changeset traversal, the live footer, and the comment editor.

## Overview

Every action has a key. The mouse-relevant ones also work by click or drag.

The keymap is rebindable per action through `[keybindings]` in the plugin config (`config.md`):

- The `action` column names the action for `[keybindings]`.
- The keys shown are defaults: a bare character, a named key, or a `ctrl+`/`alt+` chord (`config.md`).
- `tab`, `esc`, and `enter` are structural. They are fixed and never rebind.
- A key hint in the header or the footer shows its action's first bound key.
- A named key spells as its name in the config and paints as its screen label: `←`, `→`, `↑`, `↓`, `PageUp`, `PageDown`.
- The comments list acts through the same bindings and closes on `esc` and the `comments` binding.
- The agent picker acts through the `down` / `up` bindings and closes on `esc`.
- The base picker filters through a text field with the comment editor's controls, moves through the arrows, and closes on `esc`. Its keys are fixed like every text field's, whatever `down` / `up` are bound to.
- Prose and mockups elsewhere show the default keys.

| action                                                   | does                                        | keys                                        | mouse                                          |
| -------------------------------------------------------- | ------------------------------------------- | ------------------------------------------- | ---------------------------------------------- |
| `down` / `up`                                            | move the cursor in the focused pane         | `j` / `k` / `↓` / `↑`                       | click a row                                    |
| `next-hunk` / `prev-hunk`                                | jump to the next / previous hunk            | `]` / `[`                                   | —                                              |
| `next-file` / `prev-file`                                | jump to the next / previous file            | `f` / `F`                                   | —                                              |
| `collapse` / `expand`                                    | collapse / expand, else scroll sideways     | `←` / `→`                                   | click the directory or `⋯` row                 |
| —                                                        | switch focus between list and diff          | `tab`                                       | click a pane                                   |
| `page-up` / `page-down`                                  | move a page                                 | `PageUp` / `PageDown`                       | —                                              |
| `half-up` / `half-down`                                  | move a half page                            | `ctrl+u` / `ctrl+d`                         | —                                              |
| —                                                        | scroll the viewport, selection put          | —                                           | wheel over the pane                            |
| `scope-uncommitted` / `scope-branch` / `scope-last-turn` | switch scope                                | `u` / `b` / `t`                             | click the scope chip to cycle                  |
| `base-pick`                                              | open the base picker                        | `B`                                         | click the base name                            |
| `tab-changes` / `tab-all-files` / `tab-pr`               | switch tab                                  | `1` / `2` / `3`                             | click a tab name                               |
| —                                                        | open a link in rendered markdown            | —                                           | click the link                                 |
| `wrap`                                                   | toggle line wrap                            | `w`                                         | —                                              |
| `preview`                                                | toggle the markdown preview                 | `m`                                         | —                                              |
| `navigator-position`                                     | move the navigator clockwise                | `p`                                         | —                                              |
| `navigator-hide`                                         | hide / show the navigator                   | `z`                                         | —                                              |
| `navigator-grow` / `navigator-shrink`                    | grow / shrink the navigator                 | `<` / `>`                                   | drag the divider                               |
| `select`                                                 | select a line range, removed lines included | `v` then move                               | —                                              |
| —                                                        | clear the selection                         | `esc`                                       | —                                              |
| `comment`                                                | comment on the selection                    | `c`, type, `enter`                          | click or drag the gutter                       |
| —                                                        | copy text (`text-selection.md`)             | —                                           | drag over text                                 |
| `edit`                                                   | edit the comment under the cursor           | `e`                                         | —                                              |
| `edit-file`                                              | open the file under the cursor in `$EDITOR` | `ctrl+e`                                    | —                                              |
| `delete`                                                 | delete the comment under the cursor         | `d`                                         | —                                              |
| `next-comment` / `prev-comment`                          | jump to next / previous comment             | `n` / `N`                                   | —                                              |
| `comments`                                               | list and manage all comments                | `l`                                         | —                                              |
| `search`                                                 | open the search screen (`search.md`)        | `/`                                         | —                                              |
| `find`                                                   | open in-file find (`find-in-file.md`)       | `ctrl+f`                                    | —                                              |
| `keys`                                                   | toggle the footer's full shortcut list      | `?`                                         | —                                              |
| `send`                                                   | send all comments to the agent              | `s` / `S`                                   | —                                              |
| `copy`                                                   | copy all comments to the clipboard          | `y` / `Y`                                   | —                                              |
| `open-pr`                                                | open the PR in the browser (`pr-tab.md`)    | `o`                                         | click the status chip                          |
| `refresh`                                                | refresh now                                 | `r`                                         | —                                              |
| `quit`                                                   | quit                                        | `q`                                         | —                                              |

`navigator-position` cycles `right` → `bottom` → `left` → `top` → `right`.

`navigator-grow` and `navigator-shrink` change the active share by four percentage points. The allowed range clamps every change.

`navigator-hide` hides the navigator and shows it again in place (`tui.md`). On the file tabs it is never inert in `Normal` mode, so the way back is always the key that hid it. On `PR` it is inert and stays out of the footer (`tui.md`). While the navigator is hidden, `navigator-position`, `navigator-grow`, and `navigator-shrink` are inert. In `Normal` mode, `tab` then shows the navigator and focuses it. Every other mode keeps its own `tab` meaning (`search.md`).

Outside the hidden-state rules above, these four navigator actions work from either main pane on every tab. While the comment editor is open, their printable characters are text. In the comments list, the agent picker, and the base picker they are inert. Those local modes omit the navigator actions from the footer.

A divider drag belongs to the navigator position and split axis at mouse-down. A keypress, terminal resize, or config-driven layout change cancels it. A cancelled drag keeps its last painted share, and the cancelling keypress still performs its own action. After cancellation, drag events are consumed until mouse-up rather than becoming a selection in the read pane.

Writing a comment: select a range or land on a line, press `c`, type into the inline box, `enter` saves and `esc` cancels. The mouse path is the gutter, advertised by its hover `+` (`diff-view.md`). A gutter click opens the box on that line, acting as `c` there. A gutter drag selects the spanned range and opens the box on release. A gutter drag follows the line selection's rules: content rows only, never across a fold. It scrolls at the pane's edge and under the wheel like a text drag, and it ends by the text gesture's end table (`text-selection.md`). While the comment editor is open, the gutter is inert, like the base-name click. A saved comment renders as a read-only card spliced under its line, titled with its location, so written feedback stays on screen. `e` reopens the card as an edit box in place, hiding the card while editing. `d` deletes it. A successful send names the agent it added the comments to. A successful copy reports that they were copied. The transient status shows on the footer, pluralizes `comment`, and fades without covering the primary action.

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
- Both are inert while a line selection is live and while the comments list, the agent picker, or the base picker is open.
- The `PR` tab has neither.

### Expand and collapse

`expand` and `collapse` act on the collapsible under the cursor, and scroll the diff sideways from anywhere else. A rebind moves the whole bundle.

- On a directory row in the focused file list, `expand` shows its children and `collapse` hides them.
- On a fold in the diff, `expand` opens it. An open fold never closes again, so `collapse` scrolls there.
- Elsewhere, `expand` scrolls the diff right and `collapse` scrolls it left. The scroll is inert while wrap is on.

### Edit file

`edit-file` opens the file the cursor names in the user's editor, so a fix spotted mid-review
is one keypress away. The target follows the press:

- In the read pane — the `Changes` diff, an `All files` File view, or a markdown preview —
  the open file at the line under the cursor. A deletion or fold row carries no new-file
  number, so the nearest numbered row above it names the line.
- On a file row in the navigator, that file, without a line. A directory row is inert.
- In the comments list, the highlighted comment's file at its anchor start. An old-side
  anchor numbers revision lines rather than worktree lines, so it opens without one.

The comment editor, the find band, the agent picker, and the base picker are text fields and
strictly modal: every printable is theirs, so `edit-file` never fires there. Its default is
a chord — `ctrl+e` — so it also reaches the search screen, where every printable types into
the query: there it opens the picked result's file straight from the screen, outranking the
shared caret-to-end control on that screen alone (`search.md`); `End` still moves the caret.
The `PR` tab has no files to name and stays inert.

The pane suspends — alternate screen, raw mode, mouse capture, bracketed paste, and the
keyboard protocol all release — so the editor owns the terminal cleanly, then rebuilds the
same mode stack on return. The editor is `$VISUAL` or `$EDITOR`, run through the host PATH;
its whole value is the command, so `code -w` works. When a line is known it passes as
`+LINE` before the path, the convention vim, nvim, nano, hx, and kakoune share; editors
without it open at their own position. With neither variable set, the press reports what to
set instead of failing silently.

A successful edit refreshes the changeset, so the diff reconciles to the edited file in place
(`overview.md` Continuity) — reviewr itself still never writes (`overview.md`). The status
names the edited file; a nonzero exit or a failed launch reports it instead.

### Footer

The footer is one row: the primary next step, the cursor's own actions, and `send` once a comment
exists, closing with a `?`. Pressing `?` expands it to every shortcut that works here, and it stays
until `?` or `esc`. It never lists a key that would not work in the current state.

```
 e edit · d delete · n/N jump · s send 2                                      ?
```

Opening it turns the one-row action bar into a labeled grid. Row 1 becomes the `do` band under a
dim `do` label: the primary and the cursor's actions. Two bands follow it, `go` (the global
actions) and `move` (cursor movement). Every band's content aligns in one column. The `?` stays at the
right of the `do` row.

```
 do    e edit · d delete · n/N jump · s send 2                                ?
 go    u/b/t scope · / search · ctrl+f find · w wrap · l list · y copy · r refresh · 1·2·3 tabs
       tab files · p position · z hide · q quit
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
| status  | the transient message, after `send`, truncated to fit                             |
| actions | the cursor's other actions, in normal text, trimmed to fit                        |
| more    | a `?` at the right, muted but legible — always present, and expands the rest      |

A narrow row drops trailing actions to fit. The status outranks them, since the `?` panel repeats
every action and nothing repeats the status. A 40-column reviewr pane shows `no agent here …` rather than
nothing. The status drops only below a legible width. The primary, `send`, and the `?` never drop. On
a pane too narrow even for those, the primary truncates before the `?` does.

The `?` expansion:

- It lists every shortcut applicable in the current context that is not already on row 1, wrapped
  below row 1 in three labeled bands, each a dim label then its keys. `do`: the cursor's actions.
  `go`: the global actions, each shown only where it works — scope, the base picker, search, find,
  wrap, the comments list, copy, refresh, the tabs, the pane toggle, the navigator position and hide
  keys, quit. `move`:
  down and up, the hunk and file steps, the page keys. An empty band is dropped, and a key that would
  not work in the current state never appears. The hunk step shows only where it works, the `Changes`
  diff and never a preview (see Changeset traversal). `PR` has no hunk or file steps. The hide key's
  hint reads `hide` while the navigator shows and `show` while hidden.
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
| a commented line                         | `e edit`                       | `d delete · n/N jump`             |
| a fold                                   | `→ expand fold`                | —                                 |
| an open markdown preview                 | `m source`                     | —                                 |
| a file (file list)                       | `tab diff`                     | `z hide`                          |
| a collapsed directory                    | `→ expand`                     | `z hide`                          |
| an expanded directory                    | `← collapse`                   | `z hide`                          |
| nothing to review (awaiting turn)        | `u/b/t scope`                  | `r refresh`                       |
| the `branch` scope with no base          | `B pick base`                  | `u/t scope · r refresh`           |
| an empty read pane, navigator hidden     | `z show`                       | `tab files`                       |

- An armed crossing outranks the cursor's own action and leads row 1, since only the footer says the next press leaves the file. It is the one movement key on row 1 (see Changeset traversal). While it is armed, the `move` band drops the hunk step, whose key row 1 now shows.
- While the navigator is hidden, `z show` joins row 1's actions, so the collapsed footer always names the way back. Visible, `z hide` joins them only while the file list holds focus, whose row 1 has the room. Elsewhere it waits in the `go` band.
- When the awaiting-turn state and the hidden empty read pane match at once, the awaiting-turn row wins, and `z show` still joins its actions.
- `scope`, `search`, and `find` are global, not cursor actions, so the `go` band carries them, never row 1 — `search` in every context, `find` wherever the read pane has content (`search.md`, `find-in-file.md`). `scope` leads row 1 only where nothing else does, an empty or notice diff.
- Movement keys never sit on row 1. The `move` band shows them.
- The comment editor, the comments list, the agent picker, the base picker, the search screen, and the find band show their own one-row footer, without `?`. The expansion's open state is kept and restored when they close.
- `?` (the `keys` action) toggles the expansion in `Normal` mode only. It is text in the comment editor, the search and find inputs, and the base picker's filter, and inert in the comments list and the agent picker.
- The changed-file count and line totals live in the header. The footer carries only the comment count, inside `s send N`.
- On `PR` row 1 carries the PR state line and `o open ↗` per `pr-tab.md`, and `?` expands to the rest.

### Comment editor

A plain-text field that edits at the caret, not only at the end. The search input and the base picker's filter share these controls, without the newline inserts (`search.md`). An empty box shows a dim `Leave a comment…` placeholder. `e` preloads the existing text with the caret at the end.

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
- A paste outside the comment editor, the search input, the find input, and the base picker's filter is ignored. It never starts or mutates a comment.
- Movement, insertion, and deletion are character-wise. Multi-byte and wide characters count as whole characters.
- The terminal cursor follows every field's caret in display cells, anchoring an IME candidate
  window after wide characters.
- The painted block caret covers the character under the insertion point. With no character under
  it, at the end of a logical line or of the input, the terminal cursor alone marks the insertion
  point.
- `↑`/`↓` move by wrapped rows. `Home`/`End` and the kill keys act on the logical line, the run of text between explicit newlines.
- A box too short for its wrapped rows scrolls to keep the caret row visible.
- A caret past an exactly-full row sits on the next row's first cell, where the next character
  lands. Input that ends by exactly filling its last row keeps an empty continuation row for it.
- `Alt+b`/`Alt+f` always survive as ESC-prefixed sequences. The modified arrows work where the terminal delivers them. The character arrows, `Home`/`End`, and `Ctrl+A`/`Ctrl+E` always work.

### Agent picker

`Send` opens the picker (`herdr-host.md`). Every key below acts, and every other key is inert. The movement row shows the `down` / `up` defaults and follows a rebind. The digits, `enter`, and `esc` are literal.

| key                     | does                                        |
| ----------------------- | ------------------------------------------- |
| `j` / `k` / `↓` / `↑`   | move the highlight                          |
| `1` – `9`               | move the highlight to that row              |
| `enter`                 | send every comment to the highlighted agent |
| `esc`                   | cancel, keeping every comment               |

Only the unmodified `enter` sends. `Alt+Enter` and `Shift+Enter` insert a newline in the comment editor, so carrying that chord into the picker sends nothing rather than handing the whole review to the armed agent.

- A click moves the highlight to the clicked row. A click on the highlighted row sends. The highlight is armed when the picker opens, so a first click on the armed row sends immediately. Every other gesture is inert, and none reaches the view behind.
- The digits are literal here, whatever `tab-changes` and its siblings are bound to. A modified digit moves nothing.
- `esc` cancels with or without a modifier, so no keystroke traps the reviewer in the picker.
- A picker taller than the pane scrolls with the highlight.
- `esc` returns to the view the send was issued from, the comments list and the find band included.

### Base picker

`base-pick` opens the picker over the body, like the comments list (`tui.md`). It works on the file tabs while the `branch` scope is active and no `--base` flag was passed. Elsewhere it is inert and stays out of the footer. While the comment editor is open, the base-name click is inert like the key. The picker still lists branches. It also opens when that list is empty, so a revision can be typed.

The list holds one row per branch name, remote-tracking and local names merged. The checked-out branch is not listed, unless it is the default branch, whose row must stay reachable to clear a pick. Rows sort by most recent commit. Two rows outrank that order:

- The open PR's target sorts first, starred (`forge-host.md`).
- The default branch sorts next, marked `default`. Choosing it clears the pick (`review-model.md`). Choosing a revision that resolves to the same commit does not.

When the current base is a spelling that is not a listed branch name, that spelling is a row. The highlight opens on it. Otherwise the highlight opens on the current base among the branch rows, else the first row. The highlight is place state (`overview.md`).

The query is checked as a commit only when the filter matches no row, and only after typing pauses. A unique abbreviated SHA, a tag, and `HEAD~1` all resolve. An ambiguous SHA prefix does not. A unique SHA prefix shorter than the abbreviated object id completes to that abbreviation — the row is `a1b2c3d`, not the typed prefix. A full object id, an already-abbreviated SHA, a tag, and `HEAD~1` keep the typed spelling. The highlight sits on that row. Choosing it stores that name (`review-model.md`).

A non-branch row is marked `(a1b2c3d)`. A SHA spelling paints as that abbreviated SHA and carries no marker. The marker is not a separate target. A click picks the spelling.

| key                 | does                                                                 |
| ------------------- | -------------------------------------------------------------------- |
| typed character     | narrow the list, matching anywhere in the name                       |
| `↓` / `↑`           | move the highlight                                                   |
| `ctrl+n` / `ctrl+p` | move the highlight                                                   |
| `enter`             | pick the highlighted row. With no row, check the query and pick it   |
| `esc`               | cancel                                                               |

The filter is a text field with the comment editor's controls, above. `↑` and `↓` move the highlight, so the single-line filter keeps `←` and `→` for its caret. A pasted newline drops, so a name copied with its line ending filters as the bare name.

- A click moves the highlight. A click on the highlighted row picks. Every other gesture is inert, and none reaches the view behind.
- An empty filter shows a dim `filter or type a revision` placeholder.
- A filter matching no row shows `no branches match` once the query has been checked and is not a commit. Enter does nothing.
- Enter with no highlighted row checks the query immediately. A resolving spelling is recorded. A miss stays on `no branches match`.
- An empty query with no rows shows `type a revision`, and `enter` does nothing.
- A pick applies immediately: the changeset rebuilds and the header renames (`review-model.md`).
- A picker taller than the pane scrolls with the highlight.

## Non-goals

- No text selection (`text-selection.md`), cut/copy, undo/redo, markdown rendering, or click-to-place-caret in the comment editor.
- No multi-key sequence bindings. A binding is one key, alone or with a `ctrl+`/`alt+` prefix.
- No `down` / `up` crossing at a file's edges. The line cursor clamps there.
- The `?` expansion omits the navigator-resize keys, the half-page keys, and the `expand` / `collapse` keys. Resizing is a divider drag first, and the fold and scroll meanings are contextual row-1 actions.
- The base picker does not list tags, reflog entries, or recent commits. A revision that is not a branch is named by typing it.

## Related specs

- [tui](./tui.md)
- [config](./config.md)
- [diff-view](./diff-view.md)
- [review-model](./review-model.md)
- [pr-tab](./pr-tab.md)
- [search](./search.md)
- [find-in-file](./find-in-file.md)
- [text-selection](./text-selection.md)
