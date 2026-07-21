# Commit-anchored PR detection — Plan

Delivers `specs/forge-host.md` Resolution, `specs/pr-tab.md`, `specs/config.md` base canonicalization, and `specs/review-model.md` base precedence (all Draft, uncommitted).

## Goal

A PR resolves into the tab only when it provably contains the worktree's published commits. Publication points nominate through the forge's commit association, names play no part, and the candidate-branch machinery is deleted.

## Definition of Done

- [x] A zero-work worktree at the base tip resolves no PR, with sibling branches and their open PRs present.
- [x] A worktree pushed as `git push origin HEAD:<other-name>` resolves its PR with no recorded upstream.
- [x] A same-named PR whose head shares no commits with the worktree never resolves, fork or not.
- [x] A reused branch name resolves no historical PR.
- [x] A closed-unmerged PR resolves as history when an `origin` branch tip at a publication point names it and its head equals that point, and not otherwise.
- [x] The worktree's own squash-merged PR resolves as history, including after an applied suggestion moved the true head.
- [x] With no resolvable base and no `origin/HEAD`, the tab shows the empty state; a repo with only `origin/HEAD` resolves its base through it.
- [x] `main` and `origin/main` in `base_branches` behave identically, resolving `refs/remotes/origin/main` before `refs/heads/main`, and `--base` canonicalizes the same way.
- [x] `comments` and `reviewThreads` fetch their newest 100 rows and flag truncation via `hasPreviousPage`.
- [x] The candidate-derivation code and its resolve query are deleted, with their tests replaced by publication-point tests.
- [x] All test suites pass.

## Out of Scope
- A release. `CHANGELOG.md` and version bump follow `docs/RELEASING.md` separately.

## Execution Plan

