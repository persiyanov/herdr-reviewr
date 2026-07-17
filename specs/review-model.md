---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-13
---

# Review model

The objects a review is made of: the scope, the changed files in it, the comments, and the export.

## Overview

The central object is a comment: a note on a run of diff lines in one file, carrying the snippet it points at.

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

| field   | type    | meaning                                                                     |
| ------- | ------- | --------------------------------------------------------------------------- |
| `file`  | string  | repo-relative path the comment is on                                         |
| `side`  | enum    | `new` for added or context lines, `old` for purely removed lines             |
| `start` | integer | first line of the range on `side`, 1-based                                   |
| `end`   | integer | last line of the range, equal to `start` for a single line                   |
| `lines` | string  | the verbatim diff lines, each keeping its `+`/`-`/space marker               |
| `text`  | string  | free-form reviewer text, possibly multi-line                                 |

Every field is required.

The anchor rules:

- `lines` is the authoritative anchor. The agent finds the code by snippet, even after edits shift line numbers.
- `side`, `start`, and `end` orient a human. They are never re-bound when the diff shifts.
- The range is always contiguous. A selection cannot cross a fold (`diff-view.md`), so the snippet never omits hidden lines.

### Scopes

A scope selects which changes `Changes` shows and which files `All files` annotates. The two tabs share one active scope. A sidebar starts in the config's `default_scope`, `uncommitted` when unset (`config.md`). A config reread never switches the active scope.

| scope         | shows                                                          | source                                                       |
| ------------- | --------------------------------------------------------------- | ------------------------------------------------------------ |
| `uncommitted` | staged and unstaged changes vs `HEAD`, plus untracked files      | `git diff HEAD`, `git status --porcelain`                     |
| `branch`      | everything the branch carries over its base, committed or not    | `git diff $(git merge-base <base> HEAD)`, plus untracked      |
| `last-turn`   | what the agent changed in its most recent change-producing turn  | `git diff <turn baseline> <worktree snapshot>`                |

- `branch` is a superset of `uncommitted`. The base is an ancestor of `HEAD`, so working-tree changes appear in both. With nothing committed past the base, the two coincide.
- `last-turn` nests in neither. It anchors to a point in time, so it also shows work the agent has since committed.

### Base branch

The `branch` scope diffs against the merge-base of the base branch and `HEAD`.

```toml
# $HERDR_PLUGIN_CONFIG_DIR/config.toml
base_branches = ["origin/main", "origin/master", "main", "master"]   # the default
# a gitflow repo puts its trunk first:
base_branches = ["origin/develop", "origin/main", "main", "master"]
```

Precedence. The first source that yields a ref existing in the repo wins:

| # | source                          | base is                                        |
| - | ------------------------------- | ----------------------------------------------- |
| 1 | `--base <ref>` flag             | `<ref>` when it exists, otherwise skipped       |
| 2 | `base_branches` in `config.toml` | the first listed ref that exists in the repo   |

- The list is re-read on refresh. Editing it re-bases the scope without a relaunch.
- A listed ref absent from the repo is skipped, never an error.
- A missing config or omitted `base_branches` uses the default list. Invalid plugin config follows `config.md`.
- When no candidate exists, `branch` shows nothing. The other scopes are unaffected.
- The installed pane passes no arguments, so inside herdr the config key is the only channel. `--base` serves standalone and dev runs, where it wins.
- Standalone, with no `HERDR_PLUGIN_CONFIG_DIR`, reviewr reads no config file.

### Ignored paths

Every scope respects `.gitignore`. A path git ignores is never a change, so build output never enters `Changes`. To review an ignored file, track it. This gates `Changes` only: `All files` lists every file, ignored dimmed (`file-list.md`).

### Turn baseline

The `last-turn` baseline is the worktree as it was when the agent's most recent change-producing turn started. The scope diffs the baseline against the live worktree.

- While the agent works, the scope shows the turn in progress. Once the agent goes idle, the just-finished turn.
- A turn that changes no file leaves the baseline untouched. The scope keeps showing the previous change-producing turn.
- Before reviewr observes a turn start, the baseline is unset and the scope is empty (`tui.md`). It becomes live on the next observed turn.
- Commits never move the baseline. Work the agent commits mid-turn still shows.

How turns are observed and the baseline is captured is in `herdr-host.md`.

### Changed file

A row in the `Changes` list:

```
extruct/core/llm_registry.py          M   +18 -8
docs/specs/2026-06-22-methodology.md  A   +116
scripts/old_runner.py                 D   -47
```

| field           | type    | meaning                                                          |
| --------------- | ------- | ----------------------------------------------------------------- |
| `path`          | string  | repo-relative path, the new path for a rename                     |
| `previous_path` | string? | the old path when renamed, absent otherwise                       |
| `kind`          | enum    | `added`, `modified`, `deleted`, `renamed`, or `untracked`         |
| `additions`     | integer | lines added in the scope, all lines for an untracked file         |
| `deletions`     | integer | lines removed in the scope                                        |

### Diff

The selected file's structured diff, built from its old and new content (`diff-view.md`). Comment anchors and snippets come from it. An untracked file diffs against empty old content. A binary file lists, and its pane reads `binary — no line comments`.

### File content

In `All files` a comment anchors to plain file content instead of a diff. Its `side` is `new`, its range is line numbers in the current file, and its snippet lines are space-prefixed like context lines. It exports identically to a diff comment. Its header never carries ` (removed)`.

A comment renders and is acted on only in the view it belongs to: a content comment in `All files`, a diff comment in `Changes`. Their line numberings differ, so a comment never lands on an unrelated line in the other tab. Send, Copy, and the comments list carry the whole set across both tabs.

## Behavior

Comments are a review pass, not a durable record.

- Comments live in memory. There is no on-disk store.
- A comment is removed only by export or delete. Never by a refresh or the agent's edits.
- Comments can be added, edited, and deleted. Editing changes the text in place.
- Export takes the whole set and clears it. There is no single-comment export.
- A comment whose file leaves the changeset is flagged stale, and kept.
- An `All files` comment is flagged stale only when its file is deleted from the worktree.

### Export

One block per comment, to the agent input (the primary path) or the clipboard:

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

| rule      | value                                                                              |
| --------- | ----------------------------------------------------------------------------------- |
| header    | `path:start-end`, with ` (removed)` appended when `side` is `old`                    |
| body      | the comment's `lines`, verbatim                                                      |
| footer    | the comment's `text`, trimmed, line breaks kept, runs of 2+ newlines collapsed to one |
| separator | one blank line between comments                                                      |
| order     | by `file`, then `start`                                                              |
| preamble  | none                                                                                 |

- Send injects every block into the agent input, focuses the agent pane, and clears the list. It never submits. The user adds context and presses enter.
- Copy writes the same blocks to the system clipboard, then clears the list.

How the agent pane is found and filled is in `herdr-host.md`.

## Failure semantics

- A failed send or copy leaves every comment in place. Removal happens only after a successful export.
- A consumed batch is gone. A second send never re-injects it.
- Closing the pane or restarting herdr loses unexported comments.
- One instance per worktree is assumed.

## Non-goals

- No durable comment store, lifecycle states, or outdated-tracking.
- No categories, severities, or threads. Text only.
- No line-number rebasing as the diff shifts. The snippet keeps a comment locatable.
- No auto-submit of the agent prompt.

## Related specs

- [configuration](./config.md)
- [diff-view](./diff-view.md)
- [tui](./tui.md)
- [herdr-host](./herdr-host.md)
