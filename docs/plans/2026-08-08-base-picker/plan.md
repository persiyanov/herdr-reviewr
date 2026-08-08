# Base picker: Plan

Delivers `specs/review-model.md#base-branch`, `specs/input.md#base-picker`, and the `specs/tui.md` header contract (issue #42).

## Problem

The `branch` scope diffs against a fixed lookup: `base_branches` config (default `main`, `master`), then `origin/HEAD`. In a repo whose trunk is `dev`, the scope silently diffs against `main`, and the chosen base is never displayed, so the wrong diff reads as broken autodetection (issue #42, danielo515). The same fixed list blocks stacked branches, where the base is another feature branch. A global config list cannot fix it: one repo's convention poisons another.

## Goal

The base is always a visible, recorded choice: the `--base` flag, else a per-repo pick made in a picker and persisted under `refs/reviewr/`, else `origin/HEAD`. The header names it. The same ship lands the header tidy-up already in the tree: the `Files` tab label, the Send button removal, and right-aligned stats.

## Definition of Done

- [ ] On the `branch` scope the header shows `vs <name>`. A name too long for the leftover width clips with a trailing `…`. A click on it opens the picker, same as `B`. It reads `no base` when nothing resolves and `· <name> missing` when a recorded choice is skipped.
- [ ] `B` (and the header click) opens the picker on file tabs while `branch` is active and no `--base` was passed. Inert elsewhere and while composing.
- [ ] Picker rows: local and origin branch names merged, checked-out branch excluded, sorted by last commit, the open PR's target first starred, the default branch next marked `default`. Type-to-filter, `↓`/`↑`, `enter` picks, `esc` cancels, clicks per the agent picker.
- [ ] A pick applies on the next frame, persists across restarts, is shared by the repo's worktrees, and reaches another pane at its next refresh. Picking the default row clears the pick.
- [ ] A pick whose branch stops resolving is skipped, not dropped, and reactivates when the branch returns.
- [ ] `base_branches` in a config file now fails the whole file as an unknown key, with the standard recovery.
- [ ] The header reads ` 1 Changes  2 Files  3 PR  [scope]` with stats right-aligned and no Send button. `s`/`S` still export.
- [ ] README's flag table, config sample, and Base branch section match the new behavior.

## Out of Scope

- PR-target auto-follow in the resolution chain. Deferred decision in `specs/review-model.md`; see Replan.
- Renaming the `All files` concept across specs and config names. Only the header label changes; `tab-all-files` stays the config name.
- Release packaging and version bump. The user cuts releases.

## Execution Plan

1. [ ] Sync the header tidy-up into `specs/tui.md`: mockup, tab label `Files`, Send button bullet removed, stats right-aligned. Grep `specs/` for Send-button click references.
2. [ ] `src/git.rs`: replace `resolve_bases` with a resolution returning the winning name, OID, and any skipped recorded name. Chain: `--base` → pick ref → `origin/HEAD` (name via `symbolic-ref refs/remotes/origin/HEAD`, today only its OID is taken). Names resolve `refs/remotes/origin/<name>` then `refs/heads/<name>` (`resolve_base_entry`, git.rs:700).
3. [ ] `src/git.rs`: pick ref read/write under `refs/reviewr/` via `update-ref`, one ref per repository (no `worktree_key` — shared refs are the sharing mechanism). Store the name as a ref'd blob, precedent `snapshot_worktree`'s object writes (git.rs:799).
4. [ ] `src/git.rs`: new branch-list helper for the picker: `for-each-ref refs/heads refs/remotes/origin --sort=-committerdate`, names merged, `origin/HEAD` and the checked-out branch excluded. None exists today (`origin_tips` is origin-only, unsorted).
5. [ ] Thread the resolved base into `WorldSnapshot` and `App` so the header paints it and `reconcile_world` carries it. Drop `base_branches` from `WorldInput` (world.rs:29). A pick calls `reload()` so the changeset rebuilds before the next frame.
6. [ ] `src/config.rs`: delete `base_branches` (field config.rs:176, default :71, accessor :211, key entry :86, validation :371-408, `to_json` :276). The unknown-key path (config.rs:356) is the migration.
7. [ ] `src/lib.rs`: config-diff sites that watched `base_branches` (lib.rs:1272, 1338) now watch nothing there; a pick change invalidates the PR worker the way a config change did (epoch bump). `src/forge.rs:475` and `pr_local` take the new resolution, so the PR frontier walk and the branch scope share one base.
8. [ ] Keymap and footer: `Action::BasePick` row (`base-pick`, `B`) in `ACTIONS` (keymap.rs:87), `FooterAction::BasePick`, label arm in `action_key_label` (ui.rs:1404), `footer_bands` early-return row for the open picker, and the `B pick base` primary on an empty branch scope with no base (app.rs:3319).
9. [ ] `App`: `Mode::BasePick` with rows, highlight, scroll, query, and caret. Open/close/move/pick after the agent picker (app.rs:3461-3513), highlight opening on the current base. Filtering through `active_field`/`edit_input` (app.rs:2545-2574), the way Search's query edits.
10. [ ] `src/ui.rs`: base span in `render_tab_bar` with truncation, `HeaderHit::Base` in `hit_header`, popup renderer in the mode dispatch (ui.rs:77, scrim is free), row hit-test per `hit_picker_row` (ui.rs:1918).
11. [ ] `src/lib.rs`: modal key gate for `Mode::BasePick` beside the agent picker's (lib.rs:1486), mouse arms for the popup and the header base click (lib.rs:1786).
12. [ ] README: flag table (:196), config sample (:214), Base branch section (:268).
13. [ ] Tests per Verification, in the same commits as the code they check.

## Likely Files

| file                     | change                                                              |
| ------------------------ | ------------------------------------------------------------------- |
| `src/git.rs`             | resolution with names, pick ref, branch-list helper                 |
| `src/config.rs`          | `base_branches` deleted                                             |
| `src/app.rs`             | `Mode::BasePick`, picker state and verbs, footer rows, reload wire  |
| `src/world.rs`           | resolved base in `WorldInput`/`WorldSnapshot`                       |
| `src/lib.rs`             | key/mouse gates, config-diff and epoch sites                        |
| `src/ui.rs`              | header base span and hit, picker popup                              |
| `src/forge.rs`           | `pr_local` on the new resolution                                    |
| `src/keymap.rs`          | `Action::BasePick`                                                  |
| `specs/tui.md`           | header tidy-up decisions                                            |
| `README.md`              | base branch and flag docs                                           |
| `tests/render.rs`        | header hit and paint tests                                          |
| `tests/app_flow.rs`      | picker flow and persistence tests                                   |
| `tests/git_repo.rs`      | resolution chain tests                                              |
| `tests/pr_candidates.rs` | fixtures off `base_branches`                                        |

## Verification

- `cargo test --test render`: rewrite `header_clicks_map_to_scope_and_send` (render.rs:870) and `a_narrow_overflowing_header_does_not_mis_map_a_click_to_send` (:1225) for the base span instead of Send; add paint tests for `vs <name>`, truncation, `no base`, and the `missing` suffix.
- `cargo test --test app_flow`: open → filter → pick → changeset rebuilt and header renamed; pick ref written and reread after a fresh `App`; default row clears the ref; picker inert under `--base`; `B pick base` footer on the empty no-base scope.
- `cargo test --test git_repo`: chain order, skip on a missing branch, reactivate on return, `origin/HEAD` fallback, no-default repo.
- `src/config.rs` tests: `base_branches` cases (config.rs:743-830) become unknown-key expectations; canonicalization test deleted.
- `src/lib.rs` test `shell_only_config_changes_do_not_invalidate_runtime_work` (lib.rs:2552): fixture moves off `base_branches` to a surviving runtime key; add the pick-change epoch case.
- No-writes invariant: the persistence test asserts the only new ref is under `refs/reviewr/` and the worktree, index, and branches are untouched.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` A/B against a rebuilt main binary, quiet system: the reload path changed; medians must hold.
- `just ci` green.
- QA: `just qa-install`, user reopens panes. The picker-feel trial here feeds the deferred PR-target decision.
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- Gate: promote `specs/review-model.md`, `input.md`, `tui.md`, `config.md`, `overview.md` to Current.

## Replan

- If the QA trial says the base should follow an open PR's target automatically, reopen brainstorming on the deferred tier in `specs/review-model.md` before adding it.
- If blob-backed pick refs prove awkward (gc, inspection), switch to a symbolic ref form; spec text is storage-agnostic.
- 2026-08-08: initial plan.
