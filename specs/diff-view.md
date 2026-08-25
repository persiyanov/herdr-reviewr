---
Status: Current
Created: 2026-06-24
Last edited: 2026-08-25
---

# Diff view

The structured diff viewer. The viewer shows how a file changed. The model has syntax highlight, word emphasis, line numbers, and context that you can open.

## Overview

The viewer shows a `FileDiff`. The `FileDiff` is the selected file as a list of rows. The rows are built from the old content and the new content of the file. The rows are not built from parsed `git diff` text. A row is the unit that the pane shows. The cursor moves on a row. One model serves two views.

The Diff view (`Changes`) shows old versus new. That view has change rows and folds. The File view (`All files`) shows the full current file as `context` rows. A PR finding snippet (`pr-tab.md`) uses the same content-row show. That snippet is not a FileDiff.

What the reviewer sees (unified view, a renamed TypeScript file):

```
 utils.ts → code_utils.ts
 ⋯   11 unmodified lines
 15    export function createSpanFromToken(token: ThemedToken) {
 16 ▌ const element = document.createElement('div');     ← deletion (red bar + tint)
 16 ▌ const element = document.createElement('span');    ← insertion (green bar + tint)
 17 ▌ const style = getTokenStyleObject(token);          ← deletion
 17 ▌ const style = token.htmlStyle ?? getTokenStyleObject(token);   ← insertion
 18    element.style = stringifyTokenStyle(style);
 19 ▌ element.textContent = token.content;               ← insertion
 ⋯   30 unmodified lines
```

- Code has syntax highlight. The changed words have a brighter background.
- The gutter is a line number plus a change bar of one cell. The bar is red on a deletion. The bar is green on an insertion. The bar is blank on context. There is no `+` glyph or `−` glyph that marks a change on the screen.
- When the pointer hovers a content row, a `[+]` button replaces the row's line number. It sits right-aligned, as the numbers are, in `orange`, the comment composer's accent (`theme.md`). The change bar stays. The `+` advertises the gutter comment gesture (`input.md`) and shows only where that gesture works, so a PR snippet row shows none. The whole gutter width takes the click and the drag. A continuation row's gutter belongs to its logical row. Each frame recomputes the `+` from the pointer's last reported cell, and a cell holding no commentable row shows none.
- A run of unchanged lines that is longer than the context margin becomes a `⋯ N unmodified lines` fold.

### FileDiff

| field           | type    | meaning                                                        |
| --------------- | ------- | --------------------------------------------------------------------------------- |
| `path`          | string  | path relative to the repo, the new path for a rename                              |
| `previous_path` | string? | the old path when the file was renamed, absent in other cases                     |
| `state`         | enum    | `normal` shows rows; `binary` and `too_large` show a notice; `image` a preview    |
| `view`          | enum    | `diff` shows change rows and folds, `file` shows all `context`                    |
| `rows`          | Row[]   | the show units and cursor units, in display order                                 |

### Row

You can select content rows for comments in the file tabs. You cannot select a `fold`. You cannot select the rows of a PR snippet (`pr-tab.md`).

| kind        | has                           | meaning                                          |
| ----------- | ----------------------------- | ------------------------------------------------ |
| `context`   | `old_no`, `new_no`, `spans`   | an unchanged line, present in both versions      |
| `deletion`  | `old_no`, `spans`, `emphasis` | a line removed from the old version              |
| `insertion` | `new_no`, `spans`, `emphasis` | a line added in the new version                  |
| `fold`      | `hidden`                      | a closed unchanged run that you can open         |

`spans` is the line as syntax-highlight segments. The spans are plain when the language is not known. `emphasis` is the character ranges that differ from the paired line.

## Behavior

### The model

