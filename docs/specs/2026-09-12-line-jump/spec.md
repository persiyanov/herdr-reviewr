# Line jump: `Ng`

Status: Approved
Date: 2026-07-11

## Problem

A reviewer who wants line 1337 of the open file scrolls. There is no way to name a line and go to it, vim/less-style.

## Proposal

While the **read pane** (the diff, or the All-files content pane) has focus, bare digits accumulate a line-number prefix: `1`, `3`, `3`, `7` holds `1337`. The prefix is consumed by the `g` binding — `1337g` jumps. A bare `g` (no prefix) keeps its current meaning: switch to the Commits scope. Because the prefix attaches to the *action* bound to `g` (not the glyph), a rebind of `scope-commits` carries the prefix with it.

Landing rule — identity first, clamp last, the same order Continuity uses:

| case | landing |
| ---- | ------- |
| a visible row at file line `N` | that row |
| no row at `N`, rows past `N` exist | the first visible row numbered `≥ N` |
| `N` hidden inside a collapsed fold | the fold expands, then the row for `N` |
| `N` past the last numbered line | the last row |

Line numbers are new-side numbers. A deletion row has none, so it inherits the position of the row above it; a fold marker sits at its first hidden line's number.

The prefix is typed input, so it obeys the modal rules already in force: every modal (pickers, find band, composer, list) owns its keys and returns before the prefix logic runs, so digits typed in a modal never accumulate, and any key that opens a modal drops a pending prefix first. Digits with the files pane focused are inert and do not accumulate. The prefix saturates at `u32::MAX`; the landing clamps, so a long count is harmless.

The markdown preview has no cursor, so `Ng` is inert while a preview is open.

Place state moves only under the user's own input: the jump is a keypress, and it sets the cursor plus the standing cursor-reveal flag, exactly as the other navigations (`j`/`k`, hunk steps, comment jump) do.

## Consequences

- `g` keeps its scope-switch meaning unmodified; the prefix is what changes.
- The jump lands on folds and deletions by position, not by identity — a refresh that reshapes the file does not re-target the cursor, per Continuity.
