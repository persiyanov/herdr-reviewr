# Bidirectional agent/user comments

**Date:** 2026-07-11
**Status:** Approved
**Repo:** `dcieslak19973/herdr-reviewr`, branch `agent-comments`

## Goal

A hunk-style comment loop between the reviewer and the coding agent, inside the reviewr
sidebar: the user leaves anchored notes the agent can read and act on; the agent leaves
anchored notes the user sees as cards in the diff; either side resolves what's addressed.

Today comments are TUI-memory only and leave once, flattened into the agent's input by
`s`. This feature adds a persistent, worktree-scoped comment store that both sides
read and write — the agent through new CLI subcommands on the existing binary, the user
through the TUI as before.

## Constraints and context

- No daemon, no sockets, no new crate dependencies. The store is files; the agent's
  access is the already-installed binary (`$HERDR_PLUGIN_ROOT/bin/herdr-reviewr`) run as
  a subprocess by the agent itself.
- The store must never dirty the diff under review, so it lives under the **git dir**,
  not the worktree.
- Upstream's write invariant loosens by exactly one location: reviewr writes only under
  the repo's git dir — the `refs/reviewr/` baseline refs (existing) and the comment
  store (new) — and never the worktree.
- The comment anchor contract from `specs/review-model.md` is unchanged: the verbatim
  snippet (`lines`) is authoritative; `side`/`start`/`end` orient a human and are never
  re-bound.

## Store

One JSON file per comment: `<git-dir>/reviewr/comments/<id>.json`, where `<git-dir>` is
`git rev-parse --git-dir` resolved from the repo — per-worktree automatically, since
each linked worktree has its own git dir. Per-comment files make concurrent writers
(TUI and agent CLI) safe with no locking: `add` is an exclusive file create, mutation
rewrites only that comment's file via tmp + rename, delete is an unlink.

A comment document is the existing review-model comment plus lifecycle fields:

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

| field | type | meaning |
|---|---|---|
| `id` | string | `c-<epoch-ms>-<4 hex>` — unique, sortable by creation |
| `author` | enum | `user` (TUI) or `agent` (CLI default) |
| `status` | enum | `open` or `resolved` |
| `created_at` | string | ISO-8601 `…Z` |
| remaining fields | — | exactly `specs/review-model.md`'s comment object |

Unknown fields are preserved on rewrite (read as `serde_json::Value`, mutate keys,
write back). A file that fails to parse is skipped with a log line, never deleted.

## CLI

New subcommands on the existing binary; with no subcommand it launches the TUI as
today. All subcommands resolve the store from the cwd's git dir and exit non-zero with
one stderr line when not in a git repo.

```
herdr-reviewr comment add --file <path> --start <n> [--end <n>] [--side new|old]
                          [--lines <snippet>] [--author user|agent] --text <text>
herdr-reviewr comment list [--json] [--all]     # default: open only, human-readable
herdr-reviewr comment resolve <id>
herdr-reviewr comment rm <id>
herdr-reviewr skill-path                        # prints the bundled SKILL.md path
```

- `add` defaults `--end` to `--start`, `--side` to `new`, `--author` to `agent`,
  `--lines` to empty (an agent note need not carry a snippet; the TUI renders the card
  at `side:start-end` regardless).
- `list --json` emits one JSON array of full comment documents. Human output is
  `<id>  <status>  <author>  <file>:<start>-<end>  <first line of text>`.
- `resolve` and `rm` on an unknown id exit non-zero naming the id.
- `skill-path` resolves the SKILL.md relative to the plugin checkout root (the
  binary's parent's parent, matching how the manifest lays out `bin/`), falling back
  to an error naming where it looked.

## TUI

- **Load and watch.** The store is read at startup and re-read when the store
  directory's contents change, checked on the existing event tick (a cheap directory
  stat — mtime + entry count). External changes (agent adds/resolves) appear without
  user action.
- **Rendering.** Agent comments render as cards in the diff pane like user comments,
  visually distinct (an `agent` label chip and a different accent from the theme's
  existing palette). Resolved comments dim; a toggle hides them entirely.
- **Keys.** Existing comment keys keep working (`c` create, `e` edit, delete). New:
  a resolve-toggle on the selected comment (any author), and the hide-resolved toggle.
  Footer key-hints update as usual.
- **Sync modes.** New config key, following the existing config contract:

  ```toml
  comment_sync = "immediate"   # default; or "on-send"
  ```

  - `immediate`: every user comment persists to the store on save; edits and deletes
    propagate. The hunk-style flow — the agent can be told "address my review
    comments" at any time.
  - `on-send`: user comments stay TUI-local until `s`, which persists them and then
    exports as today. Preserves the upstream "nothing leaves without a keystroke"
    posture.
  - Agent comments are always store-resident and always rendered, in both modes.
- **Send (`s`).** Unchanged format (`path:start-end — comment`). In `immediate` mode
  send is a nudge — the comments are already in the store; sending additionally does
  not duplicate store entries (comments persist once, keyed by id).

## Skill and docs

- `skills/reviewr-comments/SKILL.md` ships in the plugin checkout: how to find the
  binary (`$HERDR_PLUGIN_ROOT/bin/herdr-reviewr` or `herdr-reviewr skill-path` from
  the repo), the loop (list open comments → address each → `comment resolve <id>` →
  leave your own notes with `comment add` at the file:line you changed), and the
  anchor rule (trust the snippet over the line number after edits).
- README gains a "Working with agents" section mirroring hunk's: the generic prompt
  ("run `herdr-reviewr skill-path`, load that skill, then review this code and leave
  comments"), the reverse flow ("implement the comments I left in reviewr"), and the
  `comment_sync` choice.
- Spec updates: `specs/review-model.md` (store, lifecycle fields, CLI), `specs/config.md`
  (`comment_sync`), `specs/overview.md` invariant wording (git-dir writes only).

## Error handling

- Store unreadable/uncreatable → TUI shows a one-line notice in the comment area and
  falls back to TUI-local comments (the current behavior); CLI exits non-zero with the
  reason.
- Concurrent mutation of the same comment (rare: both sides resolve at once) —
  last rename wins; both outcomes are "resolved", so the race is benign. `rm` racing a
  rewrite can resurrect a file only if the rewrite lands after the unlink; accepted.
- A comment whose `file` no longer exists in the current scope still renders in the
  comment list (the diff pane shows it when its file is on screen); nothing is
  auto-deleted.

## Testing

- Store: round-trip, id uniqueness, unknown-field preservation, corrupt-file skip —
  unit tests against a tempdir.
- CLI: integration tests spawning the binary in a tempdir git repo (the existing
  `tests/` pattern) covering add/list/resolve/rm/skill-path and the not-a-repo error.
- TUI: `tests/render.rs`-style snapshot coverage for agent cards, resolved dimming,
  and the hide toggle; app-flow tests for both `comment_sync` modes and external-change
  pickup (write a store file mid-test, tick, assert render).

## Non-goals

- No reply threads — flat notes with resolve.
- No daemon or live TUI remote control (hunk's `session navigate/reload` surface).
- No auto-rebinding of anchors when lines shift (upstream rule kept).
- No cross-worktree or repo-global comment store.
- No change to what `s` sends or to the herdr input channel.
