---
Status: Current
Created: 2026-07-12
Last edited: 2026-08-03
---

# Markdown rendering

How reviewr's built-in and optional external Markdown renderers produce styled terminal lines for PR bodies and file previews.

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

The PR tab always uses reviewr's built-in renderer. Both file tabs use it by default. A configured external command may replace it for file previews only (`config.md`).

The built-in renderer provides these elements:

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

### Built-in color and code

- Every color comes from the active theme's palette (`theme.md`).
- A fenced block highlights through the same highlighter and syntax theme as the diff panes (`diff-view.md`).
- The language comes from the fence's info string. An unknown or absent language renders plain.

### Built-in layout

- Lines wrap at the pane width, at word boundaries. A word wider than the pane hard-breaks.
- A wrapped continuation hangs under its block's content: list text aligns under list text, quoted text keeps its bars.
- A soft break renders as a space. A hard break starts a new line.
- Width is measured in terminal display cells.
- Nesting indents cap at 8 levels. A deeper level renders at the cap.
- A table's columns size to their widest cell. An over-wide table shrinks its widest column, repeatedly, until it fits the pane. Tied widest columns shrink together.
- A cell in a shrunk column wraps at word boundaries inside its column. A word wider than the column hard-breaks. The column separators continue on every wrapped line.
- A column never shrinks below the smaller of its natural width and 8 cells. A table still over-wide at every floor renders as its source text.
- A dim rule follows the header row. No separator line divides one body row from the next, however deep the cells wrap.

### Built-in links

- The PR tab and built-in file previews provide link and heading interactions.
- Clicking a link opens its destination in the browser.
- The click target spans the link text and its dim destination, across every display row they wrap onto.
- A click acts on the painted frame, so a concurrent refresh never redirects it.
- A successful open reports `opened link in browser` in the status line.
- A `#anchor` destination scrolls its own surface to the matching heading instead. Headings anchor by their GitHub slug, duplicates numbered.
- Only an `http://` or `https://` destination opens in the browser, matched case-insensitively on the trimmed destination. Any other destination — an unknown scheme, a missing anchor, a control or bidi character anywhere — is inert.
- Every destination that opens is exactly the text shown.

### Input safety

- A control character or an explicit bidirectional override renders as a visible placeholder, never raw.
- A built-in render is cached by its input text and width. An external file preview also keys on its command and the theme's dark/light appearance. A refresh with unchanged inputs recomputes nothing.

### External file previews

`file_markdown_renderer` supplies one command string for the `Changes` and `All files` previews. The command receives Markdown on standard input. reviewr executes its parsed arguments directly without a shell (`config.md`).

- `{style}` in an argument becomes `dark` or `light` from the active resolved reviewr theme.
- `{width}` in an argument becomes the preview pane's current width.
- The child receives `CLICOLOR_FORCE=1`, so Glow keeps colors while its output is piped.
- The child has a five-second wall-clock limit. reviewr kills and reaps it after the limit.
- The captured output has a 16 MiB limit.
- Output beyond 100,000 lines or 250,000 style runs falls back before ANSI conversion.
- Select Graphic Rendition ANSI color and attribute sequences become terminal styles.
- Every other terminal control sequence is removed.
- A missing command, failed exit, timeout, excessive output, or invalid ANSI output falls back to the built-in renderer.

External output has no Markdown identity metadata. Its links are not clickable and its headings are not jump targets. A preview opens at the top instead of following the source cursor. Returning from a scrolled `All files` preview returns to the source top.

## Failure semantics

Every input renders. Malformed Markdown degrades toward built-in plain paragraphs. An external renderer failure uses the complete built-in render.

## Non-goals

- No terminal hyperlink escapes (OSC 8). A link acts through the click, never the emitted text.
- No keyboard link navigation. Opening a link is mouse-only.
- Nothing else renders through it: not the comment editor, not saved comment cards, not diff or source rows.

## Related specs

- [theme](./theme.md)
- [diff-view](./diff-view.md)
- [pr-tab](./pr-tab.md)
- [forge-host](./forge-host.md)
