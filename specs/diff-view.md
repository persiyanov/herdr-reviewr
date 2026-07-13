---
Status: Current
Created: 2026-06-24
Last edited: 2026-07-12
---

# Diff view

The structured diff viewer: how a file's changes are modeled and rendered with syntax highlighting, word-level emphasis, line numbers, and foldable context.

## Overview

The viewer renders a `FileDiff`: the selected file as a list of rows, built from the file's old and new content, not from parsed `git diff` text. A row is the unit the pane paints and the cursor moves over. One model serves two views. The Diff view (`Changes`) shows old versus new with change rows and folds. The File view (`All files`) shows the whole current file as `context` rows.

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

- Code is syntax-highlighted. The changed words carry a brighter background.
- The gutter is a line number plus a one-cell change bar: red on a deletion, green on an insertion, blank on context. There is no `+`/`−` glyph on screen.
- A run of unchanged lines beyond the context margin collapses to a `⋯ N unmodified lines` fold.

### FileDiff

| field           | type    | meaning                                                        |
| --------------- | ------- | --------------------------------------------------------------- |
| `path`          | string  | repo-relative path, the new path for a rename                   |
| `previous_path` | string? | the old path when renamed, absent otherwise                     |
| `state`         | enum    | `normal` shows rows, `binary` and `too_large` show a notice     |
| `view`          | enum    | `diff` shows change rows and folds, `file` shows all `context`  |
| `rows`          | Row[]   | the render-and-cursor units, in display order                   |

### Row

Content rows are selectable for comments. A `fold` is not.

| kind        | carries                       | meaning                                        |
| ----------- | ----------------------------- | ----------------------------------------------- |
| `context`   | `old_no`, `new_no`, `spans`   | an unchanged line, present in both versions     |
| `deletion`  | `old_no`, `spans`, `emphasis` | a line removed from the old version             |
| `insertion` | `new_no`, `spans`, `emphasis` | a line added in the new version                 |
| `fold`      | `hidden`                      | a collapsed run of unchanged lines, expandable  |

`spans` is the line as syntax-highlighted segments, plain when the language is unknown. `emphasis` is the character ranges that differ from the line's paired counterpart.

## Behavior

### The model

- Old content comes from `git show`, new content from the worktree (or `git show`, in the `branch` scope). An `untracked` file has empty old content. A `deleted` file has empty new content.
- Changes group into hunks with a context margin of 3 unchanged lines.
- The whole file is highlighted, not each hunk. A multi-line string or comment colors correctly inside a hunk.
- The language is detected from the path. An unknown path renders plain.
- The diff and highlighting are cached by content. A poll that finds the file unchanged recomputes nothing.

### Word emphasis

- Changed lines pair by similarity, not position. Each deletion claims the first free insertion similar enough to be the same line edited.
- An unmatched line, or a pair sharing only scraps, gets no emphasis. The row tint already marks it.
- Changed words separated only by whitespace merge into one emphasis span. A gap holding any non-space character keeps them separate.
- Emphasis never covers leading or trailing whitespace. A deepened indent paints nothing.

### File view

- The `FileDiff` is built from current content alone: every line a `context` row, no change rows, no emphasis, no folds.
- The gutter shows the new-line number and a blank change bar.
- Highlighting, wrapping, horizontal scroll, selection, and comments behave exactly as in Diff view.
- A `binary` or `too_large` file degrades to a notice, worded `file too large` here and `file too large to diff` in Diff view.

A markdown file adds a rendered preview:

- The `preview` binding (default `m`) switches between source and rendered markdown (`markdown.md`).
- A markdown file has a `.md` or `.markdown` extension, case-insensitive. A file that stops qualifying shows source.
- The pane title carries a `· preview` suffix while the preview is open.
- The preview choice holds across refreshes of the same file. Opening a file starts in source.
- The preview is read-only: selection and commenting are inactive, comment cards do not show, and there is no cursor.
- Entering the preview clears a live selection.
- `down`/`up`, the page keys, and the wheel scroll the preview by line, with no gutter. The scroll stops with the last line at the pane's bottom edge. A refresh keeps the preview's scroll, clamped the same way.
- A preview taller than the pane shows a scrollbar on the pane's right border. One that fits shows none.
- The `wrap` binding and horizontal scroll are inert in the preview.

The toggle carries the reading position, block-aligned, resolved against the painted render:

- Entering the preview opens at the block holding the source cursor's line, or the nearest block above it.
- Returning to source puts the cursor on the top visible block's first source line, revealed.
- A round-trip with no preview scroll input restores the exact source cursor and scroll. A refresh clamp is not a scroll input.
- The horizontal offset always keeps its pre-entry value.
- A forced return — the file stops qualifying — keeps the prior source position.

### Color

- The active theme (`theme.md`) supplies every color: syntax token foregrounds and the structural fills.
- The pane background stays transparent. The diff sits on the terminal's own background.
- A deletion row tints with the theme's `red`, an insertion its `green`, emphasis a stronger blend. The cursor, selection, and fold use their own surface fills.

### Folding

- An unchanged run longer than the context margin collapses to one `fold` row showing its hidden-line count.
- Leading and trailing unchanged regions fold too, so the pane opens focused on the changes.
- Expanding replaces the fold with `context` rows. There is no manual collapse-back.
- An expansion persists across refreshes of the same file. Opening another file starts collapsed. An edit that reshapes the fold may re-collapse it.
- Expanding keeps the viewport still: a fold in the top half grows upward, one in the bottom half grows downward.

### Wrapping and the gutter

- The diff is one unified column: removed lines, then added lines, full width, one gutter.
- Long lines wrap by default, at word boundaries. A word wider than the column hard-breaks. A toggle switches to horizontal scroll (`←`/`→`), with the gutter pinned.
- A wrapped continuation row has a blank gutter and drops the break's leading space.
- A commented line shows its line number in the comment color. The change bar keeps its own color.
- Tabs render as spaces, 4 by default.

### Comment anchoring

- A comment anchors as `review-model.md` defines: a `side`, a `start..end` range, the verbatim snippet.
- A selection runs over content rows. A fold is a hard boundary it cannot cross.
- The export snippet is rebuilt from the selected rows as `+`/`−`/space-prefixed lines. The markers live in the export, not on screen.

### Config

| flag              | default      | meaning                                    |
| ----------------- | ------------ | ------------------------------------------- |
| `--theme <name>`  | `catppuccin` | the theme, chrome and syntax (`theme.md`)   |
| `--wrap on\|off`  | `on`         | whether long lines wrap on open             |

## Failure semantics

The viewer is read-only and recomputed on every refresh. It degrades rather than blocks:

- A file beyond the size budget renders `too_large`, never a hang.
- A binary file renders `binary — no line comments`.
- A highlighting failure falls back to plain spans. The diff still renders.
- An empty-on-both-sides diff renders its header and a one-line notice, never a blank pane. A pure rename or mode-only change shows that content, collapsed to a fold.
- A refresh recomputes the model. Saved and in-progress comments are unaffected.

## Non-goals

- No alternate diff layouts. One unified column, a side-by-side split is roadmap.
- No editing, staging, or reverting from the viewer.
- No line-number rebasing of comments. `review-model.md` owns comment anchoring, via the snippet.

## Related specs

- [review-model](./review-model.md)
- [tui](./tui.md)
- [theme](./theme.md)
- [markdown](./markdown.md)
