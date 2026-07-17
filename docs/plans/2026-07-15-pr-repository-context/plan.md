# Automatic fork PR resolution — Plan

Delivers [forge host](../../../specs/forge-host.md#github-hosts),
[configuration](../../../specs/config.md), and the [PR tab](../../../specs/tui.md#pr-tab) for PR #18.

## Goal

Resolve a standard fork's pull request from its base repository without setup. Preserve the ordinary
`origin` workflow and the existing one-row PR interface while removing the speculative repository-
selection product from the current diff.

## Definition of Done

- [x] The standard fork fixture from `forge-host.md` resolves its base-repository PR with no environment override or GitHub CLI default.
- [x] An absent, hostless, unsupported, or malformed `upstream` preserves the `origin` workflow.
- [x] A readable supported `upstream` is authoritative. A Git read failure never silently falls through.
- [x] A remote or config change during a fetch never paints a result from the previous target.
- [x] Exact GitHub.com and configured Enterprise hosts work. SSH host aliases are not inferred.
- [x] The PR tab retains the one-row header, clickable `status #number ↗` chip, and existing body height.
- [x] Ordinary and detached no-PR states use the approved short copy.
- [x] No `GH_REPO`, `gh repo set-default`, repository-source state, repository masthead, or masthead-only dependency remains.
- [x] README, changelog, and Current specs describe the shipped behavior.

## Out of Scope

- Product-wide copy changes. `overview.md#Voice` remains a separate Draft contract.
- User-configurable repository selection, cross-repository search, and different parents across sibling worktrees.
- A second forge, GitHub writes, release publication, or package-version change.

## Execution Plan

1. [x] **Lock the regression first.** In `tests/pr_candidates.rs`, add a real Git fixture with `origin` pointed at a fork and `upstream` pointed at its base. Prove target precedence, `origin` fallback, exact Enterprise matching, alias rejection, and remote-read failure without `GH_REPO` or CLI-default setup.
2. [x] **Collapse repository resolution to the approved contract.** In `src/git.rs`, resolve one canonical target from supported `upstream` or `origin`. Retain exact-host parsing and target equality. In `src/config.rs`, remove launch-time `GH_REPO` capture while keeping literal Enterprise-host validation. In `src/forge.rs`, delete CLI-default probing, repository-source states, and selected-repository data that no remaining consumer uses.
3. [x] **Preserve only target-level concurrency correctness.** In `src/lib.rs`, `src/app.rs`, and `src/forge.rs`, keep serialized cancellable PR work, coalesced refreshes, and complete-input stale-result rejection. Make pre-target Git failures replace the PR view. Remove coordinator branches that exist only for mutable selector sources or repository masthead state. Cover remote/config changes, overlapping triggers, off-tab deferral, and exit in `tests/app_flow.rs` and focused library tests.
4. [x] **Restore the original PR geometry.** In `src/ui.rs` and `src/lib.rs`, return every tab to one header row and restore the shipped PR title, branch, status chip, click target, viewport, and mouse routing. Keep the approved no-PR copy. Remove repository identity, fork-owner display, two-row layout tests, and the masthead-only direct grapheme dependency while leaving transitive lockfile users untouched. Restore narrow, realistic-width, and minimum-height coverage in `tests/render.rs` and `tests/app_flow.rs`.
5. [x] **Close the product surface.** Document automatic `upstream` resolution and exact-host alias removal in `README.md` and `CHANGELOG.md`; record the user-visible empty-state copy change in the changelog. Keep unchanged one-row geometry in the TUI contract, remove selector and repository-masthead guidance, verify against `specs/config.md`, `specs/forge-host.md`, and `specs/tui.md`, then promote those three specs to Current.

## Likely Files

| file                              | change                                                        |
| --------------------------------- | ------------------------------------------------------------- |
| `src/git.rs`                      | upstream-first target resolution and exact-host parsing       |
| `src/forge.rs`                    | target-bound fetch without selector or repository view state  |
| `src/lib.rs`                      | target-level refresh convergence and one-row geometry          |
| `src/app.rs`                      | PR state without pending repository exposure                  |
| `src/ui.rs`                       | original PR header and approved empty-state copy               |
| `src/config.rs`                   | remove `GH_REPO`; retain literal Enterprise-host validation   |
| `tests/pr_candidates.rs`          | exact fork scenario, fallback, host, and failure coverage      |
| `tests/app_flow.rs`               | refresh schedules, stale results, exit, and mouse geometry     |
| `tests/render.rs`                 | one-row header, width, height, and empty-state coverage        |
| `tests/common/mod.rs`             | remove repository-only snapshot fixture data                  |
| `Cargo.toml`, `Cargo.lock`        | remove the masthead-only grapheme dependency                  |
| `README.md`, `CHANGELOG.md`       | shipped workflow and migration notes                          |
| `specs/config.md`                 | promote exact-host contract                                   |
| `specs/forge-host.md`             | promote repository-resolution contract                        |
| `specs/tui.md`                    | promote PR UI and copy contract                               |

## Verification

- `cargo test --test pr_candidates` → the exact PR #18 fork fixture selects `upstream`; fallback and host cases pass.
- `cargo test --test app_flow pr_` and focused library tests → stale work, coalescing, exit, and deferred refresh behavior pass.
- `cargo test --test render pr_` → the one-row header, chip hit target, body geometry, and empty states pass.
- `cargo test --lib github_host` → exact Enterprise hosts work and aliases remain unsupported.
- `just ci` and `git diff --check` → formatting, Clippy, all tests, release build, and whitespace pass.
- Live local smoke passed → PR #18 resolved through a fork's `upstream`; unsupported `upstream`
  fell back to `origin`; a different supported `upstream` was authoritative; target repair,
  detached HEAD, hard Git failure, and a 71-column header all behaved as specified.
- Local QA handoff → `just install`, verify the linked green-valley plugin, then reopen the pane.
- Tight: every surviving addition is exercised by a Definition of Done line. Delete the rest.
- Gate: run an xhigh full-branch review, fix or explicitly defer every finding, and promote the three shipping specs to Current.

## Replan

- If the exact fork fixture cannot resolve by base repository plus head branch, reopen the repository contract instead of adding search or a selector.
- If selector-specific types are required only by existing implementation structure, reshape the structure instead of preserving the types.
- 2026-07-15: the approved contract restored automatic `upstream` fallback and removed explicit repository selection and the repository masthead.
