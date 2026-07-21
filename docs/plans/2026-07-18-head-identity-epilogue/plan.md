# HEAD exact-identity nomination — Plan

Delivers `specs/forge-host.md#resolution` (the exact-identity `HEAD` path).

## Goal

A PR resolves whenever the worktree is parked exactly on its head commit, open or merged. The remote-branch-deletion toggle and the merge method stop affecting what the tab shows.

## Definition of Done

- [x] A worktree parked at a squash-merged tip shows the merged PR after the remote branch is deleted.
- [x] A zero-work worktree at the base tip stays empty when an open PR's head equals that commit.
- [x] A worktree with no resolvable base stays empty.
- [x] The full suite, clippy, and fmt stay green.

## Out of Scope

- Continuing work on top of an orphaned tip. The tab stays empty until the next push, as today.
- Persisting a last-resolved PR. Rejected in brainstorming.

## Execution Plan

1. [x] `src/git.rs`: `PrLocalState` gains `head_nominates: bool` — true when `head_oid` exists, at least one base resolves, and `HEAD` is not an ancestor of any resolved base. Set in `pr_local`. Unit tests: fresh-at-base false, orphaned-tip true, no-base false.
2. [x] `src/forge.rs`: `fetch_inner` returns `NoPr` only when points, absorbed, and `head_nominates` are all empty or false.
3. [x] `src/forge.rs`: `nominated_head` yields the pinned `HEAD` when `head_nominates` and no point carries the OID. The gate and the query alias both route through it. `parse_association` admits a head-alias node only when its `headRefOid` equals the pinned `HEAD`, open or merged.
4. [x] Unit tests in `src/forge.rs`: head-alias exact match admits open and merged; a containing-but-not-exact node and a closed node are rejected; `nominated_head` covers the flag, the standalone case, and the point dedup; a mixed points-absorbed-head response keeps its index classes aligned.
5. [x] QA: `just qa-install`, verify a real squash-merged-and-deleted space shows `merged`, and a fresh space at main stays empty.

## Likely Files

| file                  | change                                            |
| --------------------- | ------------------------------------------------- |
| `src/git.rs`          | `head_nominates` field and guard, tests           |
| `src/forge.rs`        | gate, query alias, admission rule, tests          |
| `specs/forge-host.md` | already Draft, promote at the gate                |

## Verification

- `just ci` → green.
- DoD 1–3 → the unit tests in steps 1 and 4, plus the step-5 live QA pass.
- Tight: everything the diff adds is exercised by a DoD line.
- Gate: high-effort code review of the diff, then promote `specs/forge-host.md` to Current.

## Replan

- If GitHub's `associatedPullRequests` on a hidden-ref commit omits the PR, revisit with a `refs/pull/*/head` probe.
- 2026-07-18: initial plan.
- 2026-07-18: live probe shows closed-unmerged PRs never associate → the exact-identity path narrows to open or merged, in `specs/forge-host.md` and steps 3–4.
