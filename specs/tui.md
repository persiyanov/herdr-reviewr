---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-09
---

# TUI

The terminal interface: how the review is laid out, how you drive it by keyboard and mouse, and how it stays current.

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

- The header shows the three tabs with the active one highlighted, the scope, the count of files changed in the scope, and a clickable `Send` button with the comment count.
- The left pane is the selected file's diff — syntax-highlighted, with line numbers, change bars, word-level emphasis, and foldable context, defined in `diff-view.md`.
- The right pane is the changed-files navigator for the current scope — a directory tree, defined in `file-list.md`.
- Comments are one set across both tabs and export together; each tab otherwise owns its state (see **Tabs**).
- The comment input opens **inline, directly under the last line of the selection**, pushing the diff below it down; it grows as you type more lines. It is not a footer band.
- The footer is a live action bar — the actions available for what you're doing now, the most likely one highlighted, the rest dropped to fit one line (see **Footer**).

The tab bar shows `Changes`, `All files`, and `PR`. The active tab sets the two panes' content: a diff and changed-files tree in `Changes`, file content and a whole-repo tree in `All files`, and the PR's checks and comments in `PR` (see **PR tab**). The review loop — select, comment, send — is the same in `Changes` and `All files`; `PR` is a read-only mirror of the pull request, with no authoring.

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
| switch tab — `Changes` / `All files` / `PR` | `1` / `2` / `3` (provisional) | click a tab name |
| expand the fold under the cursor | `→` | click the `⋯` fold row |
| toggle line wrap | `w` | — |
| resize the panes — widen / narrow the file list | `]` / `[` | drag the divider between the panes |
| select a line range, removed lines included | `v` then move | click-drag in the diff |
| clear the line selection | `esc` | — |
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

### Footer

The footer is a live action bar: it shows the actions available for what you are doing right now, the most likely one highlighted, and drops the least relevant when the line is full. It never lists a key that wouldn't work in the current state, so it teaches by showing rather than by a memorized map.

```
 c comment · v select lines                          │ ⇥ files · 1·2·3 · q
```

- **Primary action** — the most likely next step, in a bright accent, always shown.
- **Other available actions** — what else works here, in normal text.
- **Send** — `s send N`, with the comment count riding the action; it rises to just below the primary once any comment is written, and is absent when none are.
- **Orientation** — a dim, stable cluster at the right — the pane toggle, the tab digits, and quit (`⇥ files · 1·2·3 · q`, less the pane toggle on `PR`); the only fixed part, dropped first when space is tight.
- **Transient status** — a message like `comment added` shows briefly among the actions and fades; it never replaces them.

The bar is one line, filled by priority — primary, then send, then the other actions, then orientation — until the width runs out; a trailing `…` marks anything clipped. Movement keys aren't shown: moving the cursor is obvious. The comment editor and the comments list show their own actions (see those sections).

The actions follow the cursor:

| Where the cursor is | Primary | Also |
| --- | --- | --- |
| A diff line | `c comment` | `v select` |
| A live selection | `c comment` | `esc clear` |
| A commented line | `e edit` | `d delete · n/N jump` |
| A fold | `→ expand fold` | — |
| A file (file list) | `⇥ diff` | — |
| A collapsed directory | `→ expand` | — |
| An expanded directory | `← collapse` | — |
| Nothing to review (awaiting a turn) | `u/b/t scope` | `r refresh` |

`u/b/t scope` is always available while reviewing, so it shows in every context on the `Changes` and `All files` tabs alongside the context's own actions (except where switching scope is itself the primary — the empty and no-diff states). Whenever a comment is written, `s send N · l list` joins the bar wherever the cursor is. The changed-file count stays in the header (scope summary + `Send` button); the footer carries only the comment count, folded into `s send N`.

On the read-only `PR` tab the bar leads with the PR's state — its merge, sync, checks, and comment counts — since that is the relevant thing to show, not authoring actions; `o open ↗` follows, then the orientation cluster (see **PR tab**).

### Tabs

- Each tab owns its state: its opened file or card, the scroll, the cursor, and which directories are expanded or cards are sent. Nothing carries between the tabs.
- Switching away and back restores the tab exactly — the same file open at the same scroll — so the tabs are independent workspaces.
- A first visit to a tab opens its first file or card (or, on a collapsed tree with the cursor on a directory, nothing until you pick one).
- A tab switch keeps the focused side — content or tree — so keyboard navigation continues; an empty left pane focuses the tree.

### PR tab

The `PR` tab is a read-only mirror of the pull request, in the same two-pane frame as Changes: the right pane navigates the PR's checks and comments, the left pane reads the selected comment, and the header carries the PR's identity and state. It reads GitHub through `forge-host.md` and writes nothing — its only outward action is opening a link in the browser.

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

