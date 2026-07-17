# Configurable base branch — Delivery Plan

**Specs:** ../../../specs/review-model.md — the living reference this plan delivers (issue #3)

## Milestone Map

1. **Configurable `base_branches`** — single milestone; the `branch` scope resolves its base from a `config.toml` list, `--base` still winning.

## Goal

A user selects the `branch`-scope base by editing `base_branches` in `$HERDR_PLUGIN_CONFIG_DIR/config.toml`, so gitflow and non-standard-trunk repos work inside herdr where no CLI flag is reachable.

## Definition of Done

- `base_branches` in `config.toml` selects the base, resolved first-existing per repo (`review-model.md#base-branch`).
- `--base <ref>` still wins when it exists; otherwise the list decides.
- A missing, unparseable, or key-less `config.toml` falls back to the default list.
- Editing `base_branches` and refreshing re-bases without relaunch.
- `README.md` and `CHANGELOG.md` document the key.
- `cargo test` and `cargo clippy` green.

## Exit State

| Artifact | Kind | Exercised by | Spec |
| --- | --- | --- | --- |
| `config::base_branches()` | fn | `git::merge_base` calls it each resolve; the `base_branches_*` tests | `review-model.md#base-branch` |
| `config::DEFAULT_BASE_BRANCHES` | const | `base_branches()` returns it on unset dir / absent / malformed / key-less config | `review-model.md#base-branch` |
| `git::base_ref` `candidates: &[String]` param | fn signature | `merge_base` passes the resolved list; the `base_ref_*` tests | `review-model.md#base-branch` |
| `base_branches` key | config schema | a user's `config.toml` selects the branch-scope base | `review-model.md#base-branch` |
| `--base` row + `base_branches` block in README | docs | tells a user how to set it | `review-model.md#base-branch` |
| `CHANGELOG.md` Unreleased entry | docs | release note | — |

## Specs Touched

| Spec | What this plan realizes | At the gate |
| --- | --- | --- |
| `review-model.md` | the whole base-branch resolution and `base_branches` key | Draft → Current ✓ |

## Out of Scope

- Per-branch base (`branch.<name>.…`) — deferred; the shared list cannot express it.
- Merge-base-closest resolution — first-existing ships; see the spec's Decisions.

## Likely Files

- `src/config.rs` — add `DEFAULT_BASE_BRANCHES` const and `base_branches()`; new parse tests.
- `src/git.rs` — `base_ref` gains `candidates: &[String]`, drops the hardcoded array; `merge_base` passes `config::base_branches()`.
- `tests/git_repo.rs` — a config-driven base resolves the `branch` scope; an absent flag falls through the list.
- `README.md` — the `--base` row, the `branch` bullet (lines 142–143), and a `base_branches` config block by the theme docs.
- `CHANGELOG.md` — Unreleased `Added` entry citing `specs/review-model.md` and issue #3.

## Execution Plan

1. [x] `config.rs`: add `DEFAULT_BASE_BRANCHES` and `base_branches() -> Vec<String>`, mirroring `config_file_theme` — read `$HERDR_PLUGIN_CONFIG_DIR/config.toml`, take the `base_branches` string array, default to the const on unset dir / absent / unparseable / key-less / empty.
2. [x] `config.rs` tests: list present; key absent → default; malformed file → default; non-string entries → default; empty array → default.
3. [x] `git.rs`: `base_ref(repo, flag, candidates: &[String])` — flag if it exists, else first existing candidate; `merge_base` calls `base_ref(repo, base, &crate::config::base_branches())`.
4. [x] `tests/git_repo.rs`: `merge_base(Scope::Branch, None)` resolves via the default list; a `--base` that does not exist falls through to the list.
5. [x] `README.md`: note config precedence on the `--base` row, update the `branch` bullet, add the `base_branches` block.
6. [x] `CHANGELOG.md`: Unreleased `Added` entry.

## Verification

- **Done:** `cargo test` green; `cargo run -- <repo>` with a `config.toml` `base_branches` and `HERDR_PLUGIN_CONFIG_DIR` set → `branch` scope diffs against the configured base; editing the list and pressing `r` re-bases.
- **Tight:** row-check the diff against the Exit State table — every added artifact has a row; no signature threads `candidates` past `base_ref`; `app.rs` unchanged.
- **Contract upheld** — `review-model.md#base-branch` has no numbered invariants; bind each precedence row and fallback to a test:

| Spec contract | Bound to | Signal |
| --- | --- | --- |
| Precedence row 1 — `--base` wins when it exists | `base_ref` unit test | an existing flag ref is used over the list |
| Precedence row 1 — non-existent flag is skipped | `base_ref` unit test | a bogus flag falls through to the list |
| Precedence row 2 — first existing list entry wins | `base_ref` unit test | the first present candidate is chosen, earlier absent ones skipped |
| Fallback — absent / malformed / key-less config → default list | `base_branches` unit tests | `base_branches()` returns `DEFAULT_BASE_BRANCHES` |
| Live — the list is read each resolve | `tests/git_repo.rs` config-driven case | a config `base_branches` steers the `branch` scope base |

## Replan Triggers

- If threading proves cleaner than the internal `config::base_branches()` read in `merge_base`, thread `candidates` and update the test call sites; record it in the log.
- If a repo with several live trunk candidates picks the wrong base under first-existing, reopen the merge-base-closest decision in the spec.

## Replan Log

- 2026-07-08: initial plan from approved contract (`specs/review-model.md`, issue #3).
- 2026-07-08: env-based test would race under parallel tests → cover config reading via the pure `base_branches_in(dir)` unit test and resolution via `merge_base` on the default list → `config.rs`, `tests/git_repo.rs`.
- 2026-07-08: empty / all-non-string `base_branches` falls back to default → spec fallback bullet made precise → `specs/review-model.md#base-branch`.
