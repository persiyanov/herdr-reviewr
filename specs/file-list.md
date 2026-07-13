---
Status: Current
Created: 2026-06-24
Last edited: 2026-07-09
---

# File list

The right-pane navigator: a directory tree that opens a file in the left pane. It lists the scope's changed files in `Changes` and the whole worktree in `All files`.

## Overview

The list groups files into a collapsible tree. A file row shows a change marker, its name, and its add/remove stats.

```
 src/
   M app.rs                    +562 −16
   A diff_view.rs              +210
   M ui.rs                     +437 −9
 specs/
   A diff-view.md              +96
   M …/2026-06-23-changes/plan +4 −2
 M  Cargo.toml                 +11 −1
 ?  herdr-plugin.toml          +25
```

In `All files` the same navigator lists the whole worktree: tracked, untracked, and ignored alike. Ignored rows render dimmed. `.git` is the one exclusion. A file the active scope changed keeps its marker and stats. The rest show name only.

```
 src/
   M app.rs                    +562 −16
     diff.rs
   M ui.rs                     +437 −9
 specs/
     overview.md
 target/                       (ignored — dimmed, one collapsed row)
 Cargo.toml
```

### Node

The list is a flat sequence of visible rows over the tree.

| field       | type     | meaning                                                             |
| ----------- | -------- | -------------------------------------------------------------------- |
| `kind`      | enum     | `dir` or `file`                                                       |
| `name`      | string   | the segment shown: a directory name, or a file's basename             |
| `depth`     | integer  | nesting level, for indentation                                        |
| `change`    | enum?    | `added`/`modified`/`deleted`/`renamed`/`untracked`, absent on a `dir` and on an unchanged file |
| `additions` | integer? | lines added in the scope, absent on unchanged rows                    |
| `deletions` | integer? | lines removed in the scope, absent on unchanged rows                  |
| `ignored`   | bool?    | in `All files`, whether git ignores this row (rendered dimmed), absent on tracked and untracked rows |
| `expanded`  | bool?    | on a `dir`, whether its children are shown                            |

## Behavior

### Tree

- Files group by directory. Directories sort first, then files, both alphabetical.
- A directory with a single child collapses into its child's row, so vertical space goes to names, not scaffolding.
- `Changes` directories open expanded. A changeset is small enough to show whole.
- `All files` directories open collapsed. The worktree is not.
- A wholly-ignored directory is one collapsed row. Its contents enumerate only on expand, so a large ignored tree costs nothing until opened.

### Selection

- The cursor selects a row. `j`/`k` and the arrows move it, skipping collapsed subtrees. The list scrolls to keep it visible.
- Moving onto a file opens it in the left pane: its diff in `Changes`, its content in `All files`.
- The wheel scrolls the viewport only. The selection and the open file stay put, so browsing never reloads a diff.
- `←`/`→` or a click collapses and expands a directory. A click on a file selects and opens it. There is no `enter` activation.
- `tab` moves focus to the left pane, to navigate and comment.
- A poll preserves the selection and expansions by path. A selected file that left the changeset falls back to the open file, then the first file.
- In `All files` a poll adds and removes rows as the worktree changes, preserving cursor, scroll, and expansions by path.
- Switching scope re-marks the `All files` tree in place. Only the markers and stats change.

### Presentation

- A file row is `<marker> <name> <stats>`: the marker colored by kind, the basename bright, parent directories dimmed, stats right-aligned.
- An ignored row dims whole, distinct from the marker colors. `All files` is the one place an ignored path is readable. An ignored file never carries a change marker, since every scope respects `.gitignore` (`review-model.md`).
- A too-narrow path shortens with a middle ellipsis (`…/2026-06-23-changes/plan`), keeping the basename and stats visible.

## Non-goals

- No reviewed-file state. Marking a file reviewed and greying it is roadmap.
- No file content rendered here. The left pane renders the diff or content (`diff-view.md`).

## Related specs

- [review-model](./review-model.md)
- [tui](./tui.md)
