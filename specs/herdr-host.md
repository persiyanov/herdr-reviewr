---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-18
---

# herdr host

How reviewr runs inside herdr: the sidebar pane, the actions that manage it, sending comments to the agent, and turn tracking.

## Overview

reviewr ships as a herdr plugin. The manifest (`herdr-plugin.toml`) declares:

| entry   | name                      | does                                                    |
| ------- | ------------------------- | ------------------------------------------------------- |
| pane    | `sidebar`                 | runs the reviewr binary                                  |
| actions | `toggle`, `open`, `close` | manage the sidebar pane                                  |
| event   | `worktree.created`        | auto-opens the sidebar (off with `auto_open = false`)    |

herdr owns the pane. The binary runs inside it. The actions and event follow the same placement contract.

The pane never shows herdr's blank grid. The binary paints its empty frame before the first git scan. A failing scan shows the error in the status line. A genuinely hung `git` leaves a frozen-but-visible sidebar. Neither is a blank pane.

## Sidebar actions

Users bind actions to keys with `[[keys.command]] type = "plugin_action"`. Scripts invoke them directly:

```
herdr plugin action invoke open --plugin persiyanov.reviewr
```

What each action does:

| action   | sidebar absent | sidebar present |
| -------- | -------------- | --------------- |
| `open`   | opens one      | does nothing    |
| `close`  | does nothing   | closes them all |
| `toggle` | opens one      | closes them all |

With valid plugin config, the shared rules are:

| question                 | answer                                                                      |
| ------------------------ | ---------------------------------------------------------------------------- |
| run it twice?            | it converges and exits 0, nothing stacks and nothing errors                   |
| does `auto_open` gate?   | no, event-only rules never apply, any placement opens (→ HH-PLACEMENT-CONFIGURED) |
| focus?                   | same rules as the toggle (→ HH-FOCUS-BY-PLACEMENT)                            |
| on refusal, on success?  | exit 1 with one stderr line, exit 0 with one stdout line naming the pane      |
| what counts as open?     | any pane labeled `reviewr` in the workspace, in any tab                       |
| which workspace?         | the focused one, wherever the action is invoked from                          |
| what does `close` sweep? | every labeled pane, even one herdr's plugin registry forgot after a restart   |

Every action validates plugin config before inspecting the workspace (config.md). An action also refuses when there is no workspace context or an open is outside a git repo. Both outcomes land in `herdr plugin log list` with the same exit and line discipline.

## Sidebar placement

The placement settings come from `$HERDR_PLUGIN_CONFIG_DIR/config.toml`. Each action and event reads one config snapshot.

```toml
toggle_placement = "overlay"   # split | overlay | zoomed | tab   (default: split)
toggle_direction = "down"      # right | down, split only         (default: right)
auto_open = false              # auto-open on worktree.created    (default: true)
```

The cross-action invariants, coded for citation:

| code                      | Always true                                                                              |
| ------------------------- | ----------------------------------------------------------------------------------------- |
| `HH-PLACEMENT-CONFIGURED` | Every open uses the placement named by `toggle_placement`.                                 |
| `HH-EVENT-SPLIT-TAB`      | The event auto-opens only `split` and `tab`.                                               |
| `HH-FOCUS-BY-PLACEMENT`   | A manual open keeps focus on the agent for `split`. It gives focus to reviewr otherwise.   |
| `HH-ONE-SIDEBAR`          | At most one sidebar exists per workspace, in steady state.                                 |
| `HH-EVENT-OFF`            | With `auto_open = false` the event does nothing.                                           |

A missing key uses its default. Invalid plugin config follows `config.md`. `toggle_direction` affects `split` only. The event never takes focus.

Each placement maps to one pane-open shape (`../docs/herdr-api-notes.md`):

| placement | selector        | direction         | covers the pane |
| --------- | --------------- | ----------------- | --------------- |
| `split`   | `--target-pane` | `right` or `down` | no              |
| `tab`     | `--workspace`   | none              | no              |
| `overlay` | active pane     | none              | yes             |
| `zoomed`  | `--target-pane` | none              | yes             |

A `split` or `zoomed` open attaches to the focused pane. When the context has none, it attaches to the workspace's first pane.

**Placement changed between open and close**

1. `toggle_placement = split`. The user toggles. A right split opens.
2. The user sets `toggle_placement = overlay`.
3. The user toggles. The script finds the labeled pane and closes it (→ HH-ONE-SIDEBAR).
4. The user toggles. An overlay opens (→ HH-PLACEMENT-CONFIGURED).

**Event with a covering placement**

1. `toggle_placement = zoomed`. A worktree is created.
2. The event fires. Zoomed is not an auto-open placement. Nothing opens (→ HH-EVENT-SPLIT-TAB).
3. The user toggles later. A zoomed pane opens and takes focus (→ HH-FOCUS-BY-PLACEMENT).

**HH-EVENT-BESIDE-LAYOUT — event beside a layout plugin**

