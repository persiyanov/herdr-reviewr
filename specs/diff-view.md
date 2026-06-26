---
Status: Current
Created: 2026-06-24
Last edited: 2026-06-26
---

# Diff view

The structured diff viewer: how a file's changes are modeled from its content and rendered with syntax highlighting, word-level emphasis, line numbers, and foldable context.

## Overview

The viewer renders a `FileDiff` — the selected file modeled as a list of rows, built from the file's old and new content (not from parsed `git diff` text). A row is the unit the diff pane paints and the cursor moves over. The pane renders the same `FileDiff` in two views: the **Diff view** (`Changes`) shows old-versus-new with change rows and folds; the **File view** (`All files`) shows the whole current file as `context` rows.

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

- Code is syntax-highlighted; the changed words (`'div'`→`'span'`, the inserted `token.htmlStyle ??`) carry a brighter background.
- The gutter is a line number plus a one-cell change bar (`▌`): red on a deletion, green on an insertion, blank on context. The bar and the row tint mark the change — there is no `+`/`−` glyph.
- A run of unchanged lines beyond the context margin collapses to a `⋯ N unmodified lines` fold the reviewer can expand.

### FileDiff

| field | type | meaning |
|-------|------|---------|
| `path` | string | Repo-relative path; the new path for a rename. |
| `previous_path` | string? | The old path when the file was renamed; absent otherwise. |
| `state` | enum | `normal` shows rows; `binary` and `too_large` show a notice instead. |
| `view` | enum | `diff` shows change rows and folds; `file` shows every line as `context`, no folds. |
| `rows` | Row[] | The render-and-cursor units, in display order. |

### Row

A row is one of four kinds. Content rows (`context`, `deletion`, `insertion`) are selectable for comments; a `fold` is not.

| kind | carries | meaning |
|------|---------|---------|
| `context` | `old_no`, `new_no`, `spans` | An unchanged line shown in both versions. |
| `deletion` | `old_no`, `spans`, `emphasis` | A line removed from the old version. |
| `insertion` | `new_no`, `spans`, `emphasis` | A line added in the new version. |
| `fold` | `hidden` | A collapsed run of `hidden` unchanged lines, expandable in place. |

- `spans` — the line's content as syntax-highlighted segments, each a `(text, color)` from highlighting the **whole** file; plain one-segment text when `language` is absent.
- `emphasis` — the character ranges within the line that differ from its paired counterpart, rendered with a brighter background; empty when the line has no close pair.

## Behavior

### Building the model

- Content comes from git: the old version via `git show <rev>:<path>`, the new version from the worktree (or `git show` for branch scope). An `untracked` file has empty old content; a `deleted` file has empty new content.
- The diff is computed with the `similar` crate (`TextDiff::from_lines`) over old vs new content, grouped into hunks with a context margin of 3 lines.
- `emphasis` comes from `similar`'s inline word-level diff over related lines within a change block (a run of deletions then a run of insertions). Lines are matched by **homolog search**, not position (after git-delta): each deletion claims the first not-yet-taken insertion similar enough to be the same line edited; skipped insertions and unmatched deletions stay plain. A pair below the similarity bar — a wholesale rewrite sharing only scraps like indentation or `///` — gets no emphasis at all, since the line-level red/green already carries it and full-line highlighting would be noise. Adjacent changed words separated only by whitespace coalesce into one span (the whitespace is swallowed), so a changed phrase highlights as one block, not fragments; gaps holding any non-space character keep the words separate. Each span is then trimmed to its tokens — leading and trailing whitespace is never highlighted, so a deepened indent or the space before an added trailing comment paints nothing.
- Highlighting comes from `syntect` over the broad bat/`two-face` syntax and theme set, so most languages color out of the box: the language is detected from the path (absent when unknown, which renders plain), the full old and new content are highlighted once each, and every row reads its line's `spans`. Full-file highlighting is why a multi-line string or comment colors correctly inside a hunk.
- The diff and the highlighting are both cached per file by content; a poll that finds the file unchanged reuses the prior rows and spans rather than recomputing.

### File view

- The `All files` tab renders a file in File view: the `FileDiff` is built from the current content alone, every line a `context` row, with no deletions, insertions, `emphasis`, or folds.
- The gutter shows the single new-line number and a blank change bar; highlighting, wrapping, horizontal scroll, line selection, and comments behave exactly as in Diff view.
- Folding is off, so the whole file is shown; a `binary` or `too_large` file degrades to a notice as it does in Diff view (the `too_large` wording differs: `file too large` here, `file too large to diff` there).

### Color

- The pane targets a dark terminal; the default theme is Catppuccin Mocha, with `--theme` to override. Catppuccin is a common terminal palette, so the diff and the shell share one set of colors.
- Syntax `spans` take only foreground token colors from the theme; the pane background stays transparent, so the diff sits on the terminal's own background.
- The structural fills draw from the same Catppuccin palette: a deletion row tints with its red, an insertion its green, `emphasis` a brighter shade, and the cursor and selection their own fills — so highlight and syntax never clash.

