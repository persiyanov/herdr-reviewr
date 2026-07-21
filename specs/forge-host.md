---
Status: Current
Created: 2026-06-27
Last edited: 2026-07-18
---

# forge host

How reviewr reads one pull request from GitHub — identity, state, checks, comments — through the `gh` CLI, for the read-only `PR` tab (`pr-tab.md`). It never writes back.

## Overview

reviewr resolves the worktree's pull request through its commits, then reads a snapshot of it through `gh` on each poll. Every shown PR provably belongs to this worktree's work. The snapshot is the single value the `PR` tab renders.

```
PR #226  open  persiyanov/deep-research-benchmark → main   ⇡ 2 unpushed
  merge      ⚠ conflicts with main
  checks     ✗ failing — ✓ build-main-image · ✓ review · ✗ tests
  comments   5 (newest first) — @you 5m · @codex 2h · @claude 2h · …
```

The snapshot:

| field                  | type   | meaning                                                                     |
| ---------------------- | ------ | --------------------------------------------------------------------------- |
| `number`               | int?   | PR number, `null` when no PR resolves                                       |
| `title`, `url`         | string | identity                                                                    |
| `body`                 | string | the PR description as GitHub returns it, empty when none                    |
| `state`                | enum   | `open`, `merged`, or `closed`                                               |
| `is_draft`             | bool   | draft flag                                                                  |
| `head_ref`             | string | the PR's head branch name, which may differ from the local branch           |
| `head_is_fork`         | bool   | the head lives in another repository (GitHub's `isCrossRepository`)         |
| `base_ref`             | string | the merge target                                                            |
| `merge`                | enum   | `clean`, `conflicting`, or `blocked`                                        |
| `sync`                 | enum   | `in_sync`, `unpushed`, `behind`, or `unknown`, with a count when known      |
| `checks`               | list   | one row per latest check: `name` and `status` (conclusion folded in)        |
| `comments`             | list   | one row per comment, newest first                                           |
| `truncated`            | bool   | a capped surface had a further page, so a list is a prefix                  |

A `comments` row:

| field                        | type   | meaning                                                              |
| ---------------------------- | ------ | ---------------------------------------------------------------------- |
| `kind`                       | enum   | `review` (a review's body), `comment` (conversation), `finding` (inline) |
| `author`, `author_is_bot`    | string, bool | the `@login` and whether it is a bot                              |
| `anchor`                     | string | `path:line` for a `finding`, the literal kind word otherwise            |
| `body`, `snippet`            | string | the text as GitHub returns it, no chrome-stripping or format parsing; only a `finding` carries a snippet |
| `created_at`                 | time   | post time, the newest-first sort key                                    |
| `is_resolved`, `is_outdated` | bool   | thread state for a `finding`, always false otherwise                    |
| `reply_count`                | int    | replies on a `finding`'s thread beyond the root                         |

## Behavior

### GitHub hosts

`github_host` in reviewr's `config.toml` adds one GitHub Enterprise hostname. Its value contract lives in `config.md`.

```toml
github_host = "github.example.com"
```

Host matching is case-insensitive. A missing setting adds no Enterprise host. `github.com` remains supported when the setting is present.

Both remotes use the primary fetch URL after Git's `url.*.insteadOf` rewrite. A separate push URL does not affect PR reads.

| remote state                                                               | outcome                                                                  |
| -------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `upstream` names `github.com` or exact `github_host` with `owner/repository` | reviewr reads that repository                                            |
| `upstream` is absent, hostless, unsupported, or malformed                  | `origin` determines the repository                                       |
| reading `upstream` fails                                                   | reviewr shows the retryable Git error and never falls through            |
| `origin` names `github.com` or exact `github_host` with `owner/repository`   | reviewr reads that repository                                            |
| `origin` names another hosted repository                                   | reviewr names the unsupported host and points Enterprise users to config |
| `origin` is missing or hostless                                            | reviewr says the PR tab needs a supported GitHub `upstream` or `origin`   |
| `origin` names a supported host without `owner/repository`                  | reviewr says the GitHub origin is malformed                              |
| reading `origin` fails                                                     | reviewr shows the retryable Git error                                    |

A fork clone with `origin` pointed at the fork and a supported `upstream` pointed at the base
repository resolves the base repository's PR without setup.

Canonical SSH remotes work in scp-style (`git@host:owner/repository.git`) and `ssh://` forms. Hosted
URL forms use `http://`, `https://`, or `git://`. File URLs and other schemes are not GitHub
repository identities. SSH aliases are not inferred as canonical GitHub hosts. `GH_HOST` cannot
redirect a fetch.

### Resolution

The worktree's commits nominate pull requests. Containment or exact head identity admits them. A branch name never proves identity, so names play no part in resolution.

- Each fetch pins `HEAD` and the base ref to commit OIDs. Ancestry, distance, and sync calculations use those pins while the agent commits beside it.
- The publication points are the nearest ancestors of the pinned `HEAD` present on `origin`. A point that is an ancestor of any resolved base entry proves nothing and is skipped.
- The pinned `HEAD` also nominates by exact identity when it is not an ancestor of any resolved base entry. A PR admits through this path only when its head is exactly that commit. The PR may be open or merged.
- With no publication point and no exact-identity admission, the tab shows the empty state. The worktree has published no provable work.
- With no base resolvable, no nomination is provable and the tab shows the empty state.
- Each publication point is asked of the forge: which pull requests contain this commit. The query runs against the `origin` repository, where the commits live. An `origin` on another host proves nothing on the target's forge, so the target repository stands in. Only PRs based on the resolved repository target count.
- Every resolved PR therefore contains the worktree's published work or carries its parked commit as the exact head. There is no third admission path.
- Exactly one open PR resolves when one contains a publication point.
- Several open PRs disambiguate in order: a head equal to the pinned `HEAD`, a head equal to a publication point, the head named by the recorded upstream. A record naming a configured base is tracking, not publication, and never joins the tiebreak. Failing all three, reviewr surfaces the count, never a silent guess.
- With no open PR, the newest-merged PR containing a publication point shows as historical state. A merged PR whose head is exactly the pinned `HEAD` resolves the same way, even with no publication point.
- A worktree parked on published base history keeps its epilogue: the absorbed tip still nominates, and a merged PR whose head is exactly that commit resolves as history. Containment proves nothing for an absorbed commit.
- A PR closed without merging does not associate. It still resolves as history through exact identity: an `origin` branch tip at a publication point names it, and its reported head equals that point.
- With none at all, the body says only `No pull request yet. Ready to ship?`
- A fork PR resolves through the same query: the commits live on the fork (`origin`), and the association carries the base-repository PR. `pr-tab.md` marks the fork case.
- Local staleness costs recall first: a stale `origin/*` ref can hide a publication point. A stale base ref can also admit mainline history as one, until a fetch heals it.
- A detached `HEAD` has no pin and shows the empty state.

What a user observes:

- A worktree pushed as `git push origin HEAD:<other-name>` resolves its PR. The pushed commits are the publication point, whatever the name.
- A teammate's PR parked at this worktree's exact `HEAD` never beats the PR on the recorded upstream.
- A remote branch extending `HEAD` can be a colleague's continuation of this work. Its PR resolves when no better pick exists. The header names the resolved branch, and `sync` shows `behind`.
- A worktree with no commits beyond the base shows the empty state. A sibling worktree's PR never attaches to it.
- A reused branch name never resurrects an earlier, unrelated PR. Old PRs do not contain this worktree's commits.
- The worktree's own merged PR shows as history while the space stays parked on its branch, even after the base absorbs the merge.
- A squash-merged PR shows as history after GitHub deletes its remote branch. The parked tip is exactly the PR's head, and that proves it.
- A rebase discards the old publication points. Between the rebase and its force-push, the tab shows the empty state. The push restores it on the next poll.

### Derived state

- `merge` folds GitHub's fields to the blockers worth surfacing: `CONFLICTING`/`DIRTY` → `conflicting`, `BLOCKED` → `blocked`, and everything else (`CLEAN`, `BEHIND`, `UNSTABLE`, still-computing `UNKNOWN`) → `clean`, which the footer shows as nothing.
- `mergeable=UNKNOWN` is GitHub computing lazily. It folds to `clean` unless `mergeStateStatus` is `DIRTY`.
- `sync` compares the pinned `HEAD` OID to the PR's `head_oid`: equal is `in_sync`, `HEAD` ahead is `unpushed` with a `git rev-list --count` count, and `head_oid` ahead is `behind`. If the PR head object is unavailable locally, the relation is `unknown`; reviewr never guesses `in_sync`.
- `unpushed` means the checks and comments on screen describe an older commit than the local tree.

### Checks

- A check row is the latest run for its name. A passed re-run replaces an earlier failure.
- Check runs and commit statuses normalize into one list.
- A top-level rollup gives the overall pass or fail.

### Comments

- Three surfaces merge into one list: submitted reviews, inline threads, and conversation comments.
- A bot's PR-level posts collapse to its latest. A human's are each kept.
- `is_resolved` and `is_outdated` come from GitHub, never recomputed against the worktree.
- Outdated and resolved threads stay in the list with their marker.
- Each surface reads its newest 100 rows, never paged to exhaustion. A further page on any surface — reviews, comments, threads, or checks — sets `truncated`, and `pr-tab.md` marks the capped list, so it is never presented as complete.

### Refresh

- The first fetch starts when the panel opens, so the tab is populated before the user reaches it.
- A refetch fires on entering the tab, on the `refresh` binding (default `r`), and on the agent's turn-end (a `working` → `idle`/`done` edge) on any tab. A turn may have pushed or merged, changing forge state with no other local signal, and one fetch per turn keeps the tab fresh before it is entered.
- A fallback poll refetches every 60 seconds while the tab is active. Off the tab there is no polling.
- The locally derived state — the pinned `HEAD` and base, the publication points, the tiebreak — moves on a mere commit or push, so it is freshness, never identity. Its churn alone never blanks the tab.
- A refresh that observes a different repository target or origin clears the current PR. reviewr cannot prove the snapshot still describes the same pull request (`overview.md` Continuity).
- Every other observed change keeps the snapshot painted and refetches behind it. The same pull request with newer work is stale, not wrong. The in-flight glyph covers the gap (`tui.md`).
- Either observation starts the replacement fetch at once, on or off the tab, so entering the tab finds fresh work already underway.
- One fetch is in flight at a time. One or more triggers arriving mid-flight supersede its result and start one fresh fetch when it completes.
- A GitHub change during a fetch can appear on the following fetch.
- Each fetch uses one validated config snapshot for host and base selection (→ CFG-ONE-SNAPSHOT, `config.md`).
- A GitHub result paints only if the current config, repository target, pinned `HEAD`, pinned base, and
  publication points still match the input that produced it. If reviewr cannot prove that match, the
  result never paints, and one replacement fetch starts against the current input.
- If the repository target is proven unchanged before a later branch-state read fails, the visible
  same-target snapshot stays with a retry notice; the next refresh performs a fresh GitHub fetch.
- The snapshot re-derives in full each fetch. reviewr keeps no hidden or historical PR cache beyond the visible snapshot.
- Exiting reviewr stops scheduling and restores the terminal immediately. No later PR completion
  can paint.

## Failure semantics

reviewr reads GitHub and never writes it, so every failure degrades to a clear state. `Changes` and `All files` are unaffected.

- A same-input failure preserves the visible snapshot and shows its remedy. With no same-input snapshot, the remedy fills the tab.

| failure               | remedy shown                      |
| --------------------- | --------------------------------- |
| missing `gh`          | the install step                  |
| unauthenticated fetch | `gh auth login --hostname <host>` |
| any other fetch error | the retry error                   |

- A failure before the repository target resolves replaces any snapshot with the retryable Git error. reviewr cannot prove that the snapshot still belongs to the current target.
- A branch-state Git failure after the same repository target resolves preserves the visible
  same-target snapshot with the retryable Git error.
- An unsupported origin names the host and points Enterprise users to `github_host`.
- No PR at any lifecycle state shows a calm empty state. The next poll lights the tab up when a PR appears.
- Every read is side-effect-free.
- Two active PR tabs on one worktree converge within one poll interval. An inactive tab catches up when entered.

## Non-goals

- No writes to GitHub. reviewr never posts, resolves a thread, re-runs a check, or merges. It never routes PR feedback to the agent.
- No repository selector or cross-repository search.
- No different parent repositories across sibling worktrees from one clone. Use a separate clone for each parent.
- No SSH host-alias normalization. An alias-only repository needs a canonical-host remote.
- No discovery of an unrecorded publication name on a non-`origin` remote.
- No event subscription. The snapshot polls `gh`, no webhook or socket.
- No server-version compatibility layer for Enterprise schemas.
- No second forge. GitHub via `gh` only.

## Related specs

- [configuration](./config.md)
- [pr-tab](./pr-tab.md)
- [herdr-host](./herdr-host.md)
- [overview](./overview.md)
