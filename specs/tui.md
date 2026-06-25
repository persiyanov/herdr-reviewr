---
Status: Current
Created: 2026-06-23
Last edited: 2026-06-25
---

# TUI

The terminal interface: how the review is laid out, how you drive it by keyboard and mouse, and how it stays current.

## Overview

```
┌ Changes [uncommitted]  9 files ──────────────────────────── [ Send (3) ] ┐
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
│ 9 file(s) · 3 comment(s)  tab focus · c comment · s send · y copy · l list │
└────────────────────────────────────────────────────────────────────────────┘
```

- The header shows the active tab, the scope, the file count, and a clickable `Send` button with the comment count.
- The left pane is the selected file's diff — syntax-highlighted, with line numbers, change bars, word-level emphasis, and foldable context, defined in `diff-view.md`.
- The right pane is the changed-files navigator for the current scope — a directory tree, defined in `file-list.md`.
- The comment input opens **inline, directly under the last line of the selection**, pushing the diff below it down; it grows as you type more lines. It is not a footer band.
- The footer is a key-hint and status line.

The tab bar shows `Changes`, and leaves room for the roadmap `All files` and `Checks` tabs.

## Behavior

### Interaction

Every action has a keyboard binding. The mouse-relevant ones also work by click or drag, since herdr is mouse-native. The specific keys are a provisional v1, not a final keymap.

| action | keyboard | mouse |
|--------|----------|-------|
| move the cursor in the focused pane — in the file list this selects a file and loads its diff | `j` / `k` / `↑` / `↓` | click a file row |
| collapse / expand the directory under the cursor | `←` / `→` | click the directory row |
| switch focus between the file list and the diff | `tab` | click a pane |
| move the cursor a page in the focused pane | `PageUp` / `PageDown` / `ctrl+u` / `ctrl+d` | — |
| scroll a pane's viewport, leaving the selection put | — | wheel over the pane |
| scroll the diff horizontally, when wrap is off and not on a fold | `←` / `→` | — |
| switch scope | `u` uncommitted / `b` branch / `t` last-turn | click the scope in the header to cycle |
| expand the fold under the cursor | `→` | click the `⋯` fold row |
| toggle line wrap | `w` | — |
| resize the panes — widen / narrow the file list | `]` / `[` | drag the divider between the panes |
| select a line range, removed lines included | `v` then move | click-drag in the diff |
| comment on the selection (opens the editor below — see Comment editor) | `c`, type, `enter` | after a drag-select |
| edit the comment under the cursor | `e` | — |
| delete the comment under the cursor | `d` | — |
| jump to next / previous comment | `n` / `N` | — |
| list and manage all comments | `l` | — |
| send all written comments to the agent | `s` / `S` | click `Send` |
| copy all written comments to the clipboard | `y` / `Y` | — |
| refresh now | `r` | — |
| quit | `q` | — |

Writing a comment: select a range or land on a line, press `c`, and an input box opens **inline under the last selected line**, where you edit it (see **Comment editor**). `enter` saves, `esc` cancels.

On save the input box closes and the comment stays visible: it renders as a **read-only card spliced inline under its line**, titled with its location, so written feedback is always on screen while reviewing rather than hidden behind a marker. `e` reopens the card under the cursor as an edit box in place (its card is hidden while editing); `d` deletes it. There is no single-vs-all choice: `s` / `S` (or the `Send` button) sends every written comment, and a successful send reports a transient status such as `sent 3 comments` that fades after a few seconds.

### Comment editor

The inline box is a plain-text field that behaves like an ordinary editor: the caret sits where you put it, and you insert or delete **at the caret**, not only at the end. An empty box shows a dim `Leave a comment…` placeholder; opening it on an existing comment (`e`) preloads the text with the caret at the end.

```
┌ comment · llm_registry.py:41 ───────────┐
│ this import path looks wrong█            │   ← caret is a block at its position
│ and breaks on 3.12                       │
└──────────────────────────────────────────┘
```

| in the box | does |
|------------|------|
| `←` / `→` | move the caret one character |
| `↑` / `↓` | move the caret one wrapped row, keeping the column where the row allows |
| `Home` / `End`, `Ctrl+A` / `Ctrl+E` | move the caret to the start / end of the logical line |
| `Alt+b` / `Alt+f` (or `Alt`/`Ctrl` + `←` / `→`) | move the caret by a word |
| `Backspace` / `Delete` | delete the character before / after the caret |
| `Ctrl+W` | delete the word before the caret |
| `Ctrl+U` / `Ctrl+K` | delete to the start / end of the logical line |
| `Alt+Enter` / `Shift+Enter` / `Ctrl+J` | insert a newline at the caret |
| `Enter` / `Esc` | save / cancel (cancel discards the draft) |

