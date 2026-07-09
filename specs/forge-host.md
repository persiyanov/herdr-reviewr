---
Status: Current
Created: 2026-06-27
Last edited: 2026-07-09
---

# forge host

How herdr-reviewr reads one pull request's state from GitHub — identity, state, checks, and comments — through the `gh` CLI for the read-only `PR` tab (`tui.md`), never writing back.

## Overview

reviewr resolves the worktree's open pull request — the PR whose head branch carries the worktree's work, found across the **candidate branches** the work could be published under (Resolution) — and, on each poll, reads a snapshot of it through `gh` on `PATH`. The snapshot is the single value the `PR` tab renders.

```
PR #226  open  persiyanov/deep-research-benchmark → main   ⇡ 2 unpushed
  merge      ⚠ conflicts with main
  checks     ✗ failing — ✓ build-main-image · ✓ review · ✗ tests
  comments   5 (newest first) — @you 5m · @codex 2h · @claude 2h · …
```

The snapshot:

- `number`, `title`, `url` (int, string, string) — identity; `number` is `null` when the branch has no PR.
- `state` (enum, `open`/`merged`/`closed`) and `is_draft` (bool) — lifecycle; only `open` is the live case.
- `head_ref` (string) — the PR's head branch name, shown in the header; the worktree's local branch name may differ, and this is the name that resolved.
- `head_is_fork` (bool) — the head branch lives in another repository (GitHub's `isCrossRepository`), marked in the header; without it a same-named fork PR would show exactly the expected name.
- `base_ref` (string) — the merge target; the PR head commit is read for `sync` (below) but not stored.
- `merge` (enum, `clean`/`conflicting`/`blocked`) — the actionable merge blockers, derived from GitHub's `mergeable` and `mergeStateStatus`.
- `sync` (enum, `in_sync`/`unpushed`/`behind`, with a count) — local `HEAD` vs `head_oid`.
- `checks` (list) — one row per latest check: `name` and `status` (the conclusion folded in).
- `comments` (list) — one row per comment, newest first: `kind`, `author`, `author_is_bot`, `anchor`, `body`, `snippet`, `created_at`, `is_resolved`, `is_outdated`, `reply_count`.
- `truncated` (bool) — a capped surface (reviews/comments/threads/checks) had a further page; the lists are a prefix, and the UI flags it rather than showing partial counts as complete.

A `comments` row:

- `kind` (enum, `review`/`comment`/`finding`) — a submitted review's body, a plain PR conversation comment, or an inline finding. Only `finding` carries `anchor` = `path:line` and `snippet`; the others are prose with `anchor` = the literal kind word.
- `author` (string) and `author_is_bot` (bool) — the comment's `@login` and whether the author is a bot.
- `body` (string) — the text as GitHub returns it, with no per-author chrome-stripping or format parsing.
- `created_at` (timestamp) — when the comment was posted, and the list's newest-first sort key.
- `is_resolved`, `is_outdated` (bool) — thread state for a `finding`; always false for `review`/`comment` (they have no anchor).
- `reply_count` (int) — replies on a `finding`'s thread beyond the root.

## Behavior

### Resolution

- Each fetch pins `HEAD` and the base ref to commit OIDs at its start; every ancestry test, distance, and the `sync` count use the pins, so one fetch reads one consistent local state even while the agent commits or rebases beside it.
- reviewr derives the worktree's **candidate branches** (below) and resolves the **open** PR across all of them in one aliased GraphQL `pullRequests(headRefName: …, states: OPEN)` call, then reads its detail with a direct `pullRequest(number: …)` query — `mergeable` only populates on direct PR access, never through the list connection.
- Exactly one open PR across the candidates resolves — whichever candidate name it lives under.
- Several open PRs: the earliest candidate in derivation order wins — the recorded push destination outranks an inferred branch, which outranks the bare local name, so a teammate's branch parked at this worktree's exact `HEAD` never beats the branch git says this worktree pushes to. Several open PRs on that one winning name disambiguate by `headRefOid` equal to the pinned `HEAD`; failing that, reviewr surfaces the ambiguity count rather than guessing silently.
- No open PR anywhere: the newest-created merged or closed PR across the candidates shows as historical state (`merged`/`closed`); with none at all, the empty state — which names the branch(es) it queried, so a resolution that surprises is inspectable, never silent.
- A fork PR reads `checks`, `comments`, and merge state from the **base** repository, where GitHub computes them. The resolution key is the head branch *name*, not a (repository, name) pair — a same-named fork branch can match; accepted, unchanged from the name-only lookup this design replaces. Because `head_ref` shows exactly the expected name in this one case, a cross-repository head is marked in the header (GitHub's `isCrossRepository`), so a fork collision is visible rather than silent.
- A detached `HEAD` — e.g. after `gh pr merge --delete-branch` — shows the empty state: with no local branch there is no worktree identity to publish, and the detached state is post-merge cleanup, not a review seat. reviewr never queries `headRefName:""`, which GitHub reads as unfiltered and would mis-resolve to an unrelated PR.

### Candidate branches

The candidates are the branch names this worktree's work could be published under — derived entirely from local git on every fetch, no persisted state, deduped in this order. Steps 1 and 3 are always included; step 2 contributes its nearest tips up to a set total of 8 names, farthest evicted first — a flurry of checkpoint pushes can crowd out a distant tip, never the recorded destination or the local name:

1. git's recorded upstream — the `branch.<name>.merge` record a `push -u` or `--track` writes — stripped of its remote prefix, unless it names a configured base branch (`review-model.md`'s `base_branches`). `@{push}` is deliberately not consulted: with any remote present git *computes* a destination equal to the local branch name even when nothing is recorded, which would shadow a real upstream.
2. Remote-tracking branches under `refs/remotes/origin/*` (excluding `origin/HEAD` and the base branches) whose tip is ancestry-comparable with the pinned `HEAD`: the tip equals it, is an ancestor of it not reachable from the pinned base, or descends from it. Ordered nearest-first by `HEAD...tip` distance, equal distances lexicographic, so the order is deterministic. With no base ref resolvable locally, only the equal and descendant tips qualify — "an ancestor carrying non-base work" is defined only against a base.
3. The local branch name, always — the old lookup's key, kept so no workflow that resolves today stops resolving.

The candidate set replaces a precedence: a wrong or stale entry costs nothing when another candidate holds the open PR, because GitHub — not a local rule — says which name a PR actually lives under. Consequences a user observes:

- A worktree pushed as `git push origin HEAD:<other-name>` — no `-u`, local name unchanged — resolves its PR: the push updated `refs/remotes/origin/<other-name>`, a distance-0 candidate.
- One tip pushed under two names (a checkpoint push beside the PR branch) resolves to whichever name holds the open PR — a tie is not a failure.
- A stale upstream — the branch of an already-merged PR, or a `push -u` backup under a PR-less name — cannot hide a live PR on another candidate: an open PR beats a merged one and beats no PR.
- Stacked branches resolve to the nearest branch of the stack holding an open PR — candidate order is nearest-first, and the recorded push destination outranks the whole stack.
- A remote branch that merely *descends* from the pinned `HEAD` can be someone else's continuation of this work; its PR can resolve when no better candidate has one. Accepted: the PR header names its branch, so the attribution is visible — and the descendant case is also exactly how a colleague's fix pushed onto the agent's PR stays on screen.
- Between a local rebase or amend and its force-push, a branch published under a different name with no upstream shows the empty state; the push restores it on the next poll. Accepted: the window is short and self-heals, and keeping resolution stateless is worth it.

### Derived state

- `merge` folds GitHub's `mergeable` and `mergeStateStatus` to the blockers worth surfacing: `CONFLICTING`/`DIRTY` → `conflicting`, `BLOCKED` → `blocked`; everything else (`CLEAN`, `BEHIND`, `UNSTABLE`, and still-computing `UNKNOWN`) → `clean`, which the footer shows as nothing.
- `mergeable=UNKNOWN` is GitHub computing lazily — it folds to `clean`, never asserted as a conflict unless `mergeStateStatus` is `DIRTY`.
- `sync` compares the fetch's pinned `HEAD` OID to `head_oid` — equal is `in_sync`, `HEAD` ahead is `unpushed` (count via `git rev-list --count <head_oid>..HEAD`), `head_oid` ahead is `behind`. The pin keeps the pairing honest: a checkout or commit landing mid-fetch never pairs one branch's PR with another branch's count.
- `unpushed` means the checks and comments on screen describe an older commit than your local tree.

### Checks

- A check row is the **latest** run for its `name` — a re-run supersedes the prior run, so a passed re-run replaces an earlier failure rather than listing both.
- Check runs (Actions/Apps) and commit statuses (external CI) normalise into one list.
- A top-level rollup gives the overall pass/fail across them.

### Comments

- Three surfaces merge into one `comments` list, all read in the one detail query: submitted reviews (`reviews`), inline threads (`reviewThreads`), and plain conversation comments (`comments`).
- All three are read because the AI reviewers split across them — one posts a review body, the other a plain comment.
- A bot's PR-level posts collapse to its **latest**; a human's are each kept.
- `is_resolved` and `is_outdated` come from `reviewThreads` (inline comments only) — relevance is GitHub's, never recomputed against the worktree.
- Outdated and resolved threads stay in the list with their marker, not filtered out.
- Each surface is read to a fixed cap of 100 rows (one page), not paged to exhaustion — a deliberate v1 bound, since a PR in a review sidebar effectively never exceeds it. When any surface (reviews, comments, threads, or checks) reports a further page, `truncated` is set and the UI shows a `+more on GitHub ↗` marker, so a capped list is never presented as complete.
- The list is sorted newest-first by `created_at`.

### Refresh

- The first fetch starts when the reviewr panel opens, not on first switching to the `PR` tab, so the tab is already populated by the time the user reaches it.
- A refetch fires on entering the tab, on the manual `r`, and on the agent's turn-end (a `working`→resting edge — `idle` or `done`) while the tab is active — that turn may have pushed or run `gh pr merge`, changing forge state with no other local signal.
- A fallback poll refetches every 60 seconds while the tab is active, covering forge-side changes (a reviewer's comment) that have no local signal. Off the tab there is no polling; re-entering refetches.
- Each fetch is two GraphQL calls (resolve the number across the candidates, then read the detail) run on a worker thread and delivered to the UI when complete, so the `gh` calls never block input or scrolling. The historical fallback (no open PR) adds at most one more resolve call.
- Only one fetch is in flight at a time. A trigger arriving mid-flight marks the fetch dirty, and a dirty fetch re-runs immediately on completion — so a turn-end push landing while a poll is in flight is picked up seconds later, not at the next poll.
- The snapshot re-derives in full each fetch — candidates from local git, PR state from GitHub; reviewr keeps no PR state across fetches.

## Failure semantics

reviewr reads GitHub but never writes it, so every failure degrades to a clear state and the rest of the app (Changes, All files) is unaffected.

- `gh` absent, present but not authenticated, or a remote no supported forge handles (today, any non-GitHub remote) — each shows its own remediation line naming the command that unblocks it; any other failure (a wrong-account 404, a transient API error) shows a generic retry message, never read as "no PR". The next poll or `r` re-attempts cleanly.
- A git command *failing* during any of the fetch's local reads — the remote URL, the branch, candidate derivation (a lock held by `git gc`, a ref pruned mid-enumeration) — is a transient fetch failure: the last good snapshot freezes with the retry marker, exactly like an unreachable API. Only a command that *succeeds and finds nothing* (no upstream configured, no matching refs) narrows the answer; failure is never read as absence, a detached `HEAD`, or a non-forge remote.
- No open PR shows a directional empty state naming the queried candidates; the next poll lights the tab up the moment a PR appears, with no manual `r`.
- A rate-limited or unreachable poll freezes on the last good snapshot with a quiet marker; a failed poll never blanks a populated tab.
- Second run: every read is idempotent and side-effect-free, so a retry returns the same snapshot.
- Concurrent runs: two sidebars on one worktree each re-derive independently and self-heal on their own polls — harmless, since neither writes and there is no shared local state; mid-transition they may briefly render different snapshots, converging within one poll interval.

## Non-goals

- No writes to GitHub — reviewr never posts, resolves a thread, re-runs a check, merges, or routes feedback to the agent.
- No event subscription — the snapshot polls `gh`; reviewr opens no webhook or socket (mirrors `herdr-host.md`'s poll-don't-subscribe).
- No second forge — GitHub via `gh` only; the forge-agnostic core (Changes, All files, the diff viewer) must not import this module.

## Decisions

- `gh` over a REST/GraphQL library — the user's authenticated `gh` is the stable, credential-free interface, matching the `herdr` CLI dependency already in `herdr-host.md`. Rejected: a bundled HTTP client with its own token discovery.
- Resolve across the worktree's candidate branches, disambiguated by GitHub — the PR's identity is where the work is published, which git's push records and remote-tracking refs reveal; a local-name lookup alone misses every `push HEAD:<other-name>` worktree, the workflow agents use constantly. GitHub says which candidate actually holds the PR, so a stale or wrong candidate costs nothing. Rejected: a local precedence picking one name before asking GitHub — a stale upstream or a distance tie silently hides an existing PR, and hostile-schedule walks showed it resolving the wrong PR outright. Also rejected: matching the repo's open PRs by `headRefOid` through the list API — capped at the first 100 open PRs (a silent miss in a busy repo) and still needing a name for the merged/closed fallback, so it is two mechanisms in practice. Reversed if worktrees routinely need PRs whose head branches were never pushed from (or fetched into) the local repo.
- Candidate derivation is stateless — re-derived from git on every fetch, with `HEAD` and the base pinned to OIDs per fetch, no cached or pinned PR number — so one rule explains every resolution and there is no invalidation to get wrong. The cost is the documented rebase-window gap above. Rejected: a session-pinned PR number, sticky until the PR closes.
- Read all three comment surfaces — the two AI reviewers split across review bodies and plain comments, so reading one would miss half the reviews. Rejected: review bodies only.
- Relevance from GitHub, not local rebasing — GitHub computes `is_outdated` against the PR head, so reviewr reads it rather than re-anchoring comments to the worktree. Rejected: local line-rebasing.
- GitHub-only — `gh` is one well-understood forge; a generic forge layer carries token discovery and per-forge API shapes with no current user. Rejected: a forge abstraction up front.

## Open decisions

- None.

## Related specs

- `./tui.md`
- `./herdr-host.md`
- `./overview.md`