- Old content comes from `git show`. New content comes from the worktree in `uncommitted`, `branch`, and `last-turn`. In the `commits` scope, old content comes from `A^` and new content from `B`, both through `git show` (`review-model.md`). An `untracked` file has empty old content. A `deleted` file has empty new content.
- Changes go into hunks. The context margin is 3 unchanged lines.
- The full file has highlight. Each hunk does not have its own highlight. A string or a comment that spans many lines has the correct color inside a hunk.
- The language comes from the path. A path that is not known shows as plain.
- The diff and the highlight are stored by content. A poll that finds the file unchanged does not compute again.
- A stored PR finding snippet (`pr-tab.md`) becomes content rows from its unified-diff lines. The rows have highlight from the path. The rows have word emphasis from the same pairing. The snippet keeps a three-line margin around the comment range. The snippet is not a FileDiff. The snippet has no folds. The snippet has no highlight of the full file.

### Word emphasis

- Changed lines pair by how similar they are. Changed lines do not pair by position. Each deletion takes the first free insertion that is similar enough to be the same line after an edit.
- A line with no pair gets no emphasis. A pair that shares only scraps gets no emphasis. The row tint already marks the change.
- Changed words that have only whitespace between them join into one emphasis span. A gap that has a character that is not a space keeps them separate.
- Emphasis does not cover whitespace at the start. Emphasis does not cover whitespace at the end. A deeper indent shows nothing.

### File view

- The `FileDiff` is built from current content only. Each line is a `context` row. There are no change rows. There is no emphasis. There are no folds.
- The gutter shows the new-line number and a blank change bar.
- Highlight, wrap, horizontal scroll, selection, and comments operate as they operate in Diff view.
- A `binary` file or a `too_large` file goes to a notice. The File view notice is `file too large`. The Diff view notice is `file too large to diff`.

### Image preview

In the Changes Diff view, a file whose current or previous bytes decode as a supported image paints a visual preview instead of rows:

- Classification reads raw bytes (never UTF-8 lossy). A successful image decode wins over the NUL binary rule. Other NUL-bearing files stay `binary`.
- The pane fits the image into its inner area, keeps aspect ratio, and centers it. There is no image scroll and no line comments.
- Selection, wrap, horizontal scroll, and the markdown preview toggle are inert.
- A decode or protocol failure paints `could not display image`, never a blank pane and never a panic.
- The pane always encodes through the unicode halfblocks fallback. A later change may probe Kitty or Sixel without stealing input on a silent terminal. Non-image binaries still paint `binary — no line comments`.
- Side-by-side before/after for a modified image is out of scope here.

### Markdown preview

A markdown file adds a shown preview. The preview is in both views.

- The `preview` key (default `m`) switches between source and shown markdown (`markdown.md`).
- A markdown file has a `.md` extension or a `.markdown` extension. Letter case does not matter.
- The preview shows the current content of the file. A `deleted` file never has a preview.
- The preview needs source rows. A notice never has a preview. An empty file never has a preview.
- If a file stops the preview, an open preview goes back to source. The toggle does not operate. This is a forced return. The file stops the preview when it is renamed away, deleted, or degraded.
- The pane title has a `· preview` suffix while the preview is open.
- The preview choice stays across refreshes of the same file. Opening a file starts in source.
- The preview only reads. Line selection does not operate. Text selection operates (`text-selection.md`). Comments do not operate. Comment cards do not show. There is no cursor.
- When the preview opens, a live selection is cleared.
- `down`, `up`, the page keys, and the wheel scroll the preview by line. There is no gutter. The scroll stops when the last line is at the bottom edge of the pane. A refresh keeps the preview scroll. The tab sets the scroll to a valid position in the same way.
- If a preview is higher than the pane, a scrollbar shows on the right border of the pane. If the preview fits, there is no scrollbar.
- The `wrap` key does not operate in the preview. Horizontal scroll does not operate in the preview.

When the preview opens, the read position stays. The position is aligned to a block. The block is the block in the shown render.

- The preview opens at the block that has the current-content line of the cursor. If that block is not there, the preview opens at the nearest block above.
- In Diff view, a row with no current-content line uses the nearest row above that has one. A deletion has no current-content line. A fold has no current-content line.
- If there is no current-content line at the cursor or above the cursor, the preview opens at its top.
- The horizontal offset always keeps the value that it had before entry.