- A paste arrives as one insertion at the caret via bracketed paste, so a multi-line paste keeps its newlines instead of the first one submitting the comment; `\r\n` and `\r` normalize to `\n`.
- The caret is a char position into the text; movement, insertion, and deletion are character-wise (multi-byte and wide characters count as whole characters), and the box word-wraps and grows exactly as it measures.
- `↑` / `↓` move by what you see (wrapped rows); `Home` / `End` and the kill keys act on the logical line — the run of text between explicit newlines — so they ignore soft wrapping.
- Word-jump is `Alt+b` / `Alt+f` (which survive as ESC-prefixed sequences); the `Alt`/`Ctrl` + `←`/`→` variants also work where the terminal delivers modified arrows, which many multiplexers strip. The character arrows, `Home`/`End`, and `Ctrl+A`/`Ctrl+E` always work.

### Refresh

- The view polls the worktree every `N` seconds, default `2`, overridable by config.
- A poll reloads the changed-files list and the open diff, keeping the selected file and scroll position where the file still exists.
- While you are composing a comment, the input and the diff you are commenting on are frozen; the file list still updates.
- `r` forces an immediate reload.
- Refresh cadence is timer-based and uses no herdr events; the same poll also samples the agent's status to track the `last-turn` baseline (`herdr-host.md`).
- In `last-turn` scope, until a turn start has been observed the file list and diff show an empty state — `waiting for the agent's next turn` — rather than a stale or whole-worktree diff.

## Failure semantics

- A poll never touches the comment input or saved comments — the draft text and the caret position survive a refresh — so you can comment while the agent writes files without losing anything.
- A poll that finds no change makes no visible update: no flicker, no lost selection or scroll, no interruption to an in-progress comment.
- Git, clipboard, and agent-send calls run synchronously between frames; they are fast for a typical repo, but a very large diff or a hung `herdr agent send` can briefly block input until it returns. Moving them off the draw path is a v1 non-goal.
- A paste outside the comment editor is ignored; it never starts or mutates a comment.

## Non-goals

- No editing, staging, or committing from the UI — review and comment only.
- No side-by-side split view — the diff is one unified column; split is roadmap.
- No text selection, cut/copy, undo/redo, markdown rendering, or click-to-place-caret in the comment editor — it is a keyboard-driven plain-text field.

## Decisions

- Two-pane focus, not scope on `tab` — `j`/`k` drive whichever pane is focused, and `tab` switches focus. Anchoring comments and jumping between them needs a per-line diff cursor, so the diff is independently focusable rather than scroll-only; scope moves to `u`/`b` (and a clickable scope chip).
- One scroll model for both panes — each pane has a cursor and an independent viewport offset. The keyboard moves the cursor (the view reveals it); the mouse wheel scrolls the pane-under-the-pointer's viewport and never moves the cursor. So wheeling to read context never moves the comment anchor, and both panes behave identically. Rejected: cursor-coupled scrolling, where the wheel drags the cursor — it mis-anchors comments and made the two panes inconsistent.
- Poll on a timer, not on agent turns — turn transitions are too coarse and slow to drive the refresh cadence; polling is simple and predictable. The agent's turn signal moves only the `last-turn` baseline (`herdr-host.md`), never the refresh interval.
- Keyboard and mouse together — the asked-for flow includes a clickable `Send` button and click-to-open files.
- Inline comment input — the box opens under the selected line (insert: the diff below shifts down) rather than in a detached footer, so the comment sits with the code it is about; it grows to fit multi-line text.
- One `Send`, not send-one vs send-all — there is just a set of written comments; `s` / `S` / the button send them all. Removing the distinction drops a needless choice from the hot path.
- Component architecture — each region (`TabBar`, `FileList`, `DiffView`, `CommentInput`, `StatusBar`) owns its state and is testable in isolation, over a single monolithic update.
- A structured diff viewer, not rendered git-diff text — the diff pane renders the model in `diff-view.md` (syntax, line numbers, change bars, word emphasis, folds), so the pane shows code, not raw `git diff` plumbing.
- A real caret, not an append buffer — the comment box keeps a character caret so text is edited where the caret is, not only at the end. The cost is one position to track; the payoff is that fixing a typo mid-comment no longer means deleting everything after it. Rejected: the end-only input it replaces.
- Bracketed paste, not raw keystrokes — pastes are read as one bracketed-paste event and inserted whole, so a multi-line paste keeps its newlines. Rejected: the terminal's default raw paste, where an embedded newline arrives as `enter` and submits the comment early.
- A plain text field, not a code editor — caret movement, edit-anywhere, word ops, and paste, but no selection, undo/redo, or markdown rendering. Review comments are short; that machinery is more than they need. Rejected: a fuller editor with selection and history.

## Open decisions

- Keymap — the v1 bindings are provisional; the final shortcut set is a separate discussion.

## Related specs

- `./diff-view.md`
- `./file-list.md`
- `./review-model.md`