### Folding

- An unchanged run longer than the context margin collapses to one `fold` row showing its hidden-line count, drawn as a distinct band so it reads as a hunk separator.
- Expanding a `fold` replaces it with its lines as `context` rows; there is no manual collapse-back, and the expansion persists across refresh polls of the same file. It is per file (opening another file starts collapsed), and an edit that reshapes the fold may re-collapse it.
- Expanding keeps the viewport visually still: a fold in the top half of the diff grows upward, so the lines below it hold their screen position; a fold in the bottom half grows downward, so the lines above it hold theirs.
- A file's leading and trailing unchanged regions fold the same way, so the pane opens focused on the changes.

### Wrapping and the gutter

- The diff is one unified column: each change block shows its removed lines then its added lines, full width, with a single line-number gutter.
- Long lines wrap by default, breaking at word boundaries (a space that fits); a word wider than the column hard-breaks. A toggle switches to horizontal scroll, moved with `←`/`→` while the gutter stays pinned. A wrapped continuation row has a blank gutter so numbers stay aligned, and drops the break's leading space so it never starts almost-empty.
- The gutter is a fixed line-number column plus the one-cell change bar.
- A line that carries a comment shows its line number in the comment color, so the change bar keeps its add/remove color and the two never collide.
- Tabs render as spaces (4 by default) so code and the gutter stay aligned.

### Comment anchoring

- A comment anchors to the diff exactly as `review-model.md` defines it: a `side` plus a `start..end` line range, with the verbatim snippet.
- A selection runs over content rows; a fold is a hard boundary it cannot cross, so the `start..end` range never brackets hidden lines the snippet would omit. The export snippet is reconstructed from the selected rows as `+`/`−`/space-prefixed lines — the markers live in the snippet sent to the agent, not on screen.

### Config

Presentation flags, each with a default:

- `--theme <name>` — the syntect theme for syntax colors.
- `--wrap on|off` — whether long lines wrap on open.

## Failure semantics

The viewer is read-only and recomputed on every refresh, so it never persists or double-applies. It degrades rather than blocks:

- A file beyond the size budget renders as `too_large` with a notice — never a hang while diffing or highlighting.
- A `binary` file renders `binary — no line comments`.
- A highlighting failure (unknown language, grammar error) falls back to plain `spans`; the diff still renders.
- A diff with no rows at all — an empty file on both sides — renders its header and a one-line notice, not a blank pane. A pure rename or mode-only change (identical content) shows that content, collapsed to a fold.
- A refresh recomputes the model from current content; the line numbers a saved or in-progress comment anchors to are unaffected, so no comment is lost or re-bound.

## Non-goals

- No alternate diff layouts — one unified column only; a side-by-side split is roadmap.
- No tree-sitter highlighting — syntect only.
- No editing, staging, or reverting from the viewer.
- No line-number rebasing of comments as the diff shifts — `review-model.md` owns that, via the snippet.

## Decisions

- Model from file content, not parsed `git diff` text — text carries no syntax context and no lines to expand; the old/new content carries both. Rejected: keep parsing unified-diff text.
- `similar` for the diff and the inline word emphasis — one pure-Rust crate does line grouping and word-level emphasis. Rejected: `git2`/libgit2, which adds a C dependency for convenience the crate already provides.
- `syntect` over tree-sitter — one mature crate, ~200 bundled languages, line spans that map straight to ratatui. Rejected: tree-sitter, which needs a grammar crate and highlight query per language.
- Truecolor syntax theme, current structural colors kept — the add/remove tint, change bars, and selection stay as they are; only syntax token colors come from the theme. Rejected: mapping syntax onto the terminal's 16 ANSI colors, which is less rich.
- Change bar and tint, not `+`/`−` glyphs — the colored bar plus row tint already mark add versus remove, so the glyphs are redundant on screen. The export snippet keeps `+`/`−`/space markers for the agent. Rejected: showing both.
- Catppuccin Mocha as the default theme — a cohesive dark palette that matches a Catppuccin terminal, so the diff blends with the shell instead of importing foreign colors. Rejected: a generic bundled theme that clashes with the terminal.
- File view is an all-`context` `FileDiff`, not a separate pager — the whole-file browser reuses the diff model, gutter, highlighting, selection, and comment machinery by modeling the file as `context` rows with folding off. Rejected: a second viewer component.
- Folding off in File view — an unchanged file would collapse to a single fold; `All files` exists to read the file, so it shows every line. Rejected: reusing the context-margin folding.

## Open decisions

- None.

## Related specs

- `./review-model.md`
- `./tui.md`
