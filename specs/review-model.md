---
Status: Current
Created: 2026-06-23
Last edited: 2026-08-08
---

# Review model

The objects a review is made of: the scope, the changed files in it, the comments, and the export.

## Overview

The central object is a comment: a note on a run of diff lines in one file, carrying the snippet it points at.

| field   | meaning                                                          |
| ------- | ---------------------------------------------------------------- |
| `file`  | repo-relative path the comment is on                             |
| `side`  | `new` for added or context lines, `old` for purely removed lines |
| `start` | first line of the range on `side`, 1-based                       |
| `end`   | last line of the range, equal to `start` for a single line       |
| `lines` | the verbatim diff lines, each keeping its `+`/`-`/space marker   |
| `text`  | free-form reviewer text, possibly multi-line                     |

Every field is required.

- `lines` is the authoritative anchor.
- `side`, `start`, and `end` are never re-bound when the diff shifts.
- The range is always contiguous.

### Scopes

A scope selects which changes `Changes` shows and which files `All files` annotates. The two tabs share one active scope. A reviewr pane starts in the config's `default_scope`, `uncommitted` when unset (`config.md`). A config reread never switches the active scope.

| scope         | shows                                                         |
| ------------- | ------------------------------------------------------------- |
| `uncommitted` | staged and unstaged changes vs `HEAD`, plus untracked files   |
| `branch`      | everything the branch carries over its base, committed or not |
| `last-turn`   | every change in the worktree's last change-producing turn     |

### Base branch

The `branch` scope diffs against the merge-base of the base branch and `HEAD`. The base is always a recorded choice, never inferred. The first source that resolves wins:

| # | source              | base is                                            |
| - | ------------------- | -------------------------------------------------- |
| 1 | `--base <ref>` flag | any rev, resolved verbatim, else as a branch name  |
| 2 | the repo pick       | the branch chosen in the base picker (`input.md`)  |
| 3 | `origin/HEAD`       | the default branch it names, when present          |

A branch name resolves through `refs/remotes/origin/<name>`, then `refs/heads/<name>`. A leading `refs/heads/`, `refs/remotes/origin/`, or `origin/` prefix on a `--base` name is stripped first. A source that resolves to no ref is skipped, never an error. When no source resolves, `branch` shows nothing and the footer offers the picker (`input.md`). The other scopes are unaffected.

The pick:

- The pick is one branch name per repository, shared by its worktrees.
- The pick persists in a private ref under `refs/reviewr/` and survives pane restarts (`overview.md`).
- The pick applies only after its ref write lands.
- The pick reaches every other pane of the repository at that pane's next refresh.
- The pick clears when the chosen branch is the one `origin/HEAD` names.
- The pick is kept and skipped while its branch does not resolve. It reactivates when the branch resolves again.
- The pick can only be replaced, never cleared, in a repository with no default branch.

The header names the base while the `branch` scope is active (`tui.md`). A base change rebuilds the changeset on the next frame, never waiting for a poll.

Open decision: whether an open PR's target branch (`forge-host.md`) joins the chain between the pick and `origin/HEAD`, pending an interaction trial. The picker stars the PR target either way (`input.md`).

Open decision: whether a name that exists both locally and on origin should resolve local first. Origin first diffs a stacked branch against a stale pushed tip. Local first diffs against a stale local `main`. The two failures are symmetric, so the order stays as it is until one is shown to hurt more.

The installed pane passes no arguments, so `--base` serves standalone and dev runs.

### Ignored paths

Every scope respects `.gitignore`. To review an ignored file, track it. This gates `Changes` only: `All files` lists every file, ignored dimmed (`file-list.md`).

### Turn baseline

The `last-turn` baseline is the worktree as it was when its most recent change-producing turn started. The scope diffs the baseline against the live worktree, so it shows every change made since that moment, whoever made it. Two agents working at once read as one diff, and the reviewer's own edits sit in it alongside theirs.

- While the worktree works, the scope shows the turn in progress. Once it rests, the just-finished turn.
- A turn that changes no file leaves the baseline untouched.
- Before reviewr observes a turn start, the baseline is unset and the scope is empty (`tui.md`).
- Commits never move the baseline.

How turns are observed and the baseline is captured is in `herdr-host.md`.

### Changed file

A row in the `Changes` list carries:

| field           | meaning                                                   |
| --------------- | --------------------------------------------------------- |
| `path`          | repo-relative path, the new path for a rename             |
| `previous_path` | the old path when renamed, absent otherwise               |
| `kind`          | `added`, `modified`, `deleted`, `renamed`, or `untracked` |
| `additions`     | lines added in the scope, all lines for an untracked file |
| `deletions`     | lines removed in the scope                                |

### Diff

The selected file's structured diff, built from its old and new content (`diff-view.md`). Comment anchors and snippets come from it. An untracked file diffs against empty old content. A binary file lists, and its pane reads `binary — no line comments`.

### File content

In `All files` a comment anchors to plain file content instead of a diff. Its `side` is `new`, its range is line numbers in the current file, and its snippet lines are space-prefixed like context lines. It exports identically to a diff comment.

A comment renders and is acted on only in the view it belongs to: a content comment in `All files`, a diff comment in `Changes`. Send, Copy, and the comments list carry the whole set across both tabs.

## Behavior

Comments are a review pass, not a durable record.

- Comments live in memory.
- A comment is removed only by export or delete. Never by a refresh or an agent's edits.
- Editing changes the text in place.
- Export takes the whole set and clears it.
- A comment whose file leaves the changeset is flagged stale, and kept.
- An `All files` comment is flagged stale only when its file is deleted from the worktree.

### Export

One block per comment, to the agent input or the clipboard:

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

| rule      | value                                                                                 |
| --------- | ------------------------------------------------------------------------------------- |
| header    | `path:start-end`, with ` (removed)` appended when `side` is `old`                     |
| body      | the comment's `lines`, verbatim                                                       |
| footer    | the comment's `text`, trimmed, line breaks kept, runs of 2+ newlines collapsed to one |
| separator | one blank line between comments                                                       |
| order     | by `file`, then `start`                                                               |
| preamble  | none                                                                                  |

- Send injects every block into the agent input, focuses the agent pane, and clears the list. It never submits (paste framing: `herdr-host.md`).
- Copy writes the same blocks to the system clipboard, then clears the list.

How the agent pane is found and filled is in `herdr-host.md`.

## Failure semantics

- A failed send or copy leaves every comment in place.
- A consumed batch is gone. A second send never re-injects it.
- Closing the pane or restarting herdr loses unexported comments.
- Each reviewr pane holds its own comments. Two panes on one worktree never share or merge them.

## Non-goals

- No durable comment store, lifecycle states, or outdated-tracking.
- No categories, severities, or threads. Text only.
- No line-number rebasing as the diff shifts.
- No auto-submit of the agent prompt.
- No inferred base branch. The reflog and the commit graph never choose a base.
- No global base list. A base is chosen per repository, never once for every repository.

## Related specs

- [configuration](./config.md)
- [diff-view](./diff-view.md)
- [tui](./tui.md)
- [herdr-host](./herdr-host.md)
