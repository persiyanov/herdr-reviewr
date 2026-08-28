# Worktree-private pick and last-turn: Plan

Delivers the sibling `spec.md`.

## Problem

The `branch` base is one blob at `refs/reviewr/base-pick`. Git shares that ref across a clone's worktrees, so picking `A` on pane `B` retargets pane `C`. Issue #83. Last-turn is already per worktree, but keyed by a path hash in the same shared store, so move/remove/reuse misbehave.

## Goal

One private namespace, `refs/worktree/reviewr/`, for the pick and the last-turn tree. A new worktree inherits nothing. Enter records the spelling, including the default branch.

## Ticket Map

1. **Cut over both refs.** Linked-worktree isolation, persist, no inherit from `refs/reviewr/`, default row writes the name, no-writes. Blocked by: none.

One ticket. The spec is one namespace. Splitting pick from last-turn would leave the hash dialect in the tree.

## Out of Scope

- Inferring a parent, persisting the commit pick, an unpick gesture, deleting leftover `refs/reviewr/` names. Spec Out of scope.

## 1. Cut over both refs

**What to build:** Pane `B` can pick `A` while pane `C` on another worktree stays on `origin/HEAD`. Last-turn on `B` is invisible on `C`. Restart on the same worktree keeps both facts. Picking the default row writes that name.

**Blocked by:** none

**Status:** done

### Acceptance criteria

- [x] Main vs linked, both directions, and two linked worktrees: a pick written in B is none in C (`WT-PICK-PRIVATE`).
- [x] Same pairs for last-turn (`WT-TURN-PRIVATE`).
- [x] A fresh process on the same worktree rereads the pick and the last-turn tree.
- [x] A planted `refs/reviewr/base-pick` does not win when the worktree ref is absent. `origin/HEAD` does (`WT-NO-INHERIT`).
- [x] A planted `refs/reviewr/turn-base/<hash>` is not this worktree's baseline (`WT-TURN-NO-INHERIT`).
- [x] After write, this worktree's `refs/worktree/reviewr/` names are exactly `base-pick` and `turn-base`. The set of `refs/reviewr/` names is unchanged (`WT-NO-HASH`).
- [x] Choosing the picker's default row stores that branch name.
- [x] `clear_base_pick` is gone. Enter always `update-ref`s the spelling.
- [x] After pick and last-turn persist, `git status --porcelain` is empty and no `refs/heads/` name moved.

### Implementation plan

1. Add a linked-worktree fixture in `tests/git_repo.rs` (or `tests/common/mod.rs`) and the isolation tests first, against today's shared ref. They fail. That is the proof the rest of the ticket is for.
2. Point `BASE_PICK_REF` at `refs/worktree/reviewr/base-pick`. Drop `clear_base_pick`. `base_picker_pick` always writes. Retarget `tests/common/mod.rs` `write_raw_base_pick` and every `refs/reviewr/base-pick` assertion.
3. Point the last-turn ref at `refs/worktree/reviewr/turn-base`. Stop hashing the path. Delete `worktree_key` if nothing else calls it. Retarget `tests/git_repo.rs` baseline tests.
4. README Base branch, `AGENTS.md` No writes: `refs/worktree/reviewr/`.

### Tests

- `a_pick_in_one_worktree_is_invisible_in_its_sibling`: WT-PICK-PRIVATE
- `a_turn_baseline_in_one_worktree_is_invisible_in_its_sibling`: WT-TURN-PRIVATE
- `the_pick_persists_in_a_private_ref_and_clears` rewritten: persist, no clear, worktree ref only
- `a_planted_shared_pick_is_not_this_worktrees_pick`: WT-NO-INHERIT
- `a_planted_hash_baseline_is_not_this_worktrees_last_turn`: WT-TURN-NO-INHERIT
- `choosing_the_default_row_records_the_name`: WT-RECORD-DEFAULT, replaces the current "clears" assertion in `tests/app_flow.rs`

### Verification

- `cargo test --test git_repo --test app_flow` green.
- `just ci` green.
- WT-NO-WRITES → the persist test's `status --porcelain` and `for-each-ref` assertions.

## Replan

- If `git worktree add` in `tests/git_repo.rs` is too slow or flaky on CI, keep the isolation tests and drop only the extra worktree setup that is not proving an invariant.
- 2026-08-28: initial plan.