Return to source is different per view.

- In Diff view, a return does not move the cursor, the scroll, or the folds. This holds for a forced return also.
- In File view, a return puts the cursor on the first source line of the top block that is in view. That line is shown.
- In File view, a round trip with no preview scroll input restores the exact source cursor and the exact source scroll. A refresh that sets a valid scroll is not a scroll input.
- In File view, a forced return keeps the source position from before.

### Color

- The active theme (`theme.md`) gives each color. The colors are the syntax token foregrounds and the structural fills.
- The pane background stays transparent. The diff sits on the background of the terminal.
- A deletion row has a tint of the theme `red`. An insertion row has a tint of the theme `green`. Emphasis is a stronger blend. The cursor, the selection, and the fold use their own surface fills.

### Folding

- An unchanged run that is longer than the context margin becomes one `fold` row. The row shows the count of hidden lines.
- A leading unchanged region also folds. A trailing unchanged region also folds. The pane opens on the changes.
- When you open a fold, the fold becomes `context` rows. You cannot close the fold again by hand.
- An open fold stays across refreshes of the same file. Opening a different file starts with folds closed. An edit that changes the shape of the fold can close the fold again.
- When you open a fold, the viewport stays. A fold in the top half grows up. A fold in the bottom half grows down.

### Wrapping and the gutter

- The diff is one unified column. Removed lines come first. Then added lines come. The column uses the full width. There is one gutter.
- Long lines wrap by default. The wrap is at word boundaries. A word that is wider than the column breaks. A toggle switches to horizontal scroll (`←` / `→`). The gutter stays. A PR snippet always wraps (`pr-tab.md`). The wrap toggle does not apply to that snippet.
- A wrapped continuation row has a blank gutter. That row drops the leading space of the break.
- A commented line shows its line number in the comment color. The change bar keeps its own color.
- Tabs show as spaces. The default is 4 spaces.

### Comment anchoring

- A comment anchors as `review-model.md` defines. The comment has a `side`, a `start..end` range, and the verbatim snippet.
- A selection runs on content rows. A fold is a hard limit. The selection cannot cross a fold.
- A selection covers the same rows from either end. An extend up over the same rows and an extend down over the same rows anchor the same comment. The input box opens under the last line of the range in both cases (`tui.md`).
- The export snippet is built again from the selected rows. Each line has a `+` prefix, a `−` prefix, or a space prefix. The marks are in the export. The marks are not on the screen.

### Config

| flag             | default      | meaning                                   |
| ---------------- | ------------ | ----------------------------------------- |
| `--theme <name>` | `catppuccin` | the theme, chrome and syntax (`theme.md`) |
| `--wrap on\|off` | `on`         | whether long lines wrap on open           |

## Failure semantics

The viewer only reads. The viewer is computed again on each refresh. The viewer degrades. The viewer does not block.

- A file over the size budget shows `too_large`. The viewer does not hang.
- A binary file that is not a supported image shows `binary — no line comments`.
- An image that fails to load or encode shows `could not display image`.
- If highlight fails, the viewer uses plain spans. The diff still shows.
- A diff that is empty on both sides shows its header and a notice of one line. The pane is not empty. A rename only, or a mode-only change, shows that content. The content is closed to a fold.
- A refresh computes the model again. Saved comments do not change. A comment that is in progress does not change.

## Non-goals

- There is no other diff layout. There is one unified column. A side-by-side split is on the roadmap.
- There is no before/after image split. A later change may add it without reshaping the text diff model.
- The viewer does not edit. The viewer does not stage. The viewer does not revert.
- The viewer does not move comment line numbers again. `review-model.md` owns comment anchors, through the snippet.

## Related specs

- [review-model](./review-model.md)
- [tui](./tui.md)
- [theme](./theme.md)
- [markdown](./markdown.md)
- [find-in-file](./find-in-file.md)
- [pr-tab](./pr-tab.md)
- [text-selection](./text-selection.md)
