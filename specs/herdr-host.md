---
Status: Current
Created: 2026-06-23
Last edited: 2026-07-27
---

# herdr host

How reviewr runs inside herdr: the sidebar pane, the actions that manage it, sending comments to an agent, and turn tracking.

## Overview

reviewr ships as a herdr plugin (`herdr-plugin.toml`). herdr owns the pane, and the binary runs inside it.

The binary paints its empty frame before the first git scan, so the pane never shows herdr's blank grid. A failing scan shows the error in the status line. A hung `git` leaves a frozen but visible sidebar.

## Invariants

| code                      | Always true                                                |
| ------------------------- | ---------------------------------------------------------- |
| `HH-PLACEMENT-CONFIGURED` | Every open uses the placement named by `toggle_placement`. |
| `HH-ONE-SIDEBAR`          | At most one sidebar exists per workspace, in steady state. |
| `HH-TURN-PER-WORKTREE`    | A turn belongs to the worktree, never to one agent.        |

## Sidebar actions

Actions bind to keys and to script invocations alike.

| action   | sidebar absent | sidebar present |
| -------- | -------------- | --------------- |
| `open`   | opens one      | does nothing    |
| `close`  | does nothing   | closes them all |
| `toggle` | opens one      | closes them all |

With valid plugin config:

| question                | answer                                                                   |
| ----------------------- | ------------------------------------------------------------------------ |
| run it twice?           | it converges and exits 0, nothing stacks and nothing errors              |
| does `auto_open` gate?  | no, any placement opens (→ HH-PLACEMENT-CONFIGURED)                      |
| focus?                  | same rules as the toggle                                                 |
| on refusal, on success? | exit 1 with one stderr line, exit 0 with one stdout line naming the pane |
| what counts as open?    | any pane labeled `reviewr` in the workspace, in any tab                  |
| which workspace?        | the focused one, wherever the action is invoked from                     |

Every action validates plugin config before inspecting the workspace (`config.md`). An action refuses without workspace context, and an open refuses outside a git repository. Both land in `herdr plugin log list`.

## Sidebar placement

Placement settings come from `$HERDR_PLUGIN_CONFIG_DIR/config.toml`. Each action and event reads one snapshot.

```toml
toggle_placement = "overlay"   # split | overlay | zoomed | tab   (default: split)
toggle_direction = "down"      # right | down, split only         (default: right)
auto_open = false              # auto-open on worktree.created    (default: true)
```

A manual open keeps focus on the agent for `split`, and gives focus to reviewr otherwise. The event auto-opens `split` and `tab` only, never takes focus, and does nothing with `auto_open = false`. A missing key uses its default. Invalid plugin config follows `config.md`.

| placement | direction         | covers the pane |
| --------- | ----------------- | --------------- |
| `split`   | `right` or `down` | no              |
| `tab`     | none              | no              |
| `overlay` | none              | yes             |
| `zoomed`  | none              | yes             |

A `split` or `zoomed` open attaches to the focused pane, or to the workspace's first pane when the context has none.

A `tab` open names the fresh tab `reviewr`, using the `tab_id` the pane-open result reports. herdr otherwise labels a new tab with a bare number. The rename is cosmetic. When it fails, or an older herdr omits `tab_id`, the open still succeeds and the tab keeps its numeric label.

**HH-EVENT-BESIDE-LAYOUT: a layout plugin handles the same event**

1. `auto_open = false`. A layout plugin also handles `worktree.created`.
2. A worktree is created. herdr runs both handlers in any order.
3. reviewr opens nothing either way. The layout builds undisturbed.
4. The user toggles later. reviewr opens over the finished layout (→ HH-PLACEMENT-CONFIGURED).

## Repo discovery

The binary reviews the pane's working directory, normalized to its git top level. A directory outside any repository shows an empty state.

## Sending to the agent

`Send` hands every written comment to one agent at once. The send asks rather than resolves.

| herdr reports          | `Send` does                                                             |
| ---------------------- | ----------------------------------------------------------------------- |
| one agent              | writes every comment into its input without submitting, then focuses it |
| several agents         | opens the agent picker                                                  |
| no agent, or no answer | refuses and names the clipboard copy                                    |

A candidate is any pane in the sidebar's herdr workspace carrying an `agent` field, except the sidebar's own. Placement never narrows the set, and no scope inside the workspace wins over another.

A send that does not land says so in one short sentence and keeps every comment. It never shows herdr's own wording.

### Agent picker

Each row leads with the agent's name, with its state and tab trail dim behind. The highlight is a row fill (`tui.md`).

```
┌ Send 3 comments to ─────────────────────────────────────────┐
│ 1  claude        idle · Grip Outreach · last used           │  ← highlighted, filled row
│ 2  release-bot   idle · Grip Outreach Campaign              │
│ 3  codex         working · 3                                │
└─────────────────────────────────────────────────────────────┘
```

Every part comes from herdr, and reviewr synthesizes none of it:

| part  | herdr source                                                          |
| ----- | --------------------------------------------------------------------- |
| name  | the agent's `name`, else its `display_agent`, else its kind           |
| state | the agent's `state_labels` entry for its state, else the state itself |
| tab   | the tab's label                                                       |

