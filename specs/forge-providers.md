---
Status: Current
Created: 2026-07-22
Last edited: 2026-07-23
---

# forge providers

What differs per forge behind `forge-host.md`: repository identity, the CLI, and how each forge's concepts fill the one snapshot.

## Overview

| forge        | CLI                                 | noun          | abbreviation | reference |
| ------------ | ----------------------------------- | ------------- | ------------ | --------- |
| GitHub       | `gh`                                | pull request  | `PR`         | `#226`    |
| GitLab       | `glab`                              | merge request | `MR`         | `!42`     |
| Azure DevOps | `az` + the `azure-devops` extension | pull request  | `PR`         | `#12`     |

On screen, only the vocabulary differs: each forge's name, noun, abbreviation, and reference form (`pr-tab.md`). Branch names pass to each CLI verbatim. Each provider owns its whole read and degrades in-band: an unreadable optional surface contributes nothing instead of failing the fetch. A mapping not stated below is the identity.

## GitHub

- Identity: `owner/repository`.
- CLI: `gh`. Login remedy: `gh auth login --hostname <host>`.
- Merge: `CONFLICTING`/`DIRTY` is `conflicting`, `BLOCKED` is `blocked`, everything else is `clean`. `UNKNOWN` is GitHub still computing, so `clean` unless `mergeStateStatus` says `DIRTY`.
- Checks: check runs and commit statuses, one list.
- Comments: reviews are `review` rows, review threads are `finding` rows with GitHub's resolved and outdated flags, conversation comments are `comment` rows.
- Query: PRs by head branch name in the target repository. Open PRs come on their own page beside the finished page, so a long finished history cannot hide one. On a fork, upstream results count only when their head lives in the fork — except a merged or closed PR whose fork was deleted, which the containment check vouches for.

## GitLab

- Identity: the full namespace path, which may nest more than two segments.
- CLI: `glab`. Login remedy: `glab auth login --hostname <host>`.
- State: `opened` is `open`, `merged` is `merged`, `closed` and `locked` are `closed`. A cross-project MR sets `head_is_fork`.
- Merge: a conflict is `conflicting`. Blocking discussions, missing required approvals, or a denied policy are `blocked`. Everything else, including still checking, is `clean`.
- Checks: the head pipeline's jobs, one row each. A job allowed to fail counts as skipped, never failing. A jobs page past the cap adds one `pipeline` row with the pipeline's own verdict. No pipeline, or one the user cannot read, means an empty list.
- Comments: MR notes are `comment` rows, diff discussions are `finding` rows with the resolved flag and no snippet (GitLab sends no code context), approvals are `review` rows. An unreadable approvals surface adds no `review` rows. Service accounts (`project_…_bot…`, `group_…_bot…`) and `[bot]`/`-bot` names count as bots. Past GitLab's ~10,000-row counting ceiling, reviewr serves the oldest page, marked truncated.
- Query: MRs by `source_branch` in the target project. Opened MRs come on their own page beside the all-state page, so a long finished history cannot hide one. On a fork, upstream results count only when their source project is the fork.

## Azure DevOps

- Identity: `organization/project/repository`. Accepted URL forms: `dev.azure.com/{organization}/{project}/_git/{repository}`, `ssh.dev.azure.com:v3/{organization}/{project}/{repository}`, and their legacy `{organization}.visualstudio.com` and `vs-ssh.visualstudio.com:v3` equivalents. A legacy `DefaultCollection` segment drops. A repository named after its project may omit the project segment. Names travel percent-encoded in the URL and are addressed decoded.
- CLI: `az` with the `azure-devops` extension. A missing extension shows its install step. Login remedy: `az login`, or `az devops login` for a personal access token.
- State: `active` is `open`, `completed` is `merged`.
- Merge: conflicts are `conflicting`, a rejected required policy is `blocked`, everything else — including a still-queued merge check — is `clean`.
- Checks: policy evaluations and commit statuses, one list.
- Comments: PR-level threads are `comment` rows, file-position threads are `finding` rows with the thread's resolved status and no snippet, reviewer votes are `review` rows. Azure's service and build-service identities count as bots, as do the shared name suffixes.
- Query: PRs by `sourceRefName` over the newest 100 active and 100 completed, in the target repository only. A fork PR into the target resolves through `forkSource` and counts only when the pinned `HEAD` contains its source tip. Beyond the lookup: a fork's own internal PRs, merged PRs older than the completed window, and abandoned PRs. An abandoned PR never counts as closed history. An unreadable enumeration fails the fetch.

## Non-goals

- No forge beyond these three.
- No per-forge rendering. The `PR` tab renders only the snapshot.

## Related specs

- [forge-host](./forge-host.md)
- [configuration](./config.md)
- [pr-tab](./pr-tab.md)
