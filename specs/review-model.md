---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-12
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

A scope selects which changes `Changes` shows and which files `All files` annotates. The two tabs share one active scope. The default is `uncommitted`.

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

## Store

Comments persist to one JSON file per comment: `<git-dir>/reviewr/comments/<id>.json`, where `<git-dir>` is `git rev-parse --git-dir` resolved from the repo — per worktree automatically, since each linked worktree has its own git dir. The directory is created on first write; a missing directory reads as no comments, never an error.

```json
{
  "id": "c-1752264012345-7f3a",
  "author": "user",
  "status": "open",
  "created_at": "2026-07-11T18:40:12Z",
  "file": "worker/index.js",
  "side": "new",
  "start": 25,
  "end": 25,
  "lines": "+  await KV.put(key, String(n + 1))",
  "text": "KV increments can lose concurrent updates"
}
```

A stored comment is the comment object above plus lifecycle fields:

| field        | type   | meaning                                                             |
| ------------ | ------ | --------------------------------------------------------------------- |
| `id`         | string | `c-<epoch-ms>-<4 hex>`, unique and sortable by creation                |
| `author`     | enum   | `user` (written only by the TUI) or `agent` (the CLI's default)        |
| `status`     | enum   | `open` or `resolved`                                                   |
| `created_at` | string | ISO-8601 `…Z`, set at write time                                        |
| remaining    | —      | exactly the comment object fields above                                |

- Per-comment files make the TUI and the CLI safe concurrent writers with no locking: adding is an exclusive file create (a same-millisecond id collision retries once with a fresh id); a status flip or delete rewrites or unlinks only that one file.
- Unknown fields survive a rewrite — a status flip reads the file as untyped JSON, sets `status`, and writes it back, so a newer version's extra field is never dropped by an older one.
- A file that fails to parse is skipped and logged, never deleted — it's left for a human to inspect.
- The TUI loads the store at startup, and re-reads it whenever a cheap per-tick signature (entry names + mtimes) changes, so an external write — the agent's CLI, another session — shows up without user action.

## CLI

New subcommands on the existing binary; run with no subcommand, it launches the TUI as always. Every subcommand resolves the store from the current directory's git dir and exits 1 with one stderr line when that fails (not a git repo, `git` not on `PATH`).

```
herdr-reviewr comment add --file <path> --start <n> [--end <n>] [--side new|old]
                          [--lines <snippet>] [--author user|agent] --text <text>
herdr-reviewr comment list [--json] [--all]
herdr-reviewr comment resolve <id>
herdr-reviewr comment rm <id>
herdr-reviewr skill-path
```

| flag                          | default   | notes                                                              |
| ----------------------------- | --------- | --------------------------------------------------------------------- |
| `--file`, `--start`, `--text` | —         | required; an unrecognized flag or one missing its value is a usage error (exit 2) |
| `--end`                       | `--start` | a single-line comment needs only `--start`                            |
| `--side`                      | `new`     | `new` or `old`                                                         |
| `--lines`                     | `""`      | an agent note need not carry a snippet; the card still renders at `side:start-end` |
| `--author`                    | `agent`   | `user` or `agent`                                                      |

- `--start` must be `>= 1`; `--end` must be `>= --start`. Either violation prints a one-line reason to stderr and exits 2, same as a malformed invocation.
- `add` prints the new comment's `id` to stdout and exits 0.
- `list` defaults to open comments only; `--all` includes resolved ones too. Human output is one row per comment: `<id>  <status>  <author>  <file>:<start>-<end>  <first line of text>`. `--json` prints one JSON array of full comment documents (the same shape the store persists) instead.
- `resolve <id>` flips a comment to `resolved`; `rm <id>` deletes its file. Either on an unknown id exits 1, naming the id.
- `skill-path` prints the bundled `skills/reviewr-comments/SKILL.md` path — resolved from the running binary's install location, falling back to the dev-checkout path when run from a source checkout — and exits 0, or exits 1 naming both candidates it looked for.

## Behavior

- A user comment is created, edited, and deleted only from the TUI. An agent comment is created only by the CLI's `comment add`; the TUI never writes one.
- A comment is removed from the store only by an explicit delete: `d` in the TUI (either author's comment) or `comment rm` from the CLI. A refresh, a disk-store merge, or the agent's code edits never remove one.
- `x` (resolve/reopen) flips `status` in place; either side can flip either author's comment. A resolved comment stays present — dimmed, and with the hide-resolved toggle on, hidden — until it is deleted.
- Editing (`e`) works only on a user comment; on an agent comment it is a no-op that shows a status note instead (`tui.md`).
- Whether a user comment reaches the shared store immediately or only at send time is the `comment_sync` setting (`config.md`). An agent comment is always store-resident and always rendered, in both modes.
- A comment whose file leaves the changeset is flagged stale, and kept.
- An `All files` comment is flagged stale only when its file is deleted from the worktree.

### Export

One block per comment, to the agent input (the primary path) or the clipboard. Only open, user-authored comments export — an agent's own comments are never sent back to it, since it already has them.

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

- Send persists every open, user-authored comment to the store first — so a failed send never loses an `on-send`-mode draft that only ever lived in memory — then injects the blocks into the agent input and focuses the agent pane. It never submits; the user adds context and presses enter.
- Copy persists the same set, then writes the blocks to the system clipboard.
- Neither clears the list. Sending or copying is not resolving: every exported comment stays `open`, stays visible, and stays exportable again — a repeat send re-injects the same open set. Only `x` (resolve) or delete/`rm` end its life in the review.

How the agent pane is found and filled is in `herdr-host.md`.

## Failure semantics

- A failed send or copy leaves every comment exactly as it was: persistence happens before the export call runs, so the comment was already durable regardless of whether the export itself succeeds.
- Send/copy never consumes. A comment stays `open` after a successful export too, and a repeat send re-sends it.
- In `immediate` `comment_sync` (the default), a saved user comment survives closing the pane or restarting herdr — it was written to disk when saved. In `on-send`, a comment not yet sent is TUI-local only and is lost on close, same as upstream's pre-store behavior.
- When the store can't be resolved or written (not a git repo, a permission failure), the TUI shows a one-line notice and keeps comments session-local for that run; the CLI exits 1 naming the problem.
- Concurrent writers (the TUI and any number of agent CLI invocations) never corrupt each other's comments — each comment is its own file, added by exclusive create and mutated by tmp-file-then-rename. A same-instant status flip from both sides picks whichever rename lands last; both outcomes are `resolved`, so the race is benign. A `rm` racing a rewrite can resurrect a file only if the rewrite lands after the unlink — accepted.
- A corrupt comment file is skipped and logged, never deleted or auto-repaired.
- One TUI instance per worktree is assumed; the CLI is a short-lived subprocess and any number of invocations may run concurrently.

## Non-goals

- No reply threads — flat notes with resolve. No categories or severities. Text only.
- No outdated-tracking beyond the existing `stale` flag (file left the changeset, or was deleted).
- No line-number rebasing as the diff shifts. The snippet keeps a comment locatable.
- No auto-submit of the agent prompt.
- No cross-worktree or repo-global comment store — one store per git dir.
- No daemon, sockets, or live remote control of the TUI from the CLI.

## Related specs

- [configuration](./config.md)
- [diff-view](./diff-view.md)
- [tui](./tui.md)
- [herdr-host](./herdr-host.md)
