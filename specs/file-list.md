---
Status: Draft
Created: 2026-06-24
Last edited: 2026-06-25
---

# File list

The right-pane navigator: a directory tree you move over to open a file in the left pane — the scope's changed files in `Changes`, the whole worktree in `All files`.

## Overview

The list groups the scope's changed files into a collapsible tree of directories and files. Each file row shows a change marker, its name, and its add/remove stats; long paths shorten with a middle ellipsis so the name and stats never clip.

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

In the `All files` tab the same navigator lists the whole worktree — every git-tracked file plus untracked files git does not ignore. A file the active scope changed keeps its marker and stats; the rest show name only.

```
 src/
   M app.rs                    +562 −16
     diff.rs
   M ui.rs                     +437 −9
 specs/
     overview.md
 Cargo.toml
```

### Node

The list is a flat sequence of visible rows over the tree; a row is a directory or a file.

| field | type | meaning |
|-------|------|---------|
| `kind` | enum | `dir` or `file`. |
| `name` | string | The segment shown — a directory name, or a file's basename. |
| `depth` | integer | Nesting level, for indentation. |
| `change` | enum? | A changed `file`'s `added`/`modified`/`deleted`/`renamed`/`untracked` marker; absent on a `dir` and on an unchanged `All files` file. |
| `additions` | integer? | A changed `file`'s lines added in the scope; absent otherwise. |
| `deletions` | integer? | A changed `file`'s lines removed in the scope; absent otherwise. |
| `expanded` | bool? | On a `dir`, whether its children are shown. |

## Behavior

### Tree

- Files are grouped by directory; a directory with a single child collapses into its child's row (`specs/` not `specs/` then one file) to spend vertical space on names, not scaffolding.
- Directories sort first, then files; both alphabetical within a parent.
- Directories are expanded by default — a changeset is usually small, so the whole tree is visible at once.
- In `All files` the tree lists every git-tracked file plus untracked files git does not ignore, so build output and ignored paths never appear.
- `All files` directories are collapsed by default — the worktree is large, so you expand into it rather than scroll it whole.

### Selection

- The cursor selects a row; `j`/`k` and the arrows move it, skipping collapsed subtrees, and the list scrolls to keep it on screen. Moving onto a file opens it in the left pane — its diff in `Changes`, its content in `All files`.
- The selection (what is open) is separate from the viewport (what is scrolled into view). The wheel scrolls the viewport on its own — the selection and the open diff stay put, so browsing the list never reloads a diff — and the selection may scroll out of view until you move it again.
- A directory collapses or expands with `←` / `→` or a click; `tab` moves focus to the diff to navigate and comment. There is no `enter` activation in the list — selecting a file already opens it.
- A click selects the row under it — a file opens, a directory toggles.
- A poll preserves the selected file and which directories are expanded, matching them by path; if the selected file left the changeset, the cursor falls back to the open file, then the first file.
- `All files` reveals a file by expanding its ancestor directories and selecting it, used when a tab switch seeds the selection (`tui.md`).
- In `All files` a poll adds and removes files as the worktree changes, preserving the cursor, scroll, and expanded directories by path.
- Switching scope re-marks the `All files` tree in place — the cursor, scroll, and expanded directories hold; only the markers and stats change.

### Presentation

- A file row is `<marker> <name> <stats>`: the change marker colored by kind, the basename bright with its parent directories dimmed, and `+a −d` stats right-aligned against the pane edge.
- When the row is too narrow, the path shortens with a middle ellipsis (`…/2026-06-23-changes/plan`) so both the basename and the stats stay visible.

## Non-goals

- No reviewed-file state in this design — marking a file reviewed and greying it is roadmap.
- No file content rendered here — the navigator lists files; the left pane renders the diff or content (`diff-view.md`).

## Decisions

- A tree, not a flat path list — directories group related changes and shorten rows; a flat list of full paths wastes width and truncates the name. Rejected: the flat `M path +a −d` list.
- Single-child directories collapse into their child — a chain of one-child directories is scaffolding; folding it keeps names readable in a narrow pane. Rejected: always rendering every directory level.
- One navigator, two trees — the `Changes` changed-files tree and the `All files` whole-repo tree are one component over different file sets, so selection, collapsing, and presentation match. Rejected: a separate repo-tree widget.
- `All files` annotates the active scope's changes — a changed file shows its marker and stats inline in the full tree, so you see what the agent touched while browsing everything, and switching scope re-annotates. Rejected: a scope-blind tree.
- Expanded by default in `Changes`, collapsed in `All files` — a changeset is small enough to show whole; the worktree is not. Rejected: one default for both.

## Open decisions

- None.

## Related specs

- `./review-model.md`
- `./tui.md`
