---
Status: Draft
Created: 2026-06-27
Last edited: 2026-06-27
---

# forge host

How herdr-reviewr reads one pull request's state from GitHub — identity, state, checks, and comments — through the `gh` CLI for the read-only `PR` tab (`tui.md`), never writing back.

## Overview

reviewr resolves the open pull request for the worktree's branch and, on each poll, reads a snapshot of it through `gh` on `PATH`. The snapshot is the single value the `PR` tab renders.

```
PR #226  open  persiyanov/deep-research-benchmark → main   ⇡ 2 unpushed
  merge      ⚠ conflicts with main
  checks     ✗ failing — ✓ build-main-image · ✓ review · ✗ tests
  comments   5 (newest first) — @you 5m · @codex 2h · @claude 2h · …
```

The snapshot:

- `number`, `title`, `url` (int, string, string) — identity; `number` is `null` when the branch has no PR.
- `state` (enum, `open`/`merged`/`closed`) and `is_draft` (bool) — lifecycle; only `open` is the live case.
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

- reviewr resolves the **open** PR for the branch via a GraphQL `pullRequests(headRefName: …, states: OPEN)` query, then reads its detail with a direct `pullRequest(number: …)` query — `mergeable` only populates on direct PR access, never through the list connection.
- A merged or closed PR on the branch does not count: a branch with one merged PR and one open PR shows the open one as the only PR, no ambiguity.
- Two or more **open** PRs on one branch is the only ambiguous case; reviewr surfaces the count and the chosen `number` rather than guessing silently.
- With no open PR but a merged or closed one, reviewr shows that PR as historical state (`merged`/`closed`); with none at all, the empty state.
- A fork PR reads `checks`, `comments`, and merge state from the **base** repository, where GitHub computes them.
- A detached `HEAD` — no branch to resolve against, e.g. after `gh pr merge --delete-branch` — shows the empty state; reviewr never queries `headRefName:""`, which GitHub reads as unfiltered and would mis-resolve to an unrelated PR.

### Derived state

- `merge` folds GitHub's `mergeable` and `mergeStateStatus` to the blockers worth surfacing: `CONFLICTING`/`DIRTY` → `conflicting`, `BLOCKED` → `blocked`; everything else (`CLEAN`, `BEHIND`, `UNSTABLE`, and still-computing `UNKNOWN`) → `clean`, which the footer shows as nothing.
- `mergeable=UNKNOWN` is GitHub computing lazily — it folds to `clean`, never asserted as a conflict unless `mergeStateStatus` is `DIRTY`.
- `sync` compares local `HEAD` to `head_oid` — equal is `in_sync`, `HEAD` ahead is `unpushed` (count via `git rev-list --count <head_oid>..HEAD`), `head_oid` ahead is `behind`.
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
- Each fetch is two GraphQL calls (resolve the number, then read the detail) run on a worker thread and delivered to the UI when complete, so the `gh` calls never block input or scrolling; only one fetch is in flight at a time (`tui.md`).
- The snapshot derives entirely from GitHub each fetch; reviewr keeps no local PR state.

## Failure semantics

reviewr reads GitHub but never writes it, so every failure degrades to a clear state and the rest of the app (Changes, All files) is unaffected.

- `gh` absent, present but not authenticated, or a non-GitHub remote — each shows its own remediation line naming the command that unblocks it; any other failure (a wrong-account 404, a transient API error) shows a generic retry message, never read as "no PR". The next poll or `r` re-attempts cleanly.
- No open PR shows a directional empty state; the next poll lights the tab up the moment a PR appears, with no manual `r`.
- A rate-limited or unreachable poll freezes on the last good snapshot with a quiet marker; a failed poll never blanks a populated tab.
- Second run: every read is idempotent and side-effect-free, so a retry returns the same snapshot.
- Concurrent runs: two sidebars on one worktree read identical GitHub state — harmless, since neither writes and there is no shared local state.

## Non-goals

- No writes to GitHub — reviewr never posts, resolves a thread, re-runs a check, merges, or routes feedback to the agent.
- No event subscription — the snapshot polls `gh`; reviewr opens no webhook or socket (mirrors `herdr-host.md`'s poll-don't-subscribe).
- No second forge — GitHub via `gh` only; the forge-agnostic core (Changes, All files, the diff viewer) must not import this module.

## Decisions

- `gh` over a REST/GraphQL library — the user's authenticated `gh` is the stable, credential-free interface, matching the `herdr` CLI dependency already in `herdr-host.md`. Rejected: a bundled HTTP client with its own token discovery.
- Resolve the open PR by branch, confirmed by `head_oid` — a merged PR on the branch is history, and a branch-name lookup alone misses renames and forks; the oid confirms the match. Rejected: resolving any PR, or by branch name only.
- Read all three comment surfaces — the two AI reviewers split across review bodies and plain comments, so reading one would miss half the reviews. Rejected: review bodies only.
- Relevance from GitHub, not local rebasing — GitHub computes `is_outdated` against the PR head, so reviewr reads it rather than re-anchoring comments to the worktree. Rejected: local line-rebasing.
- GitHub-only — `gh` is one well-understood forge; a generic forge layer carries token discovery and per-forge API shapes with no current user. Rejected: a forge abstraction up front.

## Open decisions

- None.

## Related specs

- `./tui.md`
- `./herdr-host.md`
- `./overview.md`
