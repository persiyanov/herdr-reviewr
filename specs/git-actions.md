---
Status: Current
Created: 2026-08-09
Last edited: 2026-08-09
---

# Git actions

Explicit commit, discard, and push from the `Changes` tab. These are the only user-facing Git
writes reviewr performs.

## Boundary

The actions exist only in `Changes` while the `uncommitted` scope is active. They never run from a
poll, refresh, config change, or forge result. One repository worker serializes them so hooks and a
network push never block input or painting. A completion requests fresh world and PR snapshots.

`commit`, `discard`, and `push` are rebindable actions, defaulting to `C`, `D`, and `P`. The
lowercase comment, delete-comment, and navigator-position bindings do not change.

Every applicable action is advertised in the collapsed footer while this boundary holds. They
follow the primary action and precede secondary cursor actions; the ordinary narrow-footer rule
may trim trailing actions behind `?`. The expanded footer repeats anything that did not fit.

## Commit

`C` opens a frozen picker over every uncommitted changed-file row, all checked. `↑`/`↓` or the
configured movement keys move, `space` or a row click toggles, `enter` continues with at least one
file, and `esc` cancels.

The message is multiline. Plain `enter` submits, Shift/Alt+Enter and Ctrl+J insert a newline, and
`esc` returns to the file picker without losing its selection or message. A blank message refuses.

The commit contains each selected file's complete worktree content, staged and unstaged together;
untracked files, deletions, and both paths of a rename are supported. It excludes every unselected
file and preserves unrelated entries in the real index. A temporary index seeded from `HEAD` (or
the empty tree in an unborn repository) feeds ordinary `git commit`, so configured identity,
signing, and hooks still apply. Success reconciles only selected paths in the real index to the new
`HEAD`. A pre-commit failure leaves the real index and `HEAD` untouched. A commit followed by a
real-index reconciliation failure reports the successful commit plus the recovery warning.

## Discard

`D` opens a small confirmation for the current file row; it is inert on a directory or without a
file. The dialog names both paths of a rename. Bare `enter` confirms and `esc` cancels.

Confirmation restores tracked content in both worktree and index to `HEAD`. A path absent from
`HEAD`, including an untracked file or staged addition, is removed. A rename restores its old path
and removes its new path. No unrelated path changes.

## Push and safety

`P` is offered even when the uncommitted list is empty. It pushes the attached current branch to
its configured upstream. With no configured upstream, it publishes `HEAD` under the local branch
name and sets tracking after the push succeeds. The destination is the first configured choice in
this order: `branch.<name>.pushRemote`, `remote.pushDefault`, `origin`, then the sole remote. A
broken explicit choice, no remotes, or multiple remotes without another choice refuses rather than
guessing. It never forces or prompts in the terminal. Detached HEAD, authentication, network, and
non-fast-forward failures report through the status line; a failed initial push creates no tracking
configuration.

A commit/discard dialog records its opening `HEAD`, worktree content, and index state for every
affected path. If `HEAD` or an affected path changes before execution, the action refuses and asks
the reviewer to reopen the dialog. The picker and commit message are authored/place state: they
survive refresh and config recovery. An action accepted before a config becomes invalid may finish;
its completion reconciles after recovery because a completed Git mutation cannot be discarded.

Comments are never consumed by a Git action. When a commit or discard removes their diff from the
uncommitted scope, the normal stale-comment rules apply.