- The header right-anchors a clickable `status #226 ↗` chip — status colored by lifecycle (`open` green, `draft` yellow, `merged` mauve, `closed` red), the `↗` in the number's colour — with the PR title right-aligned to its left, truncated to fit. Clicking the chip (or `o`) opens the PR.
- Between the title and the chip sits the PR's resolved head branch (`head_ref`, `forge-host.md`), dim, prefixed `⑂ ` when the head lives in a fork — the name that resolved can differ from the worktree's local branch, so the header names it. On a narrow bar the branch drops first, before the title or chip.
- The footer is the action bar (see **Footer**): on this read-only tab it leads with the PR's merge, sync, and checks state and the comment count (`⚠ conflicts with main · ⇡ 2 unpushed · ✗ 1 failing · 5 comments`), with merge and sync shown only while the PR is open, then `o open ↗` and the dim orientation cluster. When a capped surface has more rows than fetched, a trailing `+more on GitHub ↗` marker is appended (`forge-host.md`).
- The right pane (titled `Checks & comments`, so it doesn't repeat the left pane's `PR`) is the navigator: a status-only `checks` section (each check as `icon name`) above the `comments` list, which is what the cursor walks.
- The `comments` list is newest first, each row `@author anchor age`, with an `outdated` or `resolved` marker where GitHub has receded the thread.
- The left pane reads the selected comment: a finding shows its `diff_hunk` as text then the body, a review or plain comment shows its prose.
- A human author is emphasised over the bots, so a person's comment stands out in a bot-heavy list.
- `j`/`k` or a click selects and reads a comment; `PageUp`/`PageDown` and the wheel scroll the read pane; `o` (and the header button) opens the PR in the browser.
- The tab adds no authoring — `s`, `c`, `v`, `d`, `e` do nothing here.
- A `merged` or `closed` PR shows the same mirror read-only under a `#226 merged` header.
- No open PR, or no usable `gh`, shows the matching empty state from `forge-host.md`, each naming the command that unblocks it.

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
- The `PR` tab reads GitHub on its own cadence (`forge-host.md`), separate from the worktree poll: it fetches when the panel opens, refetches on entering the tab, on `r`, and on the agent's turn-end while the tab is active, and falls back to a slow timer.
- A PR refetch keeps your place, like a worktree poll: the comment cursor follows the selected comment by identity and the read-pane scroll is kept; if that comment is gone (or none was selected) the cursor clamps to the new list and the read pane resets to the top.
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
- Poll the worktree on a timer, not on agent turns — turn transitions are too coarse to drive the changed-files refresh; polling is simple and predictable. The agent's turn signal moves the `last-turn` baseline (`herdr-host.md`); the separate `PR` tab also uses turn-end as one refetch trigger (`forge-host.md`), but neither changes the worktree poll interval.
- Keyboard and mouse together — the asked-for flow includes a clickable `Send` button and click-to-open files.
- Inline comment input — the box opens under the selected line (insert: the diff below shifts down) rather than in a detached footer, so the comment sits with the code it is about; it grows to fit multi-line text.
- One `Send`, not send-one vs send-all — there is just a set of written comments; `s` / `S` / the button send them all. Removing the distinction drops a needless choice from the hot path.
- Component architecture — each region (`TabBar`, `FileList`, `DiffView`, `CommentInput`, `StatusBar`) owns its state and is testable in isolation, over a single monolithic update.
- A structured diff viewer, not rendered git-diff text — the diff pane renders the model in `diff-view.md` (syntax, line numbers, change bars, word emphasis, folds), so the pane shows code, not raw `git diff` plumbing.
- A real caret, not an append buffer — the comment box keeps a character caret so text is edited where the caret is, not only at the end. The cost is one position to track; the payoff is that fixing a typo mid-comment no longer means deleting everything after it. Rejected: the end-only input it replaces.
- Bracketed paste, not raw keystrokes — pastes are read as one bracketed-paste event and inserted whole, so a multi-line paste keeps its newlines. Rejected: the terminal's default raw paste, where an embedded newline arrives as `enter` and submits the comment early.
- A plain text field, not a code editor — caret movement, edit-anywhere, word ops, and paste, but no selection, undo/redo, or markdown rendering. Review comments are short; that machinery is more than they need. Rejected: a fuller editor with selection and history.
- The tab sets the view, not the file's state — `All files` shows a file's content even when it is changed; to read its diff you switch to `Changes`, so a tab renders one kind of thing. Rejected: showing the diff for changed files and content for unchanged ones in `All files`.
- Click a tab to switch; the key is provisional — herdr is mouse-native, so the tab bar is clickable, and the keyboard binding is part of the open keymap. Rejected: locking a tab key now.
- A context-aware action bar, not a static hint line — the footer shows only the actions that work for what the cursor is on, the most likely one highlighted, dropping the least relevant to fit one line. It teaches by surfacing the next move in place, so nothing has to be memorized. Rejected: a static dump of every binding (unscannable, grows with each key); and a lean footer plus a `?` keys overlay (still a map to open and learn, the opposite of teaching in place).
- One footer painter across all tabs — a single band renders on every tab; only its contents differ (actions on the authoring tabs, the PR's state plus `o open` on `PR`). Rejected: the `PR` tab's separate, dimmer footer.
- Changed count in the header, comment count in the footer's `Send` — the changed-file count stays in the header (scope summary + `Send` button); the footer carries only the comment count, folded into `s send N` so the tally rides the action it feeds. Rejected: a standalone count segment, or repeating `N changed` in the footer.

## Open decisions

- Keymap — the v1 bindings are provisional; the final shortcut set is a separate discussion.

## Related specs

- `./diff-view.md`
- `./file-list.md`
- `./review-model.md`
