# All files tab — Delivery Strategy

**Specs:** ../../../specs/ — the living reference this plan delivers

## Goal

Add a second tab, `All files`, that browses the whole worktree and turns it into a full review surface: read any file, comment on any line range into the one comment set, see the active scope's changes annotated, and switch tabs with the current file carrying over. The feature reuses the diff pane, navigator, and comment machinery; its one genuinely risky contract is per-tab state, since `App` holds a single set of cursor/scroll/view fields today.

## Milestone Map

1. **All files browser** — browse the whole repo in a new tab and read any file's content, with each tab keeping its own selection and scroll. A read-only walking skeleton that proves the tab machinery, the per-tab state model, and the File view (an all-`context` `FileDiff`) reusing the diff pane. Builds on the baseline repo. Ends on an **information boundary**: the per-tab state shape determines how the review surface holds per-tab comment and selection state, so it is validated before M2 depends on it.
2. **Review surface** — comment on any line (joining the one in-memory set), annotate the tree with the active scope's changes and re-mark in place, and seed a tab switch from the current file carrying the cursor line. Builds on M1's proven contract. Ends at the merge gate.

## Current Milestone

`01-all-files-browser.md`

## Deferred Decisions

- None. The provisional tab keys (`1` / `2`) live under the keymap Open Decision in `tui.md`.

## Replan Log

- 2026-06-25: initial strategy from the approved Draft specs.
- 2026-06-26: M1 built and reviewed. A focused review of the diff caught two regressions from the `files`→`entries` rename — the header count and `stale_files()` read the active tab's list, wrong on `All files`. Fixed by computing the scope changeset every reload regardless of tab (`changed_paths`), pinned by a test; M1's exit state gains `changed_paths` / `changed_count()`.
