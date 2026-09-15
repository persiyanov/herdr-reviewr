# Bitbucket Server/Data Center pull request support: Plan

Delivers the sibling `spec.md`.

## Goal

Add Bitbucket Server/Data Center PR inspection to `reviewr` with remote detection, PR lookup, activities/comments mapping, and build status checks.

## Ticket Map

1. **Remote identity & config parsing.** Support `bitbucket_host` in `config.toml` and parse Bitbucket Server SSH/HTTP remotes in `src/git.rs`.
2. **REST API client & forge mapping.** Implement `src/bitbucket.rs` handling PR discovery, activities, comments, and commit build status.
3. **UI integration & live testing.** Register `ProviderKind::Bitbucket` in UI badges and add live integration tests in `tests/pr_live.rs`.

## 1. Remote identity & config parsing
**Status:** done

## 2. REST API client & forge mapping
**Status:** done

## 3. UI integration & live testing
**Status:** done

## Verification
- `just fmt-check` green
- `just lint` green
- Unit tests green
