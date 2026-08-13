---
Status: Draft
Created: 2026-08-13
Last edited: 2026-08-13
---

# CLI navigation

`herdr-reviewr nav` steers the running sidebar from outside — a script or a coding agent puts a
specific diff on screen without synthesizing keypresses into the pane.

## Overview

The channel is a one-shot command file, not a socket or stdin: the TUI owns its terminal, and a
listener would add a lifecycle to manage for a feature whose whole job is "apply this once". The
CLI writes the file; the sidebar's event loop takes it (read + delete) on its next wakeup and
applies it. The file lives in the system temp dir, keyed by a hash of the canonical repo path —
inside the repo it would dirty the very worktree under review.

```json
{ "tab": "changes", "scope": "branch", "file": "src/lib.rs" }
```

| field   | type   | meaning                                                              |
| ------- | ------ | -------------------------------------------------------------------- |
| `tab`   | string | `changes`, `all`, or `pr`; also the key spellings `1` `2` `3`         |
| `scope` | string | `uncommitted`, `branch`, or `last-turn`; also the keys `u` `b` `t`    |
| `file`  | string | path to open, stored repo-relative; see the re-rooting rule below     |

Every field is optional; present ones apply in tab → scope → file order, so one command lands on
"this file, in this scope, on this tab". Fields hold the CLI spellings and parse on apply — a
stale file from a newer CLI degrades to a status-line error, never a crash.

The CLI re-roots the `file` argument to repo-relative: an absolute path lexically, a relative
one against the cwd when that resolves to a real file inside the repo — so `nav foo.rs` works
from any subdirectory. A path that resolves nowhere passes through as repo-relative, so a
deleted file, which exists only in the diff, stays nameable. The alternative — cwd-relative
always — lost because it breaks the deleted-file case and every script that already speaks
repo-relative paths.

## Invariants

| code              | Always true                                                                     |
| ----------------- | ------------------------------------------------------------------------------- |
| `NAV-AT-MOST-ONCE`| A command applies at most once: the take deletes the file before applying.       |
| `NAV-NORMAL-ONLY` | A command applies only in `Normal` mode — it never destroys a composed comment, a search, or an open overlay. |
| `NAV-NON-UI`      | A `nav` invocation never counts as the review UI: excluded in `main.rs` dispatch and `is_reviewr_pane`, the two halves of Pane identity (`herdr-host.md`). |
| `NAV-REPO-KEYED`  | Sidebars on different repos never cross: the command path is derived from the canonical repo root. |
| `NAV-LATENCY`     | The idle event wait is capped at 300 ms, so a command lands within that bound rather than the refresh interval's seconds. |

## Behavior

| command                                  | outcome                                                            |
| ---------------------------------------- | ------------------------------------------------------------------ |
| valid tab / scope / file                 | applied in order; the status line names the opened file            |
| `file` not in the active tab's entries   | nothing moves; the status line names the file and the scope        |
| `file` with `tab = pr`                   | tab switches; the file part reports "needs a file tab"             |
| unknown spelling in any field            | that part reports in the status line; the others still apply       |
| any command while not in `Normal` mode   | dropped whole, reported in the status line (`NAV-NORMAL-ONLY`)     |
| CLI run with no field at all             | refused at the CLI with a usage error; nothing is written          |

The CLI validates the spellings it can check locally and exits after writing — it never waits
for the apply. The outcome surfaces only in the sidebar's status line: a round-trip
acknowledgement would need the socket this design rejected.

## Failure semantics

A command written while no sidebar runs waits in the temp dir and applies when the next sidebar
on that repo starts. The alternative — expiring stale commands — lost because a scripted
open-then-navigate must survive the sidebar's startup time; the surprise is bounded to one
command, since the take deletes the file. An unreadable or unparsable file is deleted the same
way and dropped.

## Non-goals

- No cursor or line targeting. The unit of navigation is the file; finer targets belong to a
  future command only if a real workflow needs them.
- No state readback. The CLI writes; it never queries what the sidebar shows.
- No multi-command queue. A second write before the take replaces the first — last writer wins.

## Related specs

- [herdr-host](./herdr-host.md) — Pane identity, which `NAV-NON-UI` extends.
- [input](./input.md) — the in-pane keys these commands mirror.
- [tui](./tui.md) — tabs, scopes, and the status line.
