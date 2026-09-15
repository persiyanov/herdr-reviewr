# Bitbucket Server/Data Center pull request support

Status: Draft
Date: 2026-09-15

## Problem

`reviewr` supports GitHub, GitLab, and Azure DevOps pull requests, but repositories hosted on self-hosted Atlassian Bitbucket Server and Data Center (formerly Stash) show `no pull request found for <branch>` or fail remote detection.

Bitbucket Server and Data Center differ fundamentally from Bitbucket Cloud:
- Self-hosted enterprise domains (not `bitbucket.org`).
- Dedicated REST API path `/rest/api/1.0/projects/{project}/repos/{repo}/pull-requests`.
- Separate commit build status API at `/rest/build-status/1.0/commits/{commitId}`.
- Distinct SSH URL format: `ssh://git@host:7999/{project}/{repo}.git` or `ssh://git@host/{project}/{repo}.git`.
- Distinct HTTP URL formats: `/scm/{project}/{repo}.git` or web browse URL `/projects/{project}/repos/{repo}`.
- Web UI subdomains often differ from SSH endpoints (e.g., `stash-ui.host` vs `stash.host`).

## Proposal

Add Bitbucket Server/Data Center as a supported forge alongside GitHub, GitLab, and Azure DevOps.

### Configuration
Add optional `bitbucket_host` to `config.toml`:
```toml
bitbucket_host = "stash.example.com"
```
When configured, any remote whose host matches `bitbucket_host` (or its `stash-ui.` / `stash.` / `bitbucket-ssh.` alias) is parsed as `ProviderKind::Bitbucket`.

### Authentication
Authenticate API calls using the `BITBUCKET_TOKEN` environment variable passed via HTTP `Authorization: Bearer <token>`. When unset, surface an actionable error banner directing the user to export `BITBUCKET_TOKEN`.

### REST API Integration
- Pull request discovery: `GET /rest/api/1.0/projects/{project}/repos/{repo}/pull-requests?direction=OUTGOING&at=refs/heads/{branch}&state=ALL`
- Pull request details: `GET /rest/api/1.0/projects/{project}/repos/{repo}/pull-requests/{id}`
- Activity & inline comments: `GET /rest/api/1.0/projects/{project}/repos/{repo}/pull-requests/{id}/activities`
  - Inline code comments anchored to diff line and file path.
  - General review comments and approval activity.
- Build status checks: `GET /rest/build-status/1.0/commits/{commitId}` mapping `SUCCESSFUL`, `FAILED`, `INPROGRESS` to standard check statuses.

### Decisions
- **REST v1.0, not Cloud 2.0 API.** Server/DC uses `/rest/api/1.0/`, not the Bitbucket Cloud `/2.0/repositories/` schema.
- **Configured host gating.** Like `gitlab_host` and `azure_devops_host`, Bitbucket Server uses `bitbucket_host` in `config.toml` to disambiguate enterprise private domains from arbitrary git hosts.
- **SSH port and path parsing.** Parse both standard `7999` SSH ports and HTTPS `/scm/` path prefixes cleanly to Project and Repository components.
- **Generic token environment.** Use standard `BITBUCKET_TOKEN` for bearer authentication without proprietary vendor fallbacks.

## Invariants

| code | Always true | Enforcement |
| ---- | ----------- | ----------- |
| BB-REMOTE-PARSE | Valid Bitbucket Server SSH and HTTPS URLs parse to `ProviderKind::Bitbucket` with correct project and repo. | `git::tests::repository_identity_parses_bitbucket_remote_forms` |
| BB-HOST-ALIAS | Configured `bitbucket_host` matching `stash-ui.host` admits `stash.host` SSH remotes. | `git::tests::repository_identity_parses_bitbucket_remote_forms` |
| BB-TOKEN-AUTH | `fetch_live_pr` attaches `Authorization: Bearer` when `BITBUCKET_TOKEN` is set, and errors cleanly when missing. | `tests/pr_live.rs` |
| BB-CHECK-MAP | `SUCCESSFUL`, `FAILED`, `INPROGRESS` from `/rest/build-status/1.0` map to CheckStatus variants. | `src/bitbucket.rs` tests |

## Alternatives

- **Bitbucket Cloud API.** Completely different payload shape, authentication, and endpoint scheme. Kept separate.
- **Host auto-detection via probe.** Slows down initial render on every unknown git host. Explicit `bitbucket_host` aligns with existing GitLab/Azure DevOps architecture.

## Out of scope

- Bitbucket Cloud (`bitbucket.org`).
- Writing/posting new comments or approvals from reviewr (read-only PR inspection).
