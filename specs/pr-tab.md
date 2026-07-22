---
Status: Current
Created: 2026-07-17
Last edited: 2026-07-21
---

# PR tab

A read-only mirror of the pull request in the sidebar's two-pane frame: identity in the header, checks and comments in the navigator, the selected body in the read pane.

## Overview

The navigator shows checks and selects the description or a comment. The read pane shows that selection. The header carries the PR's identity and state. The tab reads GitHub through `forge-host.md` and writes nothing. Its only outward action opens a link in the browser.

```
 1 Changes  2 All files  3 PR    Deep research: GPT-5.5/5.4-mini upgrade…  deep-research  merged #226 ↗
╭─ @codex · manager.py:115 ──────────────────────────╮╭─ Checks & comments ──────────╮
│ -    if primary_result.status == PERM_FAILURE:        ││ description                  │
│ -        return primary_result                        ││                              │
│                                                       ││ checks  ✗ 1 failing          │
│ Avoid falling back after target permanent failures.   ││  ✓ build-main-image          │
│ This now attempts a fallback for every non-success…   ││  ✗ tests                     │
│                                                       ││                              │
│                                                       ││ comments · 5                 │
│                                                       ││ @you    comment          5m  │
│                                                       ││▍@codex  manager.py:115   2h  │
│                                                       ││ @claude review           2h  │
│                                                       ││ @claude manager.py:39    2h  │
│                                                       ││ @claude parse.py:187 outdated│
╰───────────────────────────────────────────────────────╯╰─────────────────────────────╯
 ⚠ conflicts with main · ⇡ 2 unpushed · ✗ 1 failing · 5 comments   o open ↗                            ?
```

## Behavior

### Header and footer

- The header right-anchors a clickable `status #226 ↗` chip, status colored by lifecycle: `open` green, `draft` yellow, `merged` mauve, `closed` red. The PR title sits to its left, truncated to fit.
- Between title and chip sits the resolved head branch (`head_ref`, `forge-host.md`), dim, prefixed `⑂ ` when the head lives in a fork. On a narrow bar the branch drops first.
- The footer leads with merge, sync, checks, and comment counts, then `o open ↗` and the `?`. Merge and sync show only while the PR is open. A capped surface appends `+more on GitHub ↗` (`forge-host.md`).
- The `?` expands to the `go` band and a `move` band of down, up, and the page keys. The `PR` tab has no hunk or file steps (`input.md`).
- The ordinary no-PR body says only `No pull request yet. Ready to ship?` A detached HEAD says `No pull request found — HEAD is detached.`

### Navigator and read pane

- The navigator, titled `Checks & comments`, shows a status-only checks section above the comments list. The cursor walks the description row and the comments.
- Comments list newest first, each row `@author anchor age`, with `outdated` or `resolved` markers where GitHub receded the thread.
- A non-empty PR description pins a `description` row at the top of the navigator, above the checks. An emptied description vanishes like a comment: the cursor clamps, the read pane resets.
- The read pane shows the selected comment: a finding shows its `snippet` then the body, a review or plain comment shows its prose, the description row shows the PR description.
- Bodies render as markdown (`markdown.md`). A finding's `snippet` stays plain `+`/`−`-colored lines.
- A human author is emphasized over the bots.
- `j`/`k` or a click selects a description or comment and reveals it in the navigator viewport. Checks are not selectable.
- The wheel over the navigator scrolls its viewport without changing the selection. The wheel over the read pane scrolls the read pane. `PageUp`/`PageDown` scroll the focused pane. Both panes stop with their last line at the bottom edge.
- `o` or the chip opens the PR in the browser.
- A body taller than the read pane shows a scrollbar on the pane's right border. One that fits shows none.
- A retry notice for a preserved snapshot stays fixed above the read body, so it remains visible without resetting the reader's scroll.
- The authoring keys (`s`, `c`, `v`, `d`, `e`) do nothing here.
- A merged or closed PR shows the same mirror, read-only.
- No usable `gh` shows the matching failure state from `forge-host.md`, naming the command that unblocks it.

### Refresh

- The tab fetches on open, on entering the tab, on `r`, and on the agent's turn-end on any tab, with a slow fallback timer while active. One fetch per turn keeps the tab fresh before it is entered. Its cadence is separate from the worktree poll (`tui.md`).
- A refetch keeps your place: the cursor follows the selected comment by identity, and both pane scroll positions hold. A vanished comment clamps the cursor and resets the read pane.

## Non-goals

- No jump from a PR comment's anchor to the code tabs.

## Related specs

- [forge-host](./forge-host.md)
- [tui](./tui.md)
- [input](./input.md)
- [markdown](./markdown.md)
