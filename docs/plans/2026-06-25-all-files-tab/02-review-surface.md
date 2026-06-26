# Milestone 02: Review surface

**Plan:** ./main.md · **Specs:** ../../../specs/ — the living reference this plan delivers

> Mapped shallowly. Detailed after M1's gate, once the per-tab state shape is proven — M2's per-tab comment and selection state depends on it.

## Goal

Turn the `All files` browser into a full review surface: comment on any line range, see the active scope's changes annotated in the tree, and switch tabs with the current file carrying over.

## Why This Comes Next

It builds directly on M1's proven contract — per-tab state, the File view, and the navigator. Nothing here is a new unknown; it is the contract's payoff.

## Scope Sketch

Detailed at the gate; listed here for orientation, drawn from M1's Out of Scope.

- Comment in the File view: selection and the editor reuse the diff path; a file-content comment is `Side::New` with space-prefixed (context) snippet lines, exporting identically to a context comment (`review-model.md`).
- Annotate the `All files` tree: changed files in the active scope show their marker and `+a −d` stats; switching scope re-marks in place, preserving cursor, scroll, and expanded directories (`file-list.md`).
- Seed a tab switch: switching into a tab with no selection seeds it from the current file, carrying the cursor line and revealing it comfortably in view; a file that cannot be seeded leaves `All files` empty (`tui.md`).
- File-content comment staleness: flagged stale only when its file is deleted from the worktree (`review-model.md`).

## At the Gate (= merge gate)

M2 completes every Draft spec this plan touches. All five — `overview.md`, `file-list.md`, `diff-view.md`, `review-model.md`, `tui.md` — are verified against the shipped code and promoted Draft → Current.
