---
Status: Draft
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

The end-state vision is a review cockpit: a changes-and-diff reviewer (`Changes`), a whole-repo file browser (`All files`), and a PR helper (`Checks`). This design covers the `Changes` and `All files` tabs and the review loop; the PR helper is roadmap.

## Scope

In scope for this design:

- The `Changes` view: a changed-files list for a scope, plus a syntax-highlighted diff viewer (`diff-view.md`).
- The `All files` tab: a whole-repo file tree with a read-and-comment content viewer, annotated with the active scope's changes (`file-list.md`, `diff-view.md`).
- Three scopes — `uncommitted`, `branch`, and `last-turn` — defined in `review-model.md`.
- Comments anchored to `path:start-end`, held in memory for the review pass.
- Export of all comments to the agent (filling its input) or to the clipboard.
- Poll-based refresh and a manual refresh key.
- Keyboard and mouse input, defined in `tui.md`.

## Roadmap

Named so the architecture stays open to them. None is part of this design.

- A `Checks` tab showing PR status and CI via `gh`, plus an aggregated comment list.
- Reviewed-file state — marking a file reviewed and greying it in the list.
- Hopping between the agent's changed files while browsing `All files`.
- A side-by-side split diff view, for wide panes.
- Search within the diff, and live theme switching.

## Invariants

- The sidebar never commits, stages, or mutates the worktree, the index, or any branch; its one git write is the private `last-turn` baseline ref under `refs/reviewr/`.
- A comment, saved or being typed, is never lost to a refresh or the agent's edits; only you remove it.
- Comments leave only by an explicit export, to the agent pane or the clipboard.
- The crate forbids `unsafe`.

## Decisions

- Lightweight in-memory comments, sent to the agent — matches a few-comments-then-prompt loop; a durable, stateful comment store (Conductor-style) is more than this needs.
- `All files` is a content browser you can comment in, not a second diff — it renders whole-file content and overlays the active scope's change markers, reusing the diff viewer and the navigator rather than a separate stack. Rejected: a read-only browser with no commenting.
- One comment set across tabs — a comment made in `All files` and one in `Changes` share the in-memory list and export together, so a review pass is one set, not one per tab. Rejected: per-tab comment lists.
- `Checks` stays roadmap — the PR helper carries fuzzier, heavier requirements (`gh`, CI, an aggregated comment list) than the review spine and the file browser. Rejected: bundling it into this design.

## Open decisions

- None.

## Related specs

- `./review-model.md`
- `./diff-view.md`
- `./file-list.md`
- `./tui.md`
- `./herdr-host.md`