1. `auto_open = false`. A layout plugin also handles `worktree.created`.
2. A worktree is created. herdr runs both handlers in any order.
3. reviewr opens nothing either way (→ HH-EVENT-OFF). The layout builds undisturbed.
4. The user toggles later. reviewr opens over the finished layout (→ HH-PLACEMENT-CONFIGURED).

**A layout plugin opens reviewr explicitly**

1. `auto_open = false`. The layout builds its tabs on the event (→ HH-EVENT-BESIDE-LAYOUT).
2. The layout invokes `open` while the new workspace has focus.
3. No labeled pane exists. The configured placement opens.
4. The layout re-runs `open`. The labeled pane exists. Nothing happens.
5. The user presses the toggle key. The sidebar closes (→ HH-ONE-SIDEBAR).

## Repo discovery

The binary reviews the pane's working directory, normalized to its git top level. A directory outside any repo shows an empty state.

## Sending to the agent

`Send` hands over every written comment at once. The target is resolved in order:

1. the sole agent in the sidebar's tab,
2. else the sole agent in its workspace.

reviewr writes the comment blocks into the agent pane without submitting, then focuses it. You add context and press enter.

The candidate rules, coded for citation:

| code                       | Always true                                                            |
| -------------------------- | ---------------------------------------------------------------------- |
| `HH-AGENT-PANES`           | Only panes carrying an `agent` field are candidates.                    |
| `HH-NOT-SELF`              | The sidebar's own pane is never a candidate.                            |
| `HH-TAB-WINS`              | A sole tab agent wins over the workspace fallback.                      |
| `HH-SOLE-OR-REFUSE`        | Zero or several candidates refuse the send. Nothing is sent partially.  |
| `HH-REFUSE-SAYS-CLIPBOARD` | A refusal says why and points at the clipboard copy.                    |

With `tab` placement the sidebar has its own tab, so resolution goes straight to the workspace fallback.

## Clipboard

The export copies through the OS clipboard utility on the machine where the binary runs.

## Turn tracking

The `last-turn` scope (`review-model.md`) needs to know when a turn starts. reviewr polls the agent's status on every worktree refresh. A turn starts when the status moves from resting (`idle` or `done`) to `working`. Moves from `blocked` or `unknown` to `working` do not start a turn.

On a turn start, reviewr snapshots the worktree as a candidate baseline. The candidate becomes the live baseline on the first poll where that turn changed a file. A turn that changes nothing never moves the baseline. The live baseline is the old side of every `last-turn` diff until the next change-producing turn replaces it.

The snapshot never touches the index, the worktree, or any branch. It respects `.gitignore`, so `last-turn` never shows ignored paths. The baseline lives in a private ref under `refs/reviewr/turn-base/`, keyed by worktree path and outside `refs/heads`, so it never appears in a branch list. The ref persists, so reopening the sidebar resumes the same baseline.

## Failure semantics

Actions:

- Two concurrent opens can both open a pane. The next action heals it: `open` no-ops and `close` sweeps both.
- Actions act on the state they observe. A `close` racing an in-flight open exits 0, and the open still lands.
- A crash after the pane opens loses nothing. The label survives, so the next action finds the pane.
- A scripted `open` lands in the focused workspace. A user who switches focus first redirects it. herdr offers no workspace selector on invoke.
- Any pane labeled `reviewr` counts as the sidebar and is swept by the next close.
- An open never opens into the pane that invoked it. A layout pane whose command is the invoke exits when the invoke finishes.
- After a close, focus falls wherever herdr leaves it.

Send and tracking:

- Browsing and the clipboard export work without the herdr CLI. Sending and turn tracking need it. Without it, `last-turn` stays empty and `uncommitted` and `branch` are unaffected.
- Turn tracking resolves the agent under the same candidate rules (`HH-AGENT-PANES`, `HH-NOT-SELF`, `HH-TAB-WINS`), so a plugin sidebar or shell in the tab never pauses tracking.
- A failed clipboard utility or `herdr agent send` reports the error. The comments stay in the list.
- A turn shorter than one poll interval, or one whose start is masked by a transient `unknown` status, is missed. `last-turn` then shows the changes since the last observed turn start. It never shows lines the agent did not write.
- A crash mid-snapshot costs at most one failed refresh. Ref updates are atomic. Leftover locks are cleared before the next snapshot and on every exit path.
- Two sidebars on one worktree write the same baseline ref. Each samples on its own clock, so their snapshots of one turn start can differ by the edits between them. Last-writer-wins keeps the baseline within one poll interval of the turn start.

## Non-goals

- No clipboard over SSH. The export targets the local machine.
- No herdr socket subscription. Turn tracking polls.
- No embedding in a caller's pane. The sidebar is always the plugin's own pane.

## Related specs

- [configuration](./config.md)
- [overview](./overview.md)
- [review-model](./review-model.md)
- [theme](./theme.md)
