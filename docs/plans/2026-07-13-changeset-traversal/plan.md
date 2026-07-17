# Changeset traversal — Plan

Delivers `specs/tui.md#changeset-traversal` (supersedes PR #7).

## Goal

Hunk-level and file-level jumps so a reviewer reads the whole changeset without a focus round-trip: `]` / `[` step hunks across file boundaries, `f` / `F` skip files from either pane.

## Definition of Done

- [x] `]` / `[` jump the diff cursor to the nearest hunk first changed row below / above the cursor.
- [x] Past a file's outermost hunk, the first press arms the crossing and the second takes it, landing on the adjacent visible file's nearest hunk.
- [x] The armed crossing leads the footer as `] next file` / `[ prev file`, demoting the cursor's own action rather than hiding it.
- [x] The arm dies on any other input, and only arms when a file to cross to exists.
- [x] `]` / `[` cross over a file with no hunks, notice diffs included.
- [x] `]` / `[` do nothing in `All files`, in a markdown preview, and with no open diff.
- [x] `f` / `F` from the diff open the adjacent file, cursor on its first row, focus kept.
- [x] `f` / `F` from the list move the cursor to the nearest file row, skipping directories.
- [x] `f` / `F` land on notice diffs, and from a preview the opened file starts in source.
- [x] A cross or skip from the diff moves the list selection onto the opened file.
- [x] With no target in the pressed direction, a press does nothing.
- [x] Both pairs do nothing on the `PR` tab, during a live selection, and under the comments list.
- [x] `<` / `>` move the pane divider the way the key points. `]` / `[` no longer resize.
- [x] `[keybindings]` rebinds `next-hunk`, `prev-hunk`, `next-file`, `prev-file`.
- [x] The README keybindings table shows the new defaults.

## Out of Scope

- Answering PR #7 (credit, review comment, close-or-rebase). A user call, after this lands.
- Reviewed-state and next-unreviewed navigation. Roadmap (`specs/file-list.md`).

## Execution Plan

1. [x] `src/keymap.rs`: add `Action::{NextHunk, PrevHunk, NextFile, PrevFile}` to `ACTIONS` with names and defaults, move `list-wider` / `list-narrower` to `>` / `<`.
2. [x] `src/app.rs`: file skips — `next_file` / `prev_file` over `file_rows`, per PR #7's shape, rebuilt against the current tree.
3. [x] `src/app.rs`: hunk steps — scan the open `FileDiff` rows for change-run starts, cross by advancing the file until one has hunks, with the tab, preview, and selection guards.
4. [x] `src/lib.rs` `handle_key`: dispatch the four actions.
5. [x] `tests/app_flow.rs`: file-skip test (PR #7's scenarios), hunk tests — within file, crossing, pass-over, clamp, each guard.
6. [x] `README.md`: keybindings rows for `]` `[` `f` `F`, resize row to `>` `<`.
7. [x] Release notes: name the resize rebind, since an old config binding `]` or `[` now errors as a collision.

## Likely Files

| file                 | change                                        |
| -------------------- | --------------------------------------------- |
| `src/keymap.rs`      | four new actions, resize defaults             |
| `src/app.rs`         | hunk-step and file-skip cursor moves          |
| `src/lib.rs`         | key dispatch for the four actions             |
| `tests/app_flow.rs`  | traversal and guard tests                     |
| `README.md`          | keybindings table                             |
| `CHANGELOG.md`       | the feature and the resize rebind             |

## Verification

- `just test` → all green, new traversal tests included.
- `cargo clippy` → clean.
- Live run in a worktree with staged edits → `]` walks hunks across files, `f` skips, `>` resizes.
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- Gate: promote `specs/tui.md` and `specs/file-list.md` to Current.

## Replan

- If the row model can't name hunk starts cheaply, then derive them in `src/diff.rs` beside fold construction.
- 2026-07-13: initial plan.
- 2026-07-13: spec walk on the revised traversal → strict nearest-hunk rule, visible-order adjacency, selection tracking, no-target and comments-list guards landed in `specs/tui.md`.
- 2026-07-13: review found the traversals stepping from the file cursor, which wraps the diff back into the open file when the cursor is parked on a directory → both now step from the open file (`App::open_file_row`), covered by a test.
- 2026-07-13: live testing → a file crossing takes two presses of `]` / `[`, the footer offers it (`FooterAction::CrossFile`), and the resize keys swapped so `<` moves the divider left. Landed in `specs/tui.md`.
