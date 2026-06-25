---
Status: Current
Created: 2026-06-23
Last edited: 2026-06-25
---

# herdr-review

herdr-review is a terminal review sidebar that runs in a herdr pane, where you browse a coding agent's changes, comment on line ranges, and send those comments back to the agent.

## Overview

The product is one binary (`herdr-review`, Rust + ratatui) in a right-hand herdr split pane, pointed at one git worktree. It never edits the worktree and sends nothing on its own; its only git write is a private `last-turn` baseline ref (`herdr-host.md`). It renders in your real terminal, so fonts and theming are whatever you already run.

A reviewer's loop:

```
open the pane → pick a changed file → read its diff → comment on a range
→ Send all your comments to the agent → add a line and hit enter
```

The end-state vision is a review cockpit: a file viewer (`All files`), a changes-and-diff reviewer (`Changes`), and a PR helper (`Checks`). This design covers the `Changes` tab and the review loop; the file viewer and PR helper are roadmap.

## Scope

In scope for this design:

- The `Changes` view: a changed-files list for a scope, plus a syntax-highlighted diff viewer (`diff-view.md`).
- Three scopes — `uncommitted`, `branch`, and `last-turn` — defined in `review-model.md`.
- Comments anchored to `path:start-end`, held in memory for the review pass.
- Export of all comments to the agent (filling its input) or to the clipboard.
- Poll-based refresh and a manual refresh key.
- Keyboard and mouse input, defined in `tui.md`.

## Roadmap

Named so the architecture stays open to them. None is part of this design.

- An `All files` tab that browses the whole repo tree, not only changed files.
- A `Checks` tab showing PR status and CI via `gh`, plus an aggregated comment list.
- Reviewed-file state — marking a file reviewed and greying it in the list.
- A side-by-side split diff view, for wide panes.
- Search within the diff, and live theme switching.

## Invariants

- The sidebar never commits, stages, or mutates the worktree, the index, or any branch; its one git write is the private `last-turn` baseline ref under `refs/reviewr/`.
- A comment, saved or being typed, is never lost to a refresh or the agent's edits; only you remove it.
- Comments leave only by an explicit export, to the agent pane or the clipboard.
- The crate forbids `unsafe`.

## Decisions

- Lightweight in-memory comments, sent to the agent — matches a few-comments-then-prompt loop; a durable, stateful comment store (Conductor-style) is more than this needs.
- Core covers `Changes` only — the tree and `Checks` carry fuzzier, heavier requirements, so the review spine ships first.

## Open decisions

- None.

## Related specs

- `./review-model.md`
- `./diff-view.md`
- `./file-list.md`
- `./tui.md`
- `./herdr-host.md`
