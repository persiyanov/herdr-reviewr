---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-09
---

# herdr-reviewr

A terminal review sidebar in a herdr pane: browse a coding agent's changes, comment on line ranges, and send the comments back to the agent.

## Overview

One binary (`herdr-reviewr`, Rust + ratatui) runs in a herdr pane, pointed at one git worktree. It renders in the real terminal, so fonts and colors are whatever the user already runs.

The reviewer's loop:

```
open the pane → pick a changed file → read its diff → comment on a range
→ send the comments to the agent → add a line and hit enter
```

Three tabs:

| tab         | shows                                                                  |
| ----------- | ---------------------------------------------------------------------- |
| `Changes`   | the active scope's changed files, with a syntax-highlighted diff viewer |
| `All files` | the whole repo tree, with a read-and-comment content viewer             |
| `PR`        | a read-only mirror of the pull request: state, checks, comments         |

## Scope

- The `Changes` view: a changed-files list per scope plus the diff viewer (`diff-view.md`).
- The `All files` tab: a repo tree and content viewer, annotated with the active scope's changes (`file-list.md`, `diff-view.md`).
- The `PR` tab: pull-request identity, state, checks, and comments, read from GitHub, with external links only (`forge-host.md`, `tui.md`).
- Three scopes: `uncommitted`, `branch`, `last-turn` (`review-model.md`).
- Comments anchored to `path:start-end`, held in memory for the review pass.
- Export of all comments to the agent input or the clipboard.
- Poll-based refresh plus a manual refresh key.
- Keyboard and mouse input (`tui.md`).

## Roadmap

Named so the architecture stays open to them. None is part of this design.

- Reviewed-file state: marking a file reviewed and greying it in the list.
- Hopping between the agent's changed files while browsing `All files`.
- A side-by-side split diff view for wide panes.
- Search within the diff, and live theme switching.

## Invariants

| #  | Always true                                                                                                                  |
| -- | ----------------------------------------------------------------------------------------------------------------------------|
| O1 | The sidebar never commits, stages, or mutates the worktree, the index, or any branch. Its one git write is the private baseline ref under `refs/reviewr/`. |
| O2 | The sidebar never writes to GitHub. It reads the pull request through `gh` and opens links in the browser, nothing more.      |
| O3 | A comment, saved or being typed, is never lost to a refresh or the agent's edits. Only the user removes it.                   |
| O4 | Comments leave only by an explicit export, to the agent pane or the clipboard.                                                |
| O5 | The crate forbids `unsafe`.                                                                                                   |

## Related specs

- [review-model](./review-model.md)
- [diff-view](./diff-view.md)
- [theme](./theme.md)
- [file-list](./file-list.md)
- [tui](./tui.md)
- [herdr-host](./herdr-host.md)
- [forge-host](./forge-host.md)
