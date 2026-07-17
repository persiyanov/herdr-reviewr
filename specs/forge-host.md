---
Status: Current
Created: 2026-06-27
Last edited: 2026-07-15
---

# forge host

How reviewr reads one pull request from GitHub — identity, state, checks, comments — through the `gh` CLI, for the read-only `PR` tab (`tui.md`). It never writes back.

## Overview

reviewr resolves the worktree's open pull request across the candidate branches its work could be published under, then reads a snapshot of it through `gh` on each poll. The snapshot is the single value the `PR` tab renders.

```
PR #226  open  persiyanov/deep-research-benchmark → main   ⇡ 2 unpushed
  merge      ⚠ conflicts with main
  checks     ✗ failing — ✓ build-main-image · ✓ review · ✗ tests
  comments   5 (newest first) — @you 5m · @codex 2h · @claude 2h · …
```

The snapshot:

| field          | type      | meaning                                                                |
| -------------- | --------- | ---------------------------------------------------------------------- |
| `number`       | int?      | PR number, `null` when no PR resolves                                  |
| `title`, `url` | string    | identity                                                               |
| `body`         | string    | the PR description as GitHub returns it, empty when none               |
| `state`        | enum      | `open`, `merged`, or `closed`                                          |
| `is_draft`     | bool      | draft flag                                                             |
| `head_ref`     | string    | the PR's head branch name, which may differ from the local branch      |
| `head_is_fork` | bool      | the head lives in another repository (GitHub's `isCrossRepository`)    |
| `base_ref`     | string    | the merge target                                                       |
| `merge`        | enum      | `clean`, `conflicting`, or `blocked`                                   |
| `sync`         | enum      | `in_sync`, `unpushed`, `behind`, or `unknown`, with a count when known |
| `checks`       | list      | one row per latest check: `name` and `status` (conclusion folded in)   |
| `comments`     | list      | one row per comment, newest first                                      |
| `truncated`    | bool      | a capped surface had a further page, so a list is a prefix             |

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

A readable, supported remote named `upstream` is authoritative. An absent or unusable `upstream`
identity falls back to `origin`; a Git read failure stays visible and never falls through. Both use
the primary fetch URL after Git's `url.*.insteadOf` rewrite. A separate push URL does not affect PR
reads.

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

- Each fetch pins `HEAD` and the base ref to commit OIDs. Ancestry, distance, and sync calculations use those pins while the agent commits beside it.
- The open PR resolves across all candidate branches.
- Exactly one open PR across the candidates resolves, under whichever name it lives.
- Several open PRs resolve to the earliest candidate in derivation order. Several on that one name disambiguate by `headRefOid` equal to the pinned `HEAD`. Failing that, reviewr surfaces the ambiguity count, never a silent guess.
- With no open PR anywhere, the newest-created merged or closed PR shows as historical state. With none at all, the body says only `No pull request yet. Ready to ship?`
- A fork PR reads checks, comments, and merge state from the base repository. The resolution key is the head branch name, not a (repository, name) pair, so a same-named fork branch can match. The `⑂` header marker makes that case visible.
- A detached `HEAD` shows the empty state. reviewr never queries `headRefName:""`, which GitHub reads as unfiltered.

### Candidate branches

Each fetch re-derives and deduplicates the possible publication names in this order. Steps 1 and 3 always remain. Step 2 contributes the nearest tips up to 8 total names.

1. Git's recorded upstream (`branch.<name>.merge`), stripped of its remote prefix, unless it names a configured base branch. `@{push}` is never consulted: git computes a destination even when nothing is recorded, which would shadow a real upstream.
2. Remote-tracking branches under `refs/remotes/origin/*` (excluding `origin/HEAD` and the base branches) whose tip is ancestry-comparable with the pinned `HEAD`: equal to it, an ancestor of it carrying non-base work, or a descendant of it. Nearest-first by `HEAD...tip` distance, ties lexicographic. With no base resolvable, only equal and descendant tips qualify.
3. The local branch name, always.

What a user observes:

- A worktree pushed as `git push origin HEAD:<other-name>` resolves its PR. The push updated a distance-0 candidate.
- One tip pushed under two names resolves to whichever name holds the open PR.
- A stale upstream never hides a live PR on another candidate. An open PR beats a merged one and beats none.
- A teammate's branch parked at this worktree's exact `HEAD` never beats the branch git says this worktree pushes to.
- Stacked branches resolve to the nearest branch of the stack holding an open PR. The recorded push destination outranks the whole stack.
- A remote branch descending from `HEAD` can be a colleague's continuation of this work. Its PR resolves when no better candidate has one, and the header names the branch.
- Between a rebase and its force-push, a branch published under a different name with no upstream shows the empty state. The push restores it on the next poll.

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
- Each surface reads one page of 100 rows, never paged to exhaustion. A further page on any surface — reviews, comments, threads, or checks — sets `truncated`, and the UI shows `+more on GitHub ↗`, so a capped list is never presented as complete.

### Refresh

- The first fetch starts when the panel opens, so the tab is populated before the user reaches it.
- A refetch fires on entering the tab, on the `refresh` binding (default `r`), and on the agent's turn-end (a `working` → `idle`/`done` edge) while the tab is active. A turn may have pushed or merged, changing forge state with no other local signal.
- A fallback poll refetches every 60 seconds while the tab is active. Off the tab there is no polling.
- A fetch-input change observed on refresh clears the current PR. It starts a fetch while the tab is active; otherwise the next tab entry starts it.
- One fetch is in flight at a time. One or more triggers arriving mid-flight supersede its result and start one fresh fetch when it completes.
- A GitHub change during a fetch can appear on the following fetch.
- Each fetch uses one snapshot of reviewr's config for host and base selection. A later fetch sees a config edit without restarting reviewr.
- A GitHub result paints only if the current config, repository target, pinned `HEAD`, and candidate
  branches still match the input that produced it. If reviewr cannot prove that match, the result
  never paints. An active tab starts one replacement; an inactive tab waits for entry.
- If the repository target is proven unchanged before a later branch-state read fails, the visible
  same-target snapshot stays with a retry notice; the next refresh performs a fresh GitHub fetch.
- The snapshot re-derives in full each fetch. reviewr keeps no hidden or historical PR cache beyond the visible snapshot.
- Exiting reviewr stops scheduling and restores the terminal immediately. No later PR completion
  can paint.

## Failure semantics

reviewr reads GitHub and never writes it, so every failure degrades to a clear state. `Changes` and `All files` are unaffected.

- A missing `gh` preserves a same-input snapshot and shows the install remedy. With no same-input snapshot, the remedy fills the tab.
- An unauthenticated fetch preserves a same-input snapshot and shows `gh auth login --hostname <host>`. With no same-input snapshot, the remedy fills the tab.
- A failure before the repository target resolves replaces any snapshot with the retryable Git error. reviewr cannot prove that the snapshot still belongs to the current target.
- A branch-state Git failure after the same repository target resolves preserves the visible
  same-target snapshot with the retryable Git error.
- An unsupported origin names the host and points Enterprise users to `github_host`.
- Any other fetch failure preserves a same-input snapshot and shows the retry error. With no same-input snapshot, the error fills the tab.
- No PR at any lifecycle state shows a calm empty state. The next poll lights the tab up when a PR appears.
- Every read is side-effect-free.
- Two active PR tabs on one worktree converge within one poll interval. An inactive tab catches up when entered.

## Non-goals

- No writes to GitHub. reviewr never posts, resolves a thread, re-runs a check, or merges. It never routes PR feedback to the agent.
- No repository selector or cross-repository search. A readable, supported `upstream` is
  authoritative; an unusable `upstream` identity falls back to `origin`.
- No different parent repositories across sibling worktrees from one clone. Use a separate clone for each parent.
- No SSH host-alias normalization. An alias-only repository needs a canonical-host remote.
- No discovery of an unrecorded publication name on a non-`origin` remote. The local branch name or a recorded upstream must identify it.
- No event subscription. The snapshot polls `gh`, no webhook or socket.
- No server-version compatibility layer for Enterprise schemas.
- No second forge. GitHub via `gh` only.

## Related specs

- [configuration](./config.md)
- [tui](./tui.md)
- [herdr-host](./herdr-host.md)
- [overview](./overview.md)
