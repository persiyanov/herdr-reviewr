---
Status: Current
Created: 2026-06-27
Last edited: 2026-07-11
---

# forge host

How reviewr reads one pull/merge request from its forge — GitHub via `gh`, GitLab via `glab`, Bitbucket Data Center via `curl` — identity, state, checks, comments — for the read-only `PR` tab (`tui.md`; GitLab shows `MR`). It never writes back.

## Overview

reviewr resolves the worktree's open pull/merge request across the candidate branches its work could be published under, then reads a snapshot of it through the origin's forge on each poll. The snapshot is the single value the `PR` tab renders, whichever forge produced it.

```
PR #226  open  persiyanov/deep-research-benchmark → main   ⇡ 2 unpushed
  merge      ⚠ conflicts with main
  checks     ✗ failing — ✓ build-main-image · ✓ review · ✗ tests
  comments   5 (newest first) — @you 5m · @codex 2h · @claude 2h · …
```

The snapshot, forge-neutral in shape:

| field          | type      | meaning                                                              |
| -------------- | --------- | --------------------------------------------------------------------- |
| `number`       | int?      | PR/MR number, `null` when none resolves                                |
| `title`, `url` | string    | identity                                                               |
| `state`        | enum      | `open`, `merged`, or `closed`                                          |
| `is_draft`     | bool      | draft flag                                                             |
| `head_ref`     | string    | the head branch name, which may differ from the local branch           |
| `head_is_fork` | bool      | the head lives in another repository/project                          |
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
| `author`, `author_is_bot`    | string, bool | the author handle and whether it is a bot                        |
| `anchor`                     | string | `path:line` for a `finding`, the literal kind word otherwise            |
| `body`, `snippet`            | string | the text as the forge returns it, no chrome-stripping or format parsing; only a `finding` carries a snippet |
| `created_at`                 | time   | post time, the newest-first sort key                                    |
| `is_resolved`, `is_outdated` | bool   | thread state for a `finding`, always false otherwise                    |
| `reply_count`                | int    | replies on a `finding`'s thread beyond the root                         |

## Behavior

### Forge hosts

Three keys in reviewr's `config.toml` name one on-prem or Enterprise host per forge: `github_host`, `gitlab_host`, `bitbucket_host`. Their value contracts live in `config.md`.

```toml
github_host    = "github.example.com"
gitlab_host    = "gitlab.corp.com"
bitbucket_host = "bitbucket.corp.com"
```

`github.com` and `gitlab.com` are built in and always supported, whether or not their Enterprise siblings are configured. There is no `bitbucket.org` built-in: Bitbucket Cloud is a different API than Bitbucket Data Center and is not supported, so it degrades the same as any other unconfigured host, naming `bitbucket_host`.

Host matching is case-insensitive. Host identity comes from `origin`'s primary fetch URL after Git's `url.*.insteadOf` rewrite. A separate push URL does not affect PR/MR reads.

| condition                                                                  | outcome                                                                     |
| --------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| exact `github_host`/`gitlab_host`/`bitbucket_host` or its SSH alias `<host>-<alias>` | reviewr reads the repository from that forge's configured host                |
| `github.com` or `gitlab.com`, or an SSH alias of either                     | reviewr reads the repository from that forge's `.com`                        |
| any other hosted `origin`, including `bitbucket.org`                        | reviewr names the unsupported host and points to the matching `*_host` key   |
| missing `origin` or an origin without a host                                | reviewr says the PR tab needs a supported origin                             |
| supported host without a valid repository path for its forge                | reviewr says the origin is malformed                                         |

The alias rows apply only to scp-style and `ssh://` origins. An alias is a trusted naming convention; reviewr does not inspect SSH config to verify where it connects.

Hosted URL forms use `http://`, `https://`, `git://`, or `ssh://`/scp-style. File URLs and other schemes are not repository identities and remain unsupported. A port on an `http`/`https`/`git` origin is unsupported, since the canonical host has none.

Path parsing is forge-specific:

- **GitHub**: `owner/repo`, exactly two segments.
- **GitLab**: `owner/repo`, where `owner` may nest groups (`group/subgroup/repo`). The whole path is percent-encoded when passed to `glab api`.
- **Bitbucket Data Center**: an HTTP(S) clone URL carries an `/scm/` prefix (`https://host/scm/PROJ/repo.git`); an SSH one does not (`ssh://git@host/PROJ/repo.git`). Both parse to a project key and a repo slug. A Bitbucket-classified HTTP(S) origin missing the `/scm/` prefix is malformed.

A fetch target is the canonical matched host plus the origin's owner/project and repository. A fetch input adds the pinned local branch, `HEAD`, candidate branches, and base configuration. `GH_HOST` cannot redirect a fetch to another GitHub instance; GitLab and Bitbucket have no analogous env-var redirect to guard against.

### Concept mapping

The snapshot model is forge-neutral; each backend maps its forge's own vocabulary onto it:

