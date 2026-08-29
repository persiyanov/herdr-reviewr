# Auto-open when a worktree workspace is born: Plan

Delivers the sibling [`spec.md`](spec.md).

## Problem

`auto_open = true` opens a reviewr pane on `worktree.created`. It does nothing on `worktree.opened`. Creating a checkout auto-opens. Opening that same checkout later does not.

Repro: create, close the workspace, keep the checkout, `herdr worktree open --path <checkout>`. New workspace, no pane. Log: `worktree.open`, `workspace.create`, no `plugin.pane.open`.

`pane.sh auto-open` already reads the opened payload, no-ops an existing pane, honors `auto_open = false`, and skips overlay and zoomed. The opened hook and the workspace-birth gate are missing. `plugin action invoke open` cannot fix it. It always targets the focused workspace.

## Goal

Ship the approved workspace-birth contract through the existing auto-open command, manifest hooks, tests, user docs, and release acceptance.

## Ticket Map

1. Auto-open on worktree workspace birth: classify both lifecycle events through the existing policy and prove the full path. Blocked by: none.

## Ticket 1: Auto-open on worktree workspace birth

**What to build:** A new worktree workspace opens reviewr whether the checkout was created or opened from disk. Opening an already-live workspace remains a no-op.

**Blocked by:** none

**Status:** done

### Acceptance criteria

- [x] `worktree.created` and `worktree.opened` with `already_open = false` use the event's workspace and checkout, preserve the existing placement and config gates, and pass `--no-focus`.
- [x] `worktree.opened` with `already_open = true` exits successfully after config validation and before any herdr command.
- [x] `herdr-plugin.toml` registers `pane.sh auto-open` for both lifecycle events.
- [x] README describes new worktree workspace birth and the single-owner rule. Changelog records the fix. `docs/herdr-api-notes.md` records the raw opened envelope. `AGENTS.md` and `herdr/pane.sh` no longer describe auto-open as created-only.
- [x] The issue reproduction passes on herdr 0.7.5 and the current supported release in an isolated workspace with one auto-open owner.
- [x] The full CI command passes, and every added branch is exercised by a named acceptance test.

### Implementation plan

1. In `tests/pane_actions.rs`, reuse `fake_herdr` and the current event-launch pattern to add the three invariant tests named in the spec. Make the birth test cover both created and opened-false envelopes. Make the opened-true test require an absent fake-herdr log. Parse `herdr-plugin.toml` with the existing `toml` dependency for the hook test. Remove or fold existing auto-open tests only when the new matrix fully subsumes them.
2. In `herdr/pane.sh`, classify an opened-true payload inside the existing event-policy block, after config and placement validation and before workspace context or pane inspection. Keep the created and opened-false paths on the current parser and open flow. Update the command comment without adding a second command or helper layer.
3. In `herdr-plugin.toml`, keep `worktree.created` and add the sibling `worktree.opened` hook with the same command.
4. Update `README.md`, `CHANGELOG.md`, `docs/herdr-api-notes.md`, and `AGENTS.md`. Describe workspace birth, keep-alive exclusion, the one-owner requirement, and the captured v0.7.5 opened payload in their existing sections.
5. Run targeted tests and `just ci`. Then run the approved release acceptance against herdr 0.7.5 and the current supported release in a disposable environment. Exercise the CLI and TUI open-existing paths without invoking plugin actions against the user's focused workspace.

### Tests

- `auto_open_birth_events_follow_shared_policy`: created and opened-false payloads target their own workspace and checkout, open through the configured split or tab policy, and never request focus.
- `auto_open_opened_live_exits_before_herdr_calls`: opened-true exits 0 with empty output and no fake-herdr log, proving the early gate stays constant-cost as pane count grows.
- `manifest_auto_open_hooks_created_and_opened`: the parsed manifest contains exactly the two auto-open lifecycle hooks with the same command.
- Existing config, placement, pane-identity, and existing-pane tests in `tests/pane_actions.rs`: the shared policy remains unchanged.

### Verification

- `cargo test --test pane_actions auto_open_birth_events_follow_shared_policy` → both birth envelopes open in the event target without focus.
- `cargo test --test pane_actions auto_open_opened_live_exits_before_herdr_calls` → the live-workspace event exits before any herdr call.
- `cargo test --test pane_actions manifest_auto_open_hooks_created_and_opened` → both manifest hooks resolve to `pane.sh auto-open`.
- `just ci` → formatting, clippy, all tests, and the release build pass.
- Isolated herdr 0.7.5 and current-release runs → opening a retained checkout creates one reviewr pane in the new target workspace; TUI open-existing behaves the same; reopening the live workspace triggers no reviewr herdr command.
- Disposable Linux runs with the exact linked manifest, shell, and candidate binary → the CLI and TUI open-existing paths open reviewr on workspace birth but not on an already-live reopen.

### Completion evidence

- `cargo test --test pane_actions` → 29 passed.
- `bash -n herdr/pane.sh` and `git diff --check` → passed.
- `just ci` → formatting, clippy, all test suites, and the release build passed.
- Isolated herdr 0.7.5 and 0.8.2 runs, one linked auto-open owner each → `worktree.created` and `worktree.opened` with `already_open = false` launched the candidate reviewr process; `worktree.opened` with `already_open = true` logged a successful hook and left the closed pane absent.
- TUI open-existing picker on herdr 0.7.5 and 0.8.2 → each created one target workspace with one candidate reviewr pane and a clean `worktree.opened` hook log.

## Out of Scope

- Keep-alive, sidebar focus, placement changes, and atomic cross-plugin ensure-open remain out of scope in [`spec.md`](spec.md).

## Replan

- If a captured v0.7.5 or current-release payload contradicts the approved event classification or target fields, reopen the spec before changing the parser.
- If v0.7.5 cannot run without touching the user's active plugin registry or session, move that acceptance run to a disposable OS user or VM. Do not weaken it to schema-only proof.
- 2026-08-29: initial plan.
- 2026-08-29: completed in disposable Linux hosts; no user plugin registry, session, panes, or installed reviewr binary were changed.
