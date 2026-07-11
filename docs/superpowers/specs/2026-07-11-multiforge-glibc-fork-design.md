# Multi-forge + glibc-independent fork of herdr-reviewr

**Date:** 2026-07-11
**Status:** Approved
**Fork:** `dcieslak19973/herdr-reviewr` (upstream: `persiyanov/herdr-reviewr`)

## Goal

Make herdr-reviewr usable in a corporate environment that (a) runs glibc 2.34 on Linux
dev hosts and (b) hosts code on self-hosted GitLab and Bitbucket Data Center rather than
GitHub. Two deliverables:

1. **glibc independence** — Linux release binaries that run on any glibc, via musl
   static builds.
2. **Full PR/MR tab parity on three forges** — GitHub (kept, since distribution stays
   on GitHub Releases), self-hosted GitLab, and Bitbucket Data Center.

The core review loop (diff view, line comments, send-to-agent, file browser) is already
forge-agnostic and is not touched.

## Constraints and context

- The binary does **no networking itself**: all GitHub access shells out to `gh`. The
  crate has zero C or TLS dependencies (syntect uses `fancy-regex`, not oniguruma).
  This design preserves that property — it is what makes musl static builds trivial and
  keeps corporate proxy/CA concerns out of the binary.
- Upstream `specs/forge-host.md` declares "No second forge" as a non-goal. This fork
  reverses that; the spec is updated accordingly.
- The fork should stay rebasable on upstream: the GitHub code path moves but does not
  change behavior.

## Architecture: forge abstraction

`src/forge.rs` becomes `src/forge/`:

- **`forge/mod.rs`** — the forge-neutral core, moved unchanged: `PrSnapshot`, `PrView`,
  candidate-branch derivation, `HEAD`/base pinning, refresh/poll semantics, the fetch
  worker thread, snapshot-preservation rules.
- **`forge/github.rs`**, **`forge/gitlab.rs`**, **`forge/bitbucket.rs`** — one backend
  each, implementing a single trait: given a `PrFetchInput` (canonical host, owner/repo
  or project-key/slug, pinned OIDs, candidate branches), produce a resolved snapshot or
  a typed error. The core owns *when* to fetch and what to do with results; backends
  own only *how to read their forge*.

The snapshot model is already forge-neutral in shape and is unchanged. Concept mapping:

| Snapshot field | GitLab (MR) | Bitbucket DC (PR) |
|---|---|---|
| `number` | MR `iid` | PR `id` |
| `is_draft` | `draft` flag | draft flag (DC 8.x+), else `false` |
| `head_ref` | `source_branch` | `fromRef.displayId` |
| `head_is_fork` | cross-project MR (`source_project_id != target_project_id`) | `fromRef.repository != toRef.repository` |
| `merge` | `detailed_merge_status` → conflicting / blocked / clean | `/merge` endpoint conflicts/vetoes → conflicting / blocked / clean |
| `checks` | head pipeline's jobs | build-status API on the head commit |
| `comments` | discussions: positioned notes → `finding` (with resolved state), other user notes → `comment`, system notes dropped | `/activities`: inline comments → `finding` (anchor from comment anchor), general comments → `comment` |

UI: the tab label is forge-aware ("PR" for GitHub/Bitbucket, "MR" for GitLab). Degraded
`PrView` states are parameterized per forge: `NoCli` names the missing tool and its
install remedy, `NotAuthed` gives the forge's login command, `Error` names the forge.
Failure semantics keep the upstream contract: every degradation names its remedy and
preserves the last same-input snapshot.

## Backends

### GitHub — `gh` (unchanged)

The existing GraphQL code moves into `forge/github.rs` behaviorally unchanged.

### GitLab — `glab`

Shells out to `glab api` (REST). Auth, custom hostnames, and proxy/CA handling are
glab's job, mirroring how `gh` is used today. Open-MR resolution: GitLab cannot OR
source branches in one call, so it is one
`projects/:id/merge_requests?source_branch=<name>&state=opened` call per candidate
branch — capped at 8 by the existing derivation, acceptable at the 60 s poll interval.
Detail, discussions, and the head pipeline's jobs are fetched for the resolved MR.

### Bitbucket Data Center — `curl`

No official CLI exists, so the backend shells out to `curl` against
`/rest/api/latest/projects/{key}/repos/{slug}/…`:

- Resolution: `pull-requests?at=refs/heads/<branch>&state=OPEN&direction=OUTGOING`
  per candidate.
- Merge state: the PR `/merge` endpoint (conflicts and vetoes).
- Checks: the build-status API for the head commit.
- Comments: `/activities`, split into inline (`finding`) and general (`comment`).

**Auth:** an HTTP access token, resolved in order: `BITBUCKET_TOKEN` env var, then
`git credential fill` for the origin host (reusing git's existing credentials). The
token is passed to curl via a config file on **stdin** (`curl -K -`), never on the
command line, so it is not visible in `/proc` or `ps`.

## Config and origin classification

Two new keys following the existing `github_host` contract (case-insensitive host
match, SSH-alias rules, one value each):

```toml
github_host    = "github.example.com"   # existing behavior, unchanged
gitlab_host    = "gitlab.corp.com"      # new; gitlab.com is always supported
bitbucket_host = "bitbucket.corp.com"   # new; no default — bitbucket.org is Cloud
                                        # (different API) and degrades with a clear message
```

`classify_remote` in `git.rs` extends from "GitHub or unsupported" to a host → forge
mapping. A host matching no forge degrades exactly as today, naming the host and the
config keys that would enable it.

Path parsing is forge-aware: GitHub and GitLab origins carry `owner/repo` (GitLab may
nest groups: `group/subgroup/repo`, URL-encoded when passed to `glab api`). Bitbucket
DC HTTP(S) clone URLs carry an `/scm/` prefix (`https://host/scm/PROJ/repo.git`) and
SSH ones do not (`ssh://git@host/PROJ/repo.git`); both parse to a project key + repo
slug, and a Bitbucket-classified origin that fits neither form is malformed.

## Build, distribution, identity

- **musl static builds:** the Linux targets in `release.yml` change from `-gnu` to
  `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (aarch64 via `cross`, as
  today). `herdr/install.sh` maps Linux platforms to the musl triples. CI asserts the
  Linux binaries are static (`ldd` reports "not a dynamic executable") so the
  any-glibc guarantee is enforced, not assumed. macOS targets unchanged.
- **Distribution:** GitHub Releases on the fork; `install.sh`'s `REPO` becomes
  `dcieslak19973/herdr-reviewr`.
- **Plugin identity:** the plugin id changes to **`dcieslak19973.reviewr`**
  (manifest `id`, action ids `dcieslak19973.reviewr.toggle/open/close`, README
  examples).

## Testing

- Backend mapping functions are pure and tested against canned JSON fixtures (real
  GitLab MR and Bitbucket DC API response shapes) — the same pattern the existing
  `gh` parsing tests use.
- Host classification gets table tests alongside the existing ones in `git.rs`.
- The existing GitHub test suite must pass unchanged after the module move.
- Subprocess invocation stays a thin untested edge, as upstream does.

## Spec updates

- `specs/forge-host.md`: rewritten as the multi-forge contract (resolution, mapping
  table, per-forge failure remedies); the "No second forge" non-goal removed.
- `specs/config.md`: `gitlab_host` / `bitbucket_host` value contracts.

## Non-goals

- No writes to any forge (upstream invariant kept).
- No Bitbucket Cloud (bitbucket.org) support — degrades with a clear message.
- No native HTTP client in the binary; networking stays in subprocesses.
- No changes to the core review loop, themes, or herdr integration.