The highlight opens on the first of these that is still a candidate:

1. the agent this session sent to last,
2. the agent the sidebar was opened beside,
3. the first row.

Only a successful send sets the first level. The last-sent row carries a dim `last used` tag.

- Rows keep herdr's own order.
- Only the first nine rows carry a number.
- The row set and its order freeze when the picker opens (`overview.md`).

The send addresses the pane on the chosen row. A pane that closed while the picker was open fails the send, and every comment stays. A successful send focuses the chosen agent and names it.

A configuration error closes the picker and drops its frozen rows. Every comment survives, and so does the last-sent agent that arms the next picker (`config.md`).

## Turn tracking

Two agents editing one worktree produce one turn (→ HH-TURN-PER-WORKTREE), so a second agent starting mid-turn never re-baselines the first one's work out of the diff.

An agent is in the worktree when its working directory resolves to the sidebar's git top level. Only a pane carrying an `agent` field counts, and never the sidebar's own. A second worktree of the same repository resolves elsewhere and is not in it. herdr workspaces and tabs never enter this rule, so placement never changes which turns the sidebar sees, and a plain clone tracks like a herdr worktree.

reviewr polls the agents in the worktree on every worktree refresh:

| the worktree | when                                                    |
| ------------ | ------------------------------------------------------- |
| rests        | every agent in it reports `idle` or `done`              |
| works        | at least one agent in it reports `working`              |
| neither      | an agent reports `blocked` or `unknown`, and none works |

A worktree holding no agents rests, so the first agent to arrive and work starts a turn.

A turn starts on the edge from rest to work. It starts only from rest, so a `blocked` permission prompt mid-turn resumes the same turn rather than starting another.

A turn ends when the worktree next rests, however many neither samples sit between. The two edges are deliberately asymmetric: a missed start only widens the diff, which is allowed below, while a missed end would strand the `PR` tab's refetch.

A poll that cannot observe the whole worktree changes nothing, and the last known membership stands. Two failures reach it: herdr is unreachable, or git cannot be run to resolve an agent's directory. Either holds the poll, so a member left unresolved under load never reads as an empty worktree. Before any poll succeeds membership is unobserved, which the empty state reports as waiting rather than as an empty worktree (`tui.md`).

On a turn start, reviewr snapshots the worktree as a candidate baseline. The candidate becomes the live baseline on the first poll where that turn changed a file, so a turn that changes nothing never moves it. The live baseline is the old side of every `last-turn` diff until the next change-producing turn replaces it.

The snapshot never touches the index, the worktree, or any branch, and it respects `.gitignore`. The baseline lives in a private ref under `refs/reviewr/turn-base/`, keyed by worktree path and outside `refs/heads`. The ref persists across sidebar restarts.

## Failure semantics

Actions:

- Two concurrent opens can both open a pane. The next action heals it: `open` no-ops and `close` sweeps both.
- A `close` racing an in-flight open exits 0, and the open still lands.
- A crash after the pane opens loses nothing. The label survives, so the next action finds the pane.
- A `close` sweeps by label, from the live pane list, so it reaches a pane herdr's plugin registry forgot after a restart.
- An open never opens into the pane that invoked it.
- An action acts on the focused workspace. herdr offers no workspace selector on invoke, so a scripted open lands wherever the user is looking, not where the script meant.
- After a close, focus falls wherever herdr leaves it.

Send and tracking:

- Browsing and the clipboard export work without the herdr CLI. Without it `last-turn` stays empty, and `uncommitted` and `branch` are unaffected.
- A failed clipboard utility or `herdr pane send-text` reports the error. The comments stay in the list.
- A turn shorter than one poll interval, or one whose start is masked by a `blocked` or `unknown` report, is missed. `last-turn` then shows the changes since the last observed turn start, which is more than that turn wrote and never less.
- An agent left `blocked` or `unknown` holds the worktree at neither for as long as it stays there, so no other agent in that worktree can start or end a turn meanwhile. Their work accumulates into the open turn's diff rather than being lost. Answering the prompt releases it: the next poll rests, ends the held turn, and the turn after that re-anchors the baseline. Distinguishing a bystander from the agent that was working needs per-agent turn identity, which `HH-TURN-PER-WORKTREE` trades away on purpose.
- A crash mid-snapshot costs at most one failed refresh. Ref updates are atomic, and leftover locks clear before the next snapshot and on every exit path.
- Two sidebars on one worktree agree on turn boundaries, since both read the same worktree. Each still snapshots on its own poll clock, so their baselines can differ by the edits made between the two samples, and each shows its own. They write one shared ref, last writer winning, which only seeds the next sidebar to open.

## Non-goals

- No clipboard over SSH. The export targets the machine where the binary runs.
- No herdr socket subscription. Turn tracking polls.
- No embedding in a caller's pane. The sidebar is always the plugin's own pane.
- No per-agent attribution in `last-turn`. herdr reports no per-file authorship.

## Related specs

- [configuration](./config.md)
- [input](./input.md)
- [overview](./overview.md)
- [review-model](./review-model.md)
- [theme](./theme.md)
