---
Status: Current
Created: 2026-06-23
Last edited: 2026-06-23
---

# herdr host

How herdr-review runs inside herdr, finds its repo, sends comments to the agent, and exports to the clipboard.

## Overview

herdr hosts the binary in a right split pane. A thin herdr plugin (`herdr-plugin.toml`) declares the pane and a key to toggle it, and may open it on `workspace.focused` so it is always present. Opening and closing the pane is herdr's job; the binary just runs in it.

The verified host command (see `../docs/herdr-api-notes.md`):

```
herdr pane split --direction right --ratio 0.35 --no-focus --cwd <repo>
```

### Repo discovery

The binary reviews one worktree: the pane's working directory, normalized to its git top-level with `git rev-parse --show-toplevel`. If that path is not a git repo, it shows an empty state rather than failing.

### Sending to the agent

The sidebar is split from the agent's pane, so they share a tab. `Send` always hands over every written comment at once. To send, the binary:

- resolves the target from `herdr agent list`: the agent in the sidebar's `$HERDR_TAB_ID`, else the sole agent in its `$HERDR_WORKSPACE_ID`;
- writes all comment blocks into that pane with `herdr agent send <agent_pane> "<text>"`, without submitting;
- focuses that pane with `herdr agent focus <agent_pane>`, so you add context and press enter.

If no agent resolves, or there are two and none shares the tab, the send fails and the status says so; the comments stay in the list. Clipboard copy (also the whole set) still works.

### Clipboard

The clipboard export uses the OS utility (`pbcopy` on macOS). The binary runs where the terminal renders, a local Ghostty, so the clipboard is the user's machine.

## Failure semantics

- The send path needs the herdr CLI; browsing diffs and the clipboard export do not, so the core works from a plain shell minus the agent send.
- If the clipboard utility or `herdr agent send` fails, the export reports an error and the comment stays in the list (see `review-model.md`).

## Non-goals

These are not built here; the architecture only stays open to them.

- No server-side clipboard under herdr-over-SSH; the export targets the machine the binary runs on.
- No `last-turn` scope — the binary does not subscribe to `pane.agent_status_changed` or snapshot the worktree. A future snapshot must be non-disruptive, using private refs only.

## Decisions

- Send via the herdr CLI, not the raw socket — `$HERDR_BIN_PATH agent send/focus/list` is the documented, transport-stable interface.
- Browsing and clipboard need no herdr — only the agent-send export depends on herdr, so the review loop degrades gracefully without it.

## Open decisions

- None.

## Related specs

- `./overview.md`
- `./review-model.md`
