---
Status: Current
Created: 2026-06-27
Last edited: 2026-07-23
---

# forge host

The `PR` tab shows the pull request for the branch you are on: its state, checks, and comments. reviewr reads it through the forge's own CLI and never writes back. Rendering lives in `pr-tab.md`.

## Overview

reviewr finds the branch's PR, then re-reads a snapshot of it on every poll. The tab follows one PR from open through merged, then switches to the branch's next PR. Until a PR exists, the tab is empty.

The remote's hostname picks the forge: GitHub read via `gh`, GitLab via `glab`, Azure DevOps via `az`. Per-forge differences live in `forge-providers.md`. Everything below holds for all three.

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
| `body`                 | string | the PR description as the forge returns it, empty when none                 |
| `state`                | enum   | `open`, `merged`, or `closed`                                               |
| `is_draft`             | bool   | draft flag                                                                  |
| `head_ref`             | string | the PR's head branch name, which may differ from the local branch           |
| `head_is_fork`         | bool   | the head lives in another repository                                        |
| `base_ref`             | string | the merge target                                                            |
| `merge`                | enum   | `clean`, `conflicting`, or `blocked`                                        |
| `sync`                 | enum   | `in_sync`, `unpushed`, `behind`, or `unknown`, with a count when known      |
| `checks`               | list   | one row per latest check: `name` and `status` (conclusion folded in)        |
| `comments`             | list   | one row per comment, newest first                                           |
| `truncated`            | bool   | a capped surface had a further page, so a list is a prefix                  |

A `comments` row:

