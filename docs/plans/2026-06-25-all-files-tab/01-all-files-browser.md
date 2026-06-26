# Milestone 01: All files browser

**Plan:** ./main.md · **Specs:** ../../../specs/ — the living reference this plan delivers

## Goal

A read-only `All files` tab: switch to it, browse the whole worktree as a collapsed tree, open any file and read its full content in the left pane. Each tab keeps its own selection and scroll across a switch.

## Why This Comes Next

It proves the three contracts the review surface builds on, end to end, before anything depends on them: the tab machinery, per-tab state (today `App` holds one set of cursor/scroll/view fields), and the File view reusing the diff pane. Per-tab state is the highest-shape-risk piece, so M1 exists to settle its shape.

## Entry State

The baseline repo: one `Changes` tab, no `TabBar` in code (`render_tab_bar` paints only the header), `App` holding a single navigation + diff state.

## Definition of Done

- The tab bar shows `Changes` and `All files`; `1` / `2` and a click switch; the active tab drives both panes.
- `All files` lists the whole worktree — tracked files plus untracked-not-ignored — as a tree, directories collapsed by default; expand, collapse, and navigation work.
- Selecting a file renders its full current content in the left pane: syntax-highlighted, line numbers, wrap and horizontal scroll, no folds, no change bars.
- Switching `Changes` ↔ `All files` restores each tab's own selection and scroll; a poll preserves the `All files` cursor, scroll, and expanded directories by path.
- `cargo test` and `cargo clippy` are clean.

## Exit State

A **closed** list — anything not named here is not built in M1. The subset of the specs' end-state that is live after M1.

- `Tab` enum (`Changes`, `AllFiles`) and `App.tab`, default `Changes`.
- Per-tab state: switching saves the active tab's navigation and left-pane state (file cursor/scroll, the tab's expansion set, and the left pane's path/rows/cursor/scroll/`h_scroll`) and restores the target's. Mechanism is the builder's call (a stored per-tab snapshot, or an extracted `TabState`); the contract is that each tab restores its own selection and scroll.
- `git::all_files(repo) -> Vec<String>`: tracked (`git ls-files`) plus the existing `untracked(repo)`, repo-relative, deduped, sorted.
- `reload()` routes by tab: `changed_files` for `Changes`, `all_files` for `All files`.
- Navigator generalized to entries carrying `path` and `Option<Annotation { change, additions, deletions }>`; `RowKind::File` carries the `Option`. `Changes` supplies `Some` for every file; `All files` supplies `None` in M1.
- Collapse-by-default for `All files`: its tree expands only directories the user opened (an `expanded_dirs` set on the tab), distinct from `Changes`' collapse-by-exception `collapsed_dirs`.
- `FileDiff.view: View { Diff, File }`, and `FileDiff::build_file(path, content, hl)` → all-`Context` rows, `view: File`, no folds; `binary` / `too_large` degrade as in Diff view.
- The left pane routes by `view`: `Changes` builds the Diff view (unchanged); `All files` builds the File view from current worktree content, cached by path + content.
- Tab bar renders both tabs with the active one highlighted and a click hit-zone per tab; `1` / `2` and a click call `set_tab`. The header count stays files-changed-in-scope.
- `App.changed_paths` and `changed_count()`: the active scope's changeset, computed every reload regardless of tab, so the header count and `stale_files()` stay scope-based while `All files` lists the whole worktree.

## Specs Touched

No spec is *fully* realized in M1 — each of these also covers commenting, annotation, or seeding, which is M2 — so all stay Draft until the merge gate.

| Spec | What this milestone realizes | At the gate |
| --- | --- | --- |
| `tui.md` | the tab bar, tab switching, per-tab selection and scroll | stays Draft → M2 |
| `diff-view.md` | the File view — all-`context`, no folds | stays Draft → M2 |
| `file-list.md` | the whole-repo listing, collapse-default, the optional annotation | stays Draft → M2 |
| `overview.md` | the `All files` tab as a structural surface | stays Draft → M2 |

## Out of Scope

Each deferred to M2, the milestone that owns it.

- Change markers and `+a −d` stats in the `All files` tree → M2.
- Commenting in the File view → M2.
- Tab-switch seeding and cursor-line carry → M2.
- Scope re-marking the tree in place → M2.
- File-content comment staleness (file deleted) → M2.

## Likely Files

- `src/app.rs` — `Tab`, `App.tab`, per-tab save/restore, `set_tab`, `reload()` routing, File-view load.
- `src/git.rs` — `all_files` (reuses `untracked`).
- `src/file_list.rs` — entries with `Option<Annotation>`, `RowKind::File` Option, collapse-default expansion.
- `src/diff.rs` — `View` enum, `FileDiff.view`, `build_file`.
- `src/ui.rs` — tab bar (two tabs, active, click zones), left-pane route to the File view renderer.
- `src/lib.rs` — `1` / `2` keys and tab-bar click → `set_tab`.

## Execution Plan

1. Add `Tab` + `App.tab` (default `Changes`); branch `reload()` to `all_files` vs `changed_files` by tab.
2. Add `git::all_files`; unit-test it against a temp repo with a tracked file, an untracked file, and a gitignored path.
3. Generalize `file_list` to entries with `Option<Annotation>`; `Changes` fills `Some`, `All files` `None`; add the `expanded_dirs` collapse-default model for `All files`.
4. Hold per-tab navigation + left-pane state; `set_tab` saves the active tab's and restores the target's.
5. Add `FileDiff.view` and `build_file`; route the left-pane build by `view`; read File-view content from the worktree, cached by path + content.
6. Render the tab bar (two tabs, active highlight, per-tab click zones); wire `1` / `2` and clicks to `set_tab` in `lib.rs`.

## Verification

- **Done:** `cargo test` green; run the binary → `2` lists the worktree collapsed, expand a directory, open a file → full content renders; `1` returns to `Changes` with its prior selection and scroll intact.
- **Tight:** the diff equals Exit State — no annotation producer, no comment-in-file path, no seeding; `All files` file rows carry `None` annotation.
- **Invariants upheld:** the File view reads worktree content only and adds no git write (`overview.md` never-mutate); a poll touches neither the comment store nor the input (`overview.md`); `#![forbid(unsafe_code)]` holds.

## Replan Triggers

- If save/restore of per-tab fields balloons `App`, extract a `TabState` struct holding the per-tab fields — still within M1.
- If the diff renderer or selection assumes a change row and mis-handles an all-`context` File view, fix it here — it is the contract M1 exists to prove.
- If `git ls-files` plus `untracked` double-counts or misses a case, settle the listing in `all_files` before the navigator consumes it.