1. [x] `src/config.rs`: canonicalize `base_branches` at validation — strip `refs/heads/` / `refs/remotes/origin/` / `origin/` prefixes, collapse duplicates keeping the first. Unit tests for the strip rule and collapse.
2. [x] `src/git.rs`: one base-resolution helper (`refs/remotes/origin/<name>`, then `refs/heads/<name>`, then the `origin/HEAD` fallback) shared by `pin_base`, `base_ref`, and scope's `merge_base`; `--base` flows through the same canonicalization. Update fixtures.
3. [x] `src/git.rs`: publication points — boundary commits of `rev-list HEAD --not --remotes=origin`, filtered to those not ancestors of the pinned base. Replaces `pr_local` candidates; `PrFetchInput` carries points instead of names. Unit tests: linear history, merge-y history, zero-work, no-base, unborn, detached.
4. [x] `src/forge.rs`: association resolve — one aliased GraphQL call, `object(oid:$p{i}){... on Commit{associatedPullRequests}}` per point against the `origin` repository, fetching `number state headRefOid baseRepository isCrossRepository createdAt`; filter to PRs based on the resolved repository target; pick per the spec's three-step disambiguation; newest-merged historical fallback; closed-unmerged epilogue via `for-each-ref --points-at <point>` names and an exact `headRefOid` match. Delete `build_resolve_query`, `parse_resolve`, `select_open`, `select_historical`, and `remote_candidates`/`candidate_order` in `git.rs`. Unit tests on the pick and parse.
5. [x] `src/forge.rs`: `build_detail_query` — `comments(last:100)`, `reviewThreads(last:100)`, `hasPreviousPage`; update the truncation tests.
6. [x] `src/ui.rs` / `src/app.rs`: ambiguity copy drops the per-branch phrasing (`Found N open PRs for this branch` → the spec's count wording); empty states otherwise unchanged.
7. [x] `tests/pr_candidates.rs` → publication-point fixtures on real temp repos; the DoD resolution scenarios land as `src/forge.rs` unit tests (gates, association, picks) plus the env-gated live probe `tests/pr_live.rs`.

## Likely Files

| file                     | change                                                                  |
| ------------------------ | ------------------------------------------------------------------------ |
| `src/config.rs`          | canonicalize `base_branches`, dedup, tests                               |
| `src/git.rs`             | base helper, publication points, delete candidate machinery              |
| `src/forge.rs`           | association resolve, delete name resolve, pagination                     |
| `src/ui.rs`              | ambiguity copy                                                           |
| `src/app.rs`             | empty-state plumbing                                                     |
| `src/lib.rs`             | `--base` canonicalization pass-through, `PrFetchInput` plumbing          |
| `tests/pr_candidates.rs` | rewrite as publication-point fixtures                                    |
| `tests/app_flow.rs`      | end-to-end DoD scenarios                                                 |

## Verification

- `cargo test` → all suites green, including the new point and pick tests.
- `cargo clippy` → clean.
- Live: swap the local build into the herdr pane and check three real worktrees — a fresh zero-work one (empty state), one pushed as `HEAD:<name>` (its PR), one with a squash-merged PR (the epilogue). The fork case was spike-verified against `persiyanov/herdr-reviewr#19`.
- Tight: everything the diff adds is exercised by a DoD line, and everything the spec deleted leaves the tree.
- Gate: promote `forge-host.md`, `pr-tab.md`, `config.md`, `review-model.md` to Current; commit the Draft diff with the implementation.

## Replan

- If the association endpoint is missing on a user's GitHub Enterprise version, the fetch error surfaces as the existing retryable `Error` state; log here if it needs its own remedy copy.
- If boundary-commit counts explode on merge-heavy local history, cap the queried points and log the cap here and in `forge-host.md`.
- 2026-07-17: initial plan (kinship admission over name candidates).
- 2026-07-17: QA round 3 — the fetch-record tier was purged on review: its input is a transient file, so a resolved PR could silently decay on the next bare fetch, and a one-off pull outranking a recorded upstream was incoherent. The contract returns to two tiers — commits prove, a parked tip proves by exact identity — and a space with no evidence shows the empty state.
- 2026-07-17: QA round 2 — a zero-work worktree deliberately reset to a fetched branch showed no PR while the branch's open PR was the space's whole purpose. The for-merge `FETCH_HEAD` entry is the explicit local record of that intent; it now nominates, corroborated by the fetched commit (PR head equals it or provably descends from it). A bare fetch claims nothing, so the sibling/Dependabot adversary stays impossible. Landed in `specs/forge-host.md` Resolution.
- 2026-07-17: QA round — a space parked at its merged tip showed the empty state once the base absorbed the merge (the documented epilogue lifetime, rejected in live use). Fixed with absorbed candidates: a base-history parked tip nominates, and only a merged PR whose head is exactly that commit admits. Landed in `specs/forge-host.md` Resolution.
- 2026-07-17: high-effort review round (10 finder angles) — confirmed and fixed: the empty target selection set with no named points (tgt block now always carries `id`), the base filter now compares repository ids (rename-proof), the closed lookup queries every tip name per point, `--base` resolves verbatim first, every resolved base excludes points, a base-named recorded upstream never joins the tiebreak, the association source must share the target's host, plus the efficiency and structure findings (single origin read, empty-origin short-circuit, bounded boundary walk, shared pick/dedup helpers, `PrFetchInput` embeds `PrLocalState`). Spec guard clauses and table padding landed with them.
- 2026-07-17: closed-unmerged epilogue restored via exact-OID name lookup, zero precision cost. Landed in `specs/forge-host.md`.
- 2026-07-17: association moved to GraphQL `associatedPullRequests` (spike-verified on the fork and squash cases) and `origin/HEAD` added as the base fallback.
- 2026-07-17: pivot — spike proved `commits/{sha}/pulls` covers squash merges and fork PRs (queried via `origin`), so nomination moved from name candidates to publication points and the candidate machinery is deleted. Landed in `specs/forge-host.md` Resolution.
