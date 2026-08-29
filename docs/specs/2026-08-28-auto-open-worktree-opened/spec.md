# Auto-open when a worktree workspace is born

Status: Approved
Date: 2026-08-28

Issue: [#82](https://github.com/persiyanov/herdr-reviewr/issues/82)

## Problem

`auto_open = true` opens a reviewr pane on `worktree.created`. It does nothing on `worktree.opened`. Creating a checkout auto-opens. Opening that same checkout later does not.

Repro: create, close the workspace, keep the checkout, `herdr worktree open --path <checkout>`. New workspace, no pane. Log: `worktree.open`, `workspace.create`, no `plugin.pane.open`.

`pane.sh auto-open` already reads the opened payload, no-ops an existing pane, honors `auto_open = false`, and skips overlay and zoomed. The opened hook and the workspace-birth gate are missing. `plugin action invoke open` cannot fix it. It always targets the focused workspace.

## Proposal

Add `[[events]] on = "worktree.opened"` running `pane.sh auto-open`. Keep the created hook.

Classify each event after config validation and the existing `auto_open` and placement gates:

| Event | Workspace birth |
| ----- | --------------- |
| `worktree.created` | Yes |
| `worktree.opened` with `.data.already_open` false | Yes |
| `worktree.opened` with `.data.already_open` true | No. Exit before workspace or pane inspection. |

`already_open` is a required JSON boolean beside `workspace` and `worktree`. True means herdr found a live workspace for that checkout. False means it created one. Whether herdr focuses that workspace remains the caller's choice. The v0.7.5 schema and source contain the subscribable hook and required event fields, so `min_herdr_version` stays `0.7.5`.

Create does not emit opened. Restore emits neither event. The opened emitters are `worktree open` and the TUI open-existing picker.

With `auto_open = true` and placement `split` or `tab`:

- Create, or open a checkout that is not already a live workspace (path, branch, or TUI picker): `plugin pane open` in that workspace with `--no-focus`.
- Open a checkout whose workspace is already live: auto-open exits. No pane, even if the user closed reviewr there. Toggle restores it.
- Sidebar click: no auto-open (`workspace.focused`, unhooked).
- `auto_open = false`, overlay, zoomed, or a reviewr pane already there: no open. Same gates as today.

README and changelog: auto-open is a new worktree workspace, including open of an existing checkout that was not already live. `auto_open = false` opts out of both events. Record the raw opened envelope only in `docs/herdr-api-notes.md`; behavior tests consume its workspace id, checkout path, and birth classification.

One component owns auto-open for a workspace. A layout or session plugin that opens reviewr must set reviewr's `auto_open = false`; otherwise two check-then-open paths can race and create duplicate panes. The known coder-sessions workaround must stop opening reviewr before relying on this hook. Cross-plugin atomic deduplication would require a herdr-level ensure-open operation.

Decisions: birth, not keep-alive. One command, two hooks. Created stays (create does not emit opened). Skip-when-true is the whole new gate.

## Invariants

False if the named test is red.

| code | Always true | Enforcement |
| ---- | ----------- | ----------- |
| AO-BIRTH | Created and opened-false payloads follow the same existing policy. With `auto_open` true, split or tab placement, and no reviewr pane, open in the event's workspace and checkout with `--no-focus`. | `auto_open_birth_events_follow_shared_policy` |
| AO-OPENED-LIVE | With valid config, an opened-true payload exits 0 before any herdr command. | `auto_open_opened_live_exits_before_herdr_calls` |
| AO-HOOKS | Manifest hooks both `worktree.created` and `worktree.opened` to `pane.sh auto-open`. | `manifest_auto_open_hooks_created_and_opened` |

Release acceptance runs against herdr 0.7.5 and the current supported release in an isolated workspace with one auto-open owner. Close a workspace while retaining its checkout, open it by path, and verify one reviewr pane appears in the target workspace without taking pane focus. Repeat through the TUI picker. Open the already-live checkout again and verify no herdr command runs from reviewr's hook.

## Alternatives

- No `already_open` gate. Live `worktree open` resurrects a closed pane.
- Create only. Reporter loop stays broken.
- Second config key. One gesture, two policies.
- Replace both hooks with `workspace.created` plus a worktree gate. It routes every ordinary workspace birth through reviewr and replaces the established worktree-specific creation contract for no user-visible gain.
- Drop the created hook. New checkouts stop auto-opening.
- Allow concurrent auto-open owners. The existing-pane sweep is not atomic, so two owners can create duplicate panes.

## Out of scope

Sidebar focus. Changing overlay, zoomed, `--no-focus`, or the existing-pane sweep. Atomic cross-plugin ensure-open. Bumping `min_herdr_version`. `plugin action invoke`. Session restore.

## Open questions

None.