| Snapshot field | GitHub (`gh`) | GitLab (`glab`) | Bitbucket DC (`curl`) |
| --- | --- | --- | --- |
| `number` | PR number | MR `iid` | PR `id` |
| `is_draft` | draft flag | `draft` flag | draft flag (DC 8.x+), else `false` |
| `head_ref` | head branch | `source_branch` | `fromRef.displayId` |
| `head_is_fork` | `isCrossRepository` | cross-project MR (`source_project_id != target_project_id`) | `fromRef.repository != toRef.repository` |
| `merge` | `mergeable`/`mergeStateStatus` → conflicting/blocked/clean | `detailed_merge_status` → conflicting/blocked/clean | `/merge` endpoint conflicts/vetoes → conflicting/blocked/clean |
| `checks` | check runs + commit statuses | head pipeline's jobs | build-status API on the head commit |
| `comments` | reviews, inline threads, conversation comments merged | discussions: positioned notes → `finding` (with resolved state), other user notes → `comment`, system notes dropped | `/activities`: inline comments → `finding` (anchor from comment anchor), general comments → `comment`; no review-body surface |

### Resolution

- Each fetch pins `HEAD` and the base ref to commit OIDs at its start. Every ancestry test, distance, and sync count uses the pins, so one fetch reads one consistent local state while the agent commits beside it.
- **GitHub** resolves the open PR across all candidate branches in one aliased GraphQL `pullRequests(headRefName: …, states: OPEN)` call. Its detail comes from a direct `pullRequest(number: …)` query, because `mergeable` populates only on direct access.
- **GitLab** and **Bitbucket Data Center** have no OR-across-branches equivalent to GitHub's aliased query, so resolution is one list call per candidate branch — `merge_requests?source_branch=<name>&state=opened` (GitLab) or `pull-requests?at=refs/heads/<branch>&state=OPEN&direction=OUTGOING` (Bitbucket) — capped at 8 candidates by the existing derivation, acceptable at the 60 s poll interval. Both feed the same `Pick` policy GitHub's single call feeds.
- Exactly one open PR/MR across the candidates resolves, under whichever name it lives.
- Several open PRs/MRs resolve to the earliest candidate in derivation order. Several on that one name disambiguate by the head commit equal to the pinned `HEAD`. Failing that, reviewr surfaces the ambiguity count, never a silent guess.
- With no open PR/MR anywhere, the newest-created merged or closed one shows as historical state (one further list call per candidate, same shape). With none at all, the empty state names the queried candidates, so a surprising resolution is inspectable.
- A fork PR/MR reads checks, comments, and merge state from the base repository/project. The resolution key is the head branch name, not a (repository, name) pair, so a same-named fork branch can match. The `⑂` header marker makes that case visible.
- A detached `HEAD` shows the empty state. reviewr never queries an unfiltered branch name, which some forges would read as "any branch."

### Candidate branches

The names this worktree's work could be published under, re-derived from local git on every fetch, deduped in this order. Steps 1 and 3 are always included. Step 2 contributes nearest tips up to a total of 8 names, farthest evicted first, never evicting steps 1 or 3.

1. Git's recorded upstream (`branch.<name>.merge`), stripped of its remote prefix, unless it names a configured base branch. `@{push}` is never consulted: git computes a destination even when nothing is recorded, which would shadow a real upstream.
2. Remote-tracking branches under `refs/remotes/origin/*` (excluding `origin/HEAD` and the base branches) whose tip is ancestry-comparable with the pinned `HEAD`: equal to it, an ancestor of it carrying non-base work, or a descendant of it. Nearest-first by `HEAD...tip` distance, ties lexicographic. With no base resolvable, only equal and descendant tips qualify.
3. The local branch name, always.

What a user observes:

- A worktree pushed as `git push origin HEAD:<other-name>` resolves its PR/MR. The push updated a distance-0 candidate.
- One tip pushed under two names resolves to whichever name holds the open PR/MR.
- A stale upstream never hides a live PR/MR on another candidate. An open one beats a merged one and beats none.
- A teammate's branch parked at this worktree's exact `HEAD` never beats the branch git says this worktree pushes to.
- Stacked branches resolve to the nearest branch of the stack holding an open PR/MR. The recorded push destination outranks the whole stack.
- A remote branch descending from `HEAD` can be a colleague's continuation of this work. Its PR/MR resolves when no better candidate has one, and the header names the branch.
- Between a rebase and its force-push, a branch published under a different name with no upstream shows the empty state. The push restores it on the next poll.

### Derived state

- `merge` folds each forge's own blocker fields to the three-value model in the concept-mapping table above: an actual conflict → `conflicting`; a gate a reviewer can act on (a discussion block, a policy veto, a required approval) → `blocked`; everything else, including a still-computing response, → `clean`, which the footer shows as nothing.
- `sync` compares the pinned `HEAD` OID to the PR/MR's head commit: equal is `in_sync`, `HEAD` ahead is `unpushed` with a `git rev-list --count` count, and the head commit ahead is `behind`. If the head commit is unavailable locally, the relation is `unknown`; reviewr never guesses `in_sync`.
- `unpushed` means the checks and comments on screen describe an older commit than the local tree.

