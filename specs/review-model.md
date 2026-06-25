---
Status: Current
Created: 2026-06-23
Last edited: 2026-06-25
---

# Review model

The objects a review is made of: the scope being reviewed, the changed files in it, the comments you leave, and how they export.

## Overview

The central object is a comment — a note attached to a run of diff lines in one file, carrying the snippet it points at:

```json
{
  "file": "extruct/core/llm_registry.py",
  "side": "new",
  "start": 40,
  "end": 41,
  "lines": "-from .z import w\n+from .x import y",
  "text": "this import path looks wrong"
}
```

| field | type | required | meaning |
|-------|------|----------|---------|
| `file` | string | yes | Repo-relative path the comment is on. |
| `side` | enum | yes | `new` when the range is added or context lines; `old` when it is purely removed lines. |
| `start` | integer | yes | First line of the range on `side`, 1-based. |
| `end` | integer | yes | Last line of the range; equals `start` for a single line. |
| `lines` | string | yes | The verbatim diff lines the comment anchors to, each keeping its `+`/`-`/space marker. |
| `text` | string | yes | Free-form reviewer text, no categories or severities. May span multiple lines. |

`lines` is the authoritative anchor: it lets the agent find the exact code even after later edits shift line numbers. `side`/`start`/`end` orient a human and are not re-bound when the diff shifts. The range is always contiguous — `lines` covers every line in `start..end` — because a selection cannot cross a fold (`diff-view.md`), so the range never brackets hidden lines the snippet omits.

### Scopes

A scope selects which changes the `Changes` view shows. The default is `uncommitted`.

| scope | means | source |
|-------|-------|--------|
| `uncommitted` | staged and unstaged changes vs `HEAD`, plus untracked files | `git diff HEAD` and `git status --porcelain` |
| `branch` | `HEAD` vs the merge-base with the base branch | `git diff $(git merge-base <base> HEAD)...HEAD` |
| `last-turn` | the worktree vs the turn baseline — what the agent changed in its most recent change-producing turn, including untracked files | `git diff <turn-baseline> <worktree snapshot>` |

The base branch is `origin/main`, falling back to `origin/master`, then `main`, then `master`. It is overridable by config or flag.

### Turn baseline

The `last-turn` baseline is the worktree as it was at the start of the agent's most recent turn that changed a file. The scope diffs that baseline against the live worktree, so while the agent works it shows the turn in progress, and once the agent goes idle it shows the just-finished turn.

- A turn that changes no file — a question answered, a plan discussed — leaves the baseline untouched, so the scope keeps showing the previous change-producing turn.
- Until reviewr has observed a turn start, the baseline is unset and the scope is empty (`tui.md`); it becomes live on the next turn.
- The baseline is independent of commits: if the agent commits mid-turn, the scope still diffs the baseline against the worktree, so committed and uncommitted work both appear.

How reviewr observes turns and captures the baseline is in `herdr-host.md`.

### Changed file

A row in the `Changes` list. As rendered:

```
extruct/core/llm_registry.py          M   +18 -8
docs/specs/2026-06-22-methodology.md  A   +116
scripts/old_runner.py                 D   -47
```

| field | type | meaning |
|-------|------|---------|
| `path` | string | Repo-relative path; the new path for a rename. |
| `previous_path` | string? | The old path when the file was renamed, for diffing against its real old content; absent otherwise. |
| `kind` | enum | One of `added`, `modified`, `deleted`, `renamed`, `untracked`. |
| `additions` | integer | Lines added in the scope; an untracked file counts as all additions. |
| `deletions` | integer | Lines removed in the scope. |

### Diff

For the selected file in the current scope, a structured diff built from the file's old and new content, defined in `diff-view.md`. It is what comment anchors and snippets come from: a comment's `lines` snippet is reconstructed from the rows it covers. An untracked file diffs against empty old content; a binary file appears in the list but its pane reads `binary — no line comments`.

## Behavior

### Lifecycle

Comments are lightweight and short-lived: a review pass, not a durable record.

- Comments live in memory while the sidebar runs; there is no on-disk store.
- A comment is removed only by exporting or deleting it — never by a refresh or by the agent editing files.
- You can add, edit, and delete a comment; editing changes its text in place.
- Exporting sends the whole set at once and clears it — there is no single-comment export; a sent or copied batch has done its job.
- A comment whose file leaves the changeset is flagged stale but kept; you decide whether to send or delete it.

### Export

A comment goes to the agent (the primary path) or the clipboard, as one block per comment, with the file, the line range, and the snippet it anchors to:

```
extruct/core/llm_registry.py:41
-from .z import w
+from .x import y
this import path looks wrong
and breaks the 3.12 import resolver

scripts/old_runner.py:38 (removed)
-    cleanup_temp_files()
why drop this? it is still needed
```

| rule | value |
|------|-------|
| header | `path:start-end`, with ` (removed)` appended when `side` is `old` |
| body | the comment's `lines`, verbatim |
| footer | the comment's `text`, trimmed, with its line breaks kept; runs of 2+ newlines collapse to one so no blank line splits a block |
| separator | one blank line between comments |
| order | by `file`, then `start` |
| preamble | none — the format reads as review comments on its own |

The actions:

- Send — inject every comment's block into the agent input, focus the agent pane, and clear the list.
- Copy — write the same blocks to the system clipboard, then clear the list.

Both act on the whole set; there is no single-comment variant. Send fills the agent input without submitting; you add context and press enter. How the agent pane is found and filled is in `herdr-host.md`.

## Failure semantics

Export is the only side effect, and comments are in-memory.

- The agent editing files concurrently never removes a comment or the text being typed; a refresh only re-reads diffs.
- Comments are removed only after a successful export; a failed send or copy leaves all of them in place.
- A consumed batch is gone, so a second send never re-injects it — no duplicates.
- Comments live only in memory: closing the sidebar pane or restarting herdr loses any not yet exported.
- The sidebar assumes one instance per worktree.

## Non-goals

- No durable comment store, lifecycle states, or outdated tracking — unlike a full PR-review tool.
- No categories, severities, or threads — text only.
- No line-number rebasing as the diff shifts; the `lines` snippet, not the number, keeps a comment locatable.
- No auto-submit of the agent prompt — you press enter.

## Decisions

- Carry the diff snippet, GitHub-style — a comment exports the lines it anchors to (like GitHub's `diff_hunk`), so removed lines are commentable and the agent sees the exact code, not a number that may shift.
- In-memory and consumed on export, not a durable store — the workflow is a few comments then a prompt; Conductor persists comments in SQLite with a state machine and `is_outdated`, and Superset persists none, so the light end fits.
- Allow in-place edit — delete-and-retype mid-review is a trust-breaking surface; editing text is cheap.
- Flag stale comments, never auto-drop — silently losing a comment destroys trust and forces you to wait for the agent to stop; a comment is removed only by export or delete.
- Send to the agent, with clipboard secondary — the fill-input-and-focus flow is the asked-for path; clipboard stays for paste-anywhere and remote.
- One Send, not send-one vs send-all — the workflow is "write a few comments, then hand them over"; a per-comment send is a needless choice on the hot path, so `Send` always takes the whole set (`Copy` likewise).
- `last-turn` anchors to the most recent change-producing turn, not every turn — re-baselining on every turn start would blank the view after any text-only turn (a question, a plan); holding the baseline until a turn actually edits a file keeps the last real diff on screen. Rejected: re-baseline on every idle→working edge.

## Open decisions

- None.

## Related specs

- `./diff-view.md`
- `./tui.md`
- `./herdr-host.md`