| field                        | type         | meaning                                                                  |
| ---------------------------- | ------------ | ------------------------------------------------------------------------ |
| `kind`                       | enum         | `review` (a review's body), `comment` (conversation), `finding` (inline) |
| `author`, `author_is_bot`    | string, bool | the `@login` and whether it is a bot                                     |
| `anchor`                     | string       | `path:line` for a `finding`, the literal kind word otherwise             |
| `body`, `snippet`            | string       | the text as the forge returns it, only a `finding` carries a snippet     |
| `created_at`                 | time         | post time, the newest-first sort key                                     |
| `is_resolved`, `is_outdated` | bool         | thread state for a `finding`, always false otherwise                     |
| `reply_count`                | int          | replies on a `finding`'s thread beyond the root                          |

## Behavior

### Forge hosts

Each forge knows its public hosts. One config key per forge adds one self-hosted hostname (`config.md`). A key adds a host, never removes the built-in ones. Matching is case-insensitive.

| forge        | built-in hosts                            | self-hosted key     |
| ------------ | ----------------------------------------- | ------------------- |
| GitHub       | `github.com`                              | `github_host`       |
| GitLab       | `gitlab.com`                              | `gitlab_host`       |
| Azure DevOps | `dev.azure.com`, `*.visualstudio.com`     | `azure_devops_host` |

A remote counts when its hostname matches a forge host and its path is a repository on that forge. `upstream` wins over `origin`, so a fork clone reads the base repository's PRs with no setup:

| remote state                                                        | outcome                                                                     |
| ------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| `upstream` names a recognized forge host with a repository identity | reviewr reads that repository on that forge                                 |
| `upstream` is absent, hostless, unsupported, or malformed           | `origin` determines the repository                                          |
| reading `upstream` fails                                            | reviewr shows the retryable Git error and never falls through               |
| `origin` names a recognized forge host with a repository identity   | reviewr reads that repository on that forge                                 |
| `origin` names another hosted repository                            | reviewr names the unsupported host and points self-hosted users to the keys |
| `origin` is missing or hostless                                     | reviewr says the PR tab needs a recognized forge `upstream` or `origin`     |
| `origin` names a recognized host without a repository identity      | reviewr says the forge origin is malformed                                  |
| reading `origin` fails                                              | reviewr shows the retryable Git error                                       |

The repository target is the forge, the hostname, and the repository path together. reviewr reads each remote's fetch URL after Git's `insteadOf` rewrite. Push URLs, SSH aliases, and CLI variables like `GH_HOST` never pick a repository. Azure DevOps' ssh hosts count as their https equivalents. SSH remotes work in `git@host:path` and `ssh://` forms, web remotes in `http(s)://` and `git://`. Other schemes are not repositories.

### Resolution

The tab shows the newest PR opened from the current branch. No branch, or no PR, means the empty state.

A PR can live under a different branch name than the local one, so reviewr searches by up to three kinds of name:

- the branch's own name,
- the branch it tracks, unless that name is a resolved or recorded base (tracking `main` is not publishing to it, and a pick that fails to resolve still names a base),
- any `origin` branch pointing at this branch's work, so `git push origin HEAD:other-name` still finds the PR. A branch pointing at base history carries no work and does not count.

Among the PRs found under those names, in the resolved repository only:

- The newest open PR wins. Open PRs match by name alone, so a branch sharing a busy branch's name adopts its open PR.
- With none open, the newest merged or closed PR wins — but only if this branch contains its head commit. A reused branch name never resurrects an old PR, and a teammate's PR from a different branch never attaches, even when it builds on this branch's commits.
- With neither, the tab is empty.

Details:

- Each fetch pins `HEAD` and the base to fixed commits up front. The agent committing mid-fetch cannot skew it.
- On a fork, reviewr also asks about the fork's own PRs where the forge's query reaches them. A PR into upstream outranks the fork's own, and in upstream only fork-sourced PRs count.
- A detached `HEAD` (mid-rebase, say) has no branch and shows the empty state, but never wipes a snapshot already on screen.
- Finding a pushed name depends on local records. A pruned `origin/*` ref or a missing tracking record can hide one.

### Derived state

- `merge` surfaces only blockers: a conflict is `conflicting`, a rule or policy block is `blocked`, everything else is `clean` — including a forge still computing. The footer shows `clean` as nothing.
- `sync` compares the pinned `HEAD` to the PR's head commit: equal is `in_sync`, `HEAD` ahead is `unpushed` with a count, PR head ahead is `behind`. A PR head missing locally is `unknown`, never a guessed `in_sync`.
- `unpushed` means the checks and comments on screen describe an older commit than the local tree.

### Checks

- One row per check name, latest run only. A passed re-run replaces the earlier failure.
- A top-level rollup gives the overall pass or fail.

### Comments

- Reviews, inline threads, and conversation comments merge into one newest-first list.
- A bot's PR-level posts collapse to its latest. A human's all stay.
- `is_resolved` and `is_outdated` come from the forge, never recomputed locally. Resolved and outdated threads stay listed, marked.
- Each surface reads its newest 100 rows, never paged to exhaustion. A further page sets `truncated` and `pr-tab.md` marks the capped list. A forge that cannot identify its newest page serves the oldest, marked truncated.

### Refresh

- The first fetch starts when the panel opens.
- A refetch fires on entering the tab, on the `refresh` binding (default `r`), and when the worktree ends a turn (`herdr-host.md`, HH-TURN-PER-WORKTREE) — an agent may have pushed or merged with no local trace. On the tab, a fallback poll refetches every 60 seconds. Off the tab, no polling.
- One fetch runs at a time. `refresh` cancels it and restarts. Any other trigger lets it finish and paint, then runs one fresh fetch.
- A result shows only if everything it was fetched for — config, repository target, branch, pinned commits, branch names — still matches. Otherwise reviewr discards it and fetches again, on or off the tab.
- Commits and pushes are freshness, never identity. They refetch behind the visible snapshot and never blank it: the same PR with newer work is stale, not wrong. The in-flight glyph covers the gap (`tui.md`).
- The tab clears only when the repository target, origin, or checked-out branch changes. Then reviewr cannot prove the snapshot still belongs to this branch (`overview.md` Continuity).
- A fetch that finds no PR keeps the snapshot while the pinned `HEAD` still is or contains the shown PR's head commit. A pruned remote branch never blanks the tab mid-session; pulling the merged base does.
- Each fetch uses one validated config snapshot for host and base selection (→ CFG-ONE-SNAPSHOT, `config.md`).
- Every fetch re-derives the snapshot in full. There is no cache beyond what is on screen.
- Exiting reviewr stops scheduling and restores the terminal immediately. Nothing paints afterward.

## Failure semantics

reviewr only reads, so every failure degrades to a clear state. `Changes` and `All files` are unaffected.

- A failure on the same input keeps the visible snapshot and shows its remedy. With no snapshot, the remedy fills the tab.

| failure                                 | remedy shown                                          |
| --------------------------------------- | ----------------------------------------------------- |
| missing forge CLI or required extension | that component's install step (`forge-providers.md`)  |
| unauthenticated fetch                   | that CLI's login command (`forge-providers.md`)       |
| any other fetch error                   | the retry error                                       |

- A failure before the repository target resolves replaces any snapshot with the retryable Git error. A Git failure after the same target resolved keeps the snapshot, with the same error.
- An origin that is not, or stops being, a recognized forge replaces any snapshot with the unsupported-host remedy and points to the host keys.
- A host key naming a server that runs a different forge fails as the chosen CLI's fetch error.
- No PR at all shows the calm empty state. The next poll lights the tab up when one appears.
- Two active PR tabs on one worktree converge within one poll interval.

## Non-goals

- No writes to any forge: no posting, resolving, re-running checks, or merging. No routing PR feedback to the agent.
- No transport of its own. The forge's CLI owns hosts, credentials, and TLS.
- No repository selector or cross-repository search.
- No different parent repositories across sibling worktrees from one clone. Use a separate clone.
- No SSH host-alias normalization. An alias-only repository needs a canonical-host remote.
- No discovery of an unrecorded publication name on a non-`origin` remote.
- No detection of forge-side renames or redirects. reviewr trusts the remote identity verbatim.
- No remote-scoping of the tracked branch name. The bare name applies whichever remote it tracks.
- No event subscription. The snapshot polls the CLI, no webhook or socket.
- No server-version compatibility layer for self-hosted schemas.

## Related specs

- [forge-providers](./forge-providers.md)
- [configuration](./config.md)
- [pr-tab](./pr-tab.md)
- [herdr-host](./herdr-host.md)
- [overview](./overview.md)
