# Branch-based PR resolution: Plan

Delivers `specs/forge-host.md#resolution` and the Query bullets in `specs/forge-providers.md`.

## Problem

The PR tab resolves through commit-identity machinery that nobody can predict: containment queries, exact-head identity, merge-commit identity with an authorship gate. It grew by patches and leaks across lifecycles — a new worktree off main showed the previous PR as merged. The user reset the design to the `gh pr view` mental model: the tab shows the newest PR opened from the current branch.

## Goal

The PR tab shows the newest PR opened from the current branch, on all three forges. Everything else about the tab (snapshot, checks, comments, refresh) is unchanged.

## Definition of Done

- [x] A fresh branch or worktree shows the empty state.
- [x] A PR opened from the branch shows through open and merged, whoever opened it, under squash, merge, and rebase strategies.
- [x] Commits on the branch after the merge keep the merged PR. The branch's next PR replaces it.
- [x] An agent on main with work pushed as `HEAD:<side-branch>` shows the side branch's PR. After merge and pull, main is empty.
- [x] A new branch reusing a deleted branch's name shows the empty state, not the old PR.
- [x] On a fork, PRs resolve from both repositories, upstream wins, and only fork-sourced PRs count in upstream.
- [x] Several open PRs resolve to the newest. The ambiguous-count state no longer exists.
- [x] The commit-identity machinery is deleted: containment queries, merge-commit identity, authorship reads, `viewerDidAuthor`/`connectionData`/`glab user` calls.

## Out of Scope

- Rendering changes beyond deleting the ambiguous-count view. `pr-tab.md` is otherwise untouched.
- The perceived-latency benchmark. Resolution runs on the fetch worker, off the frame loop.

## Execution Plan

1. [x] Reset: `git restore src/ tests/` to drop the uncommitted merge-commit implementation, and delete `docs/plans/2026-07-23-merge-commit-epilogue/`. The specs keep the new Draft.
2. [x] `src/git.rs`: reshape `PrLocalState` to carry the branch's forge names — local branch name, recorded upstream (unless it names a resolved base), and the origin branch names `publication_points` already collects at the pushed frontier. Keep the pins and the detached case. Drop `head_nominates` and the absorbed set.
3. [x] `src/git.rs`: add the ancestry check for the merged/closed guard (`merge-base --is-ancestor`, missing object means not admitted).
4. [x] `src/forge.rs` (GitHub): replace `associate_points`/`parse_association` with one aliased `pullRequests(headRefName:)` query per name, all states. Pick: newest open, else newest merged-or-closed passing the ancestry guard. Delete `Pick::Ambiguous` and its `PrView` variant.
5. [x] `src/gitlab.rs`: promote the existing `source_branch` listing to the only lookup, all states. Delete the containment fan-out, `OidKind`, `containment_admission`, `own_merge_admits`, `viewer_username`.
6. [x] `src/azure_devops.rs`: filter the two enumerations by `sourceRefName` against the names. Delete `merge_commit_admitted`, `connection_identity`, `epilogue_oids`.
7. [x] Fork case: when `upstream` resolves a different repository than `origin`, query both — upstream filtered to fork-sourced heads, client-side where the forge API cannot filter. Upstream's pick outranks the fork's.
8. [x] `src/lib.rs` + `src/ui.rs`: remove the ambiguous-count rendering.
9. [x] Tests: rewrite `tests/pr_candidates.rs` around name derivation (the `HEAD:<other-name>` push, the reused name, the synced-main case). Rewrite the provider pick tests around by-name payloads. Keep the snapshot/checks/comments tests untouched.
10. [x] Design walks over the Draft (hostile schedules, careless-user fumbles), skipped during brainstorming by pace choice. Findings route to the spec before the merge gate.

## Likely Files

| file                     | change                                                       |
| ------------------------ | ------------------------------------------------------------ |
| `src/git.rs`             | branch-name derivation, ancestry check, `PrLocalState` shape |
| `src/forge.rs`           | by-name query, newest-wins pick, delete association machinery |
| `src/gitlab.rs`          | `source_branch` listing as the only path, deletions          |
| `src/azure_devops.rs`    | `sourceRefName` filter, deletions                            |
| `src/lib.rs`, `src/ui.rs`| remove the ambiguous-count state                             |
| `tests/pr_candidates.rs` | name-derivation scenarios                                    |

## Verification

- `just ci` → green.
- Live GitHub: `cargo run` in `~/me/extruct-ai` on synced main → empty. In a worktree parked on a branch with a merged PR → `merged #N`.
- Live flow: next agent PR in a real pane — open shows, merge shows, pull empties.
- Tight: everything the diff adds is exercised by a DoD line.
- Gate: high-effort `/code-review` loop until clean, then `/garfield`, then promote both specs to Current, then `just qa-install`.

## Replan

- Fork flows are unit-tested (`fork_repository`, `parse_branch_lookup`, `collect_assoc`) but have no live QA — no fork clone exists in this environment. First fork-clone use is the live check; a miss there reopens the fork query design.

- If a forge API cannot list closed PRs by branch name, that forge loses the closed epilogue and the spec's per-forge Query bullet records it.
- If the fork-qualified head filter is unavailable server-side anywhere, filter client-side on the PR's reported source repository.
- 2026-07-23: initial plan. Supersedes and deletes `2026-07-23-merge-commit-epilogue/`.
