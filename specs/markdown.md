---
Status: Current
Created: 2026-07-12
Last edited: 2026-07-12
---

# Markdown rendering

How markdown text renders as styled terminal lines: one renderer behind the PR tab's bodies and the File view's preview.

## Overview

A comment body, rendered:

```
 Fix the fallback loop                            ← ## heading: bold, in an accent
 The retry loop never exits early:                ← **never** renders bold
   if attempts > MAX {                            ← ```rust fence, highlighted like the diff pane
       break;
   }
 See the failing run (https://ci.example/8123)    ← [text](url): accent text, dim destination
```

The PR tab's description and comment bodies (`tui.md`) and the File view's preview of a markdown file (`diff-view.md`) render through it.

| element                       | renders as                                                                     |
| ----------------------------- | ------------------------------------------------------------------------------ |
| paragraph                     | wrapped text, one blank line between blocks                                    |
| heading                       | bold text in an accent, deeper levels dimmer, `#` markers removed              |
| bold / italic / strikethrough | the matching terminal attribute                                                |
| inline code                   | a distinct code tint, backticks removed                                        |
| fenced code block             | syntax-highlighted lines, indented                                             |
| indented code block           | plain code lines, indented                                                     |
| block quote                   | a dim quote-bar prefix, one bar per nesting level                              |
| list item                     | a `•` or `1.` marker, one indent step per nesting level                        |
| task-list item                | a `☐` or `☑` marker                                                            |
| link                          | its text underlined in an accent, the destination appended dim when it differs |
| image                         | a dim `⧉ alt-text` placeholder                                                 |
| table                         | aligned columns, a bold header row, dim rules                                  |
| thematic break                | a dim rule across the pane                                                     |
| raw HTML                      | its source text, dim                                                           |
| footnote syntax               | its source text, or a reference link once a definition names it                |

## Behavior

### Color and code

- Every color comes from the active theme's palette (`theme.md`).
- A fenced block highlights through the same highlighter and syntax theme as the diff panes (`diff-view.md`).
- The language comes from the fence's info string. An unknown or absent language renders plain.

### Layout

- Lines wrap at the pane width, at word boundaries. A word wider than the pane hard-breaks.
- A wrapped continuation hangs under its block's content: list text aligns under list text, quoted text keeps its bars.
- A soft break renders as a space. A hard break starts a new line.
- Width is measured in terminal display cells.
- Nesting indents cap at 8 levels. A deeper level renders at the cap.
- A table's columns size to their widest cell. A table wider than the pane renders as its source text.

### Links

- Clicking a link opens its destination in the browser.
- The click target spans the link text and its dim destination, across every display row they wrap onto.
- A click acts on the painted frame, so a concurrent refresh never redirects it.
- A successful open reports `opened link in browser` in the status line.
- A `#anchor` destination scrolls its own surface to the matching heading instead. Headings anchor by their GitHub slug, duplicates numbered.
- Only an `http://` or `https://` destination opens in the browser, matched case-insensitively on the trimmed destination. Any other destination — an unknown scheme, a missing anchor, a control or bidi character anywhere — is inert.
- Every destination that opens is exactly the text shown.

### Input safety

- A control character or an explicit bidirectional override renders as a visible placeholder, never raw.
- A render is cached by its input text. A refresh with unchanged text recomputes nothing.

## Failure semantics

Every input renders. Malformed or partial markdown degrades toward plain paragraphs, never an error.

## Non-goals

- No terminal hyperlink escapes (OSC 8). A link acts through the click, never the emitted text.
- No keyboard link navigation. Opening a link is mouse-only.
- Nothing else renders through it: not the comment editor, not saved comment cards, not the `Changes` tab.

## Related specs

- [theme](./theme.md)
- [diff-view](./diff-view.md)
- [tui](./tui.md)
- [forge-host](./forge-host.md)
