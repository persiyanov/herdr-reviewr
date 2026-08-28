# Worktree-private pick and last-turn

Status: Draft
Date: 2026-08-28

Issue: [#83](https://github.com/persiyanov/herdr-reviewr/issues/83)

## Problem

A herdr reviewer has one worktree and one reviewr pane per branch. A stack `main ← A ← B ← C` is four panes on one clone. That is the plugin's steady state.

The `branch` scope's base is one blob at `refs/reviewr/base-pick`. Git shares that ref across linked worktrees. Picking `A` on `B` writes the one ref. `C` rereads it on the next refresh and diffs against `A` too.

The 2026-08-08 picker chose that sharing so a clone whose trunk is `dev` could pick `dev` once (issue #42). It fails as soon as two panes need two parents.

Last-turn is already a worktree fact, stored the homemade way: `refs/reviewr/turn-base/<FNV-1a of the absolute path>` in the shared store. `git worktree move` loses it. `git worktree remove` leaks it. A new worktree at the same path sees a ghost snapshot.

Neither fact is a diff. The pick is a spelling blob. Last-turn is a tree object. The live diff is computed against `HEAD` or the worktree.

## Proposal

Both facts live in git's per-worktree namespace.

| fact | ref | object |
| ---- | --- | ------ |
| base pick | `refs/worktree/reviewr/base-pick` | blob of one printable spelling |
| last-turn baseline | `refs/worktree/reviewr/turn-base` | tree of the snapshot |

Read and write those names with `git -C` this worktree. Do not poke `$GIT_DIR`. A spelling written in the main worktree is invisible from a linked worktree of the same clone, and the other way around.

Observable:

- Open `B`, pick `A`. Header `vs A`. Open `C`, pick `B`. Header `vs B`. `B`'s pane still reads `vs A`.
- Close `B`'s pane and reopen it. Header still `vs A`.
- Add a new worktree `D` and open a pane. It has no pick and no last-turn. `branch` follows `origin/HEAD`. `last-turn` is empty until a change-producing turn on `D`.
- Two panes on one worktree share the pick and the last-turn tree. Comments stay per pane and in memory.
- First launch on a worktree with no `refs/worktree/reviewr/base-pick` follows `origin/HEAD`. A leftover `refs/reviewr/base-pick` does not count as a pick. After any pick, including the default branch, that spelling is recorded. Picking `main` writes `main`. The pane does not follow a later `origin/HEAD` retarget.
- An upgrade does not copy old refs. `refs/reviewr/base-pick` and `refs/reviewr/turn-base/<hash>` stay on disk and are never read. The reviewer re-picks once per worktree. Last-turn fills on the next change-producing turn there. Rolling back to the previous binary restores those leftover refs, not the worktree refs just written. Two panes on one worktree during `qa-install`, one old binary and one new, do not share a pick.

Resolve chain, first hit wins, skip never error:

1. `--base`
2. this worktree's pick
3. `origin/HEAD`

The picker is unchanged except the default row. The default branch stays listed even when it is checked out, so it can be picked. Choosing it records that name. The `default` marker stays so the clone's default is visible. Enter always writes the highlighted spelling. There is no clear.

The pick still applies only after its ref write lands. A base change still rebuilds the changeset on the next frame. Spelling shape, origin-then-local branch resolve, SHA pins, and `HEAD~1` re-resolve stay.

Last-turn capture stays. The ref is exactly `refs/worktree/reviewr/turn-base`. There is no path hash. Isolation is the ref.

No writes: reviewr never commits, stages, or mutates the worktree, the index, or any branch. Its only git writes are private refs under `refs/worktree/reviewr/`. Leftover `refs/reviewr/` names from earlier releases are unread and unwritten.

### Decisions

- **Owner is the worktree.** The pane reviews this checkout. A stack of panes needs a stack of parents. A branch-keyed store trips on detach and rename. Pane-local memory dies on reopen.
- **Slot is `refs/worktree/`.** Git already isolates it across move and remove. A path hash under `refs/reviewr/` copies last-turn's ghosts.
- **No clone default.** An unpicked worktree follows `origin/HEAD`. A leftover shared pick is the stomp. The picker has one Enter and cannot mean two facts.
- **Enter records, including the default name.** The list is names. `main` and `develop` are the same kind of row. Unpicked is only "never picked on this worktree."
- **Cut over.** Do not read or copy `refs/reviewr/base-pick` or `refs/reviewr/turn-base/<hash>`. Old panes still running during `qa-install` keep using them. New panes start clean.
- **One namespace for both facts.** Last-turn moves with the pick so the homemade hash does not remain as a second dialect.

## Invariants

Each one is false if a single test below is red.

| code | Always true | Enforcement |
| ---- | ----------- | ----------- |
| WT-PICK-PRIVATE | `git -C` worktree C cannot `cat-file blob refs/worktree/reviewr/base-pick` after a write in B. Both directions: main vs linked, and two linked. | `tests/git_repo.rs` |
| WT-TURN-PRIVATE | `git -C` worktree C cannot `rev-parse refs/worktree/reviewr/turn-base` after a write in B. Same pairs as WT-PICK-PRIVATE. | Same fixture. |
| WT-PICK-PERSIST | A new process on the same worktree reads the spelling the last process wrote. | Existing persist test, retargeted at the worktree ref. |
| WT-TURN-PERSIST | A new process on the same worktree reads the last-turn tree the last process wrote. | Existing baseline persist test, retargeted. |
| WT-NO-INHERIT | A worktree with no `refs/worktree/reviewr/base-pick` ignores a planted `refs/reviewr/base-pick` and, with no `--base`, takes `origin/HEAD` when that resolves. | `resolve_base` on a linked worktree that never picked. |
| WT-TURN-NO-INHERIT | A worktree with no `refs/worktree/reviewr/turn-base` ignores a planted `refs/reviewr/turn-base/<hash>`. The last-turn baseline is unset. | Same fixture, plant the old hash ref. |
| WT-NO-HASH | After a pick and a last-turn write, this worktree's `refs/worktree/reviewr/` names are exactly `base-pick` and `turn-base`. The set of `refs/reviewr/` names is unchanged from before those writes. | Persistence test plus last-turn write. |
| WT-RECORD-DEFAULT | Choosing the picker's default row leaves a blob whose bytes are that branch name. | `tests/app_flow.rs` default-row pick. |
| WT-WRITE-THEN-APPLY | The changeset and header use a spelling only after `update-ref` for that worktree's pick succeeded. | Existing apply-after-write test. |
| WT-NO-WRITES | After pick and last-turn persist, `git status --porcelain` is empty and no `refs/heads/` name was created or moved. | Existing no-writes assertion, new ref names. |

## Alternatives

- **Keep one shared `refs/reviewr/base-pick`.** One `develop` pick serves every pane. Two stacked panes cannot both be right. That is today's bug.
- **`refs/reviewr/base-pick/<path-hash>` for the pick, last-turn unchanged.** Same owner, homemade slot. Move loses the pick. Remove leaks. Same-path reuse ghosts.
- **Worktree pick, then the old shared ref as clone default, then `origin/HEAD`.** Unpicked `C` inherits `B`'s parent. The stomp as a fallback. The picker cannot set a default as a distinct fact.
- **Copy last-turn from the path-hash ref on first read.** One-release glue. The next turn writes a fresh tree anyway.
- **In-memory pick, like the commit pick.** Reopen loses the parent. The commit pick is a reading position. The base is a worktree fact.
- **`git config --worktree`.** Needs a shared `extensions.worktreeConfig` write. Spellings need quoting. Worse blob.
- **Clear on the default row.** A second meaning for one Enter. Header `vs main` already matches a recorded `main` until `origin/HEAD` retargets. Following that retarget is the surprise, because the row said `main`.
- **Move last-turn later.** Two dialects until then. The hash's move-loss and ghosts stay.

## Out of scope

- Inferring a parent from the graph, the reflog, or an open PR's target.
- Persisting the commit pick.
- A gesture that returns a pane to unpicked after it has picked.
- Deleting leftover `refs/reviewr/` names.
- Comments, still per pane and in memory.
- `--base`, still a process flag. The installed pane still passes no arguments.

## Open questions

None.