### Checks

- A check row is the latest run for its name. A passed re-run replaces an earlier failure.
- Every forge's check runs and commit/build statuses normalize into one list.
- A top-level rollup gives the overall pass or fail.

### Comments

- Every forge's comment surfaces merge into one list: GitHub's reviews, inline threads, and conversation comments; GitLab's discussions split by whether they carry a position; Bitbucket's activity feed split into anchored and general comments. GitHub and GitLab distinguish a `review` kind; Bitbucket has no review-body surface, so it never produces one.
- A bot's PR/MR-level posts collapse to its latest. A human's are each kept.
- `is_resolved` and `is_outdated` come from the forge, never recomputed against the worktree.
- Outdated and resolved threads stay in the list with their marker.
- Each surface reads one page of 100 rows (20 for the historical-fallback list), never paged to exhaustion. A further page on any surface sets `truncated`, and the UI shows `+more ↗`, so a capped list is never presented as complete.

### Refresh

- The first fetch starts when the panel opens, so the tab is populated before the user reaches it.
- A refetch fires on entering the tab, on `r`, and on the agent's turn-end (a `working` → `idle`/`done` edge) while the tab is active. A turn may have pushed or merged, changing forge state with no other local signal.
- A fallback poll refetches every 60 seconds while the tab is active. Off the tab there is no polling.
- A fetch-input change observed on refresh clears the current PR/MR. It starts a fetch while the tab is active; otherwise the next tab entry starts it.
- A GitHub fetch with an open PR is two GraphQL calls; one that checks historical PRs is three. A GitLab or Bitbucket fetch is one list call per candidate (≤8) to resolve, plus a detail call, a checks call, and a comments call — more calls than GitHub's aliased query, but on the same 60 s cadence. All run on a worker thread, so no forge CLI ever blocks input or scrolling.
- One fetch is in flight at a time. A trigger arriving mid-flight supersedes its result and starts a fresh fetch when it completes.
- Each fetch uses one snapshot of reviewr's config for host and base selection. A later fetch sees a config edit without restarting reviewr.
- A completed fetch updates the PR tab only when the current worktree and config still derive the same input.
- The snapshot re-derives in full each fetch. reviewr keeps no hidden or historical PR/MR cache beyond the visible snapshot.

## Failure semantics

reviewr reads its forge and never writes it, so every failure degrades to a clear state. `Changes` and `All files` are unaffected.

- A missing CLI tool (`gh` or `glab` not on `PATH`; Bitbucket's `curl` absent) preserves a same-input snapshot and shows the install remedy, naming the missing tool. With no same-input snapshot, the remedy fills the tab.
- An unauthenticated fetch preserves a same-input snapshot and shows the forge's login remedy: `gh auth login --hostname <host>` for GitHub, `glab auth login --hostname <host>` for GitLab. With no same-input snapshot, the remedy fills the tab.
- Bitbucket has no CLI login: a missing or rejected token shows `NoToken`, naming `BITBUCKET_TOKEN` and git credentials as the two ways to supply one. A `401`/`403` from `curl` also reads as `NotAuthed` for Bitbucket, with the same remedy wording.
- An unsupported origin names the host and points to the matching `*_host` key.
- Any other fetch failure preserves a same-input snapshot and shows the retry error, naming the forge. With no same-input snapshot, the error fills the tab.
- A missing `origin` is a clean absence. Any other git command failure is transient and never read as absence, a detached `HEAD`, or an unsupported remote.
- No open PR/MR shows a directional empty state naming the queried candidates. The next poll lights the tab up when one appears.
- Every read is idempotent and side-effect-free. A retry returns the same snapshot.
- Two active PR tabs on one worktree converge within one poll interval. An inactive tab catches up when entered.

## Non-goals

- No writes to any forge. reviewr never posts, resolves a thread, re-runs a check, or merges, on GitHub, GitLab, or Bitbucket. It never routes PR/MR feedback to the agent.
- No event subscription. The snapshot polls its forge's CLI or `curl`, no webhook or socket.
- No server-version compatibility layer. An Enterprise/Data Center schema that lacks the snapshot's fields fails like any unavailable forge API.
- No Bitbucket Cloud (`bitbucket.org`) support. Its API differs from Data Center's and is out of scope; it degrades like any other unsupported host.
- No native HTTP client in the binary. GitHub and GitLab access shells out to `gh`/`glab`; Bitbucket shells out to `curl`. Networking and its proxy/CA/auth concerns stay in subprocesses.

## Related specs

- [configuration](./config.md)
- [tui](./tui.md)
- [herdr-host](./herdr-host.md)
- [overview](./overview.md)
