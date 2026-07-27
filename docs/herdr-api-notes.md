# herdr API notes (verified against herdr 0.7.5)

The herdr surface herdr-review depends on, confirmed live. herdr-review ships as a
herdr **plugin** (`../herdr-plugin.toml`); the binary runs inside a plugin pane.

## Plugin manifest (`herdr-plugin.toml`)

Top-level: `id`, `name`, `version`, `min_herdr_version`, `platforms` (required); `description`.

```toml
[[build]]                                   # run on `plugin install`, skipped by `plugin link`
command = ["cargo", "install", "--path", "."]

[[panes]]                                   # an openable pane entrypoint
id = "sidebar"
placement = "split"                         # overlay (default) | split | tab | zoomed
command = ["herdr-review"]                  # see "pane command" below

[[actions]]                                 # invokable command, bindable to a key
id = "toggle"
contexts = ["pane", "workspace"]
command = ["bash", "herdr/sidebar.sh", "toggle"]

[[events]]                                  # run a command on a herdr event
on = "worktree.created"
command = ["bash", "herdr/sidebar.sh", "open"]
```

Lifecycle: `herdr plugin link <dir>` (local dev, no build) · `herdr plugin install <owner>/<repo>` ·
`plugin list` · `plugin action invoke <action_id> --plugin <id>` · `plugin log list --plugin <id>`.

## Open / close the sidebar pane

```
herdr plugin pane open --plugin reviewr --entrypoint sidebar \
  --placement split --direction right --target-pane <pane> --cwd <repo> --no-focus
herdr plugin pane close <pane_id>
```
- A `split` (or `zoomed`) pane **must** pass `--target-pane` (it implies the workspace); `--workspace` alone errors.
- New pane id: `.result.plugin_pane.pane.pane_id`. The pane is auto-labeled with the entrypoint `title`.
- The same pane object carries `tab_id` (verified across 10 live plugin panes, 0.7.5). A `tab`-placement open reads `.result.plugin_pane.pane.tab_id` to rename the fresh tab.
- **`plugin pane close` only closes panes in the in-memory plugin-pane registry** — after a herdr
  restart it refuses a still-live sidebar with `plugin_pane_not_found` (observed, 0.7.1). Plain
  `herdr pane close <pane_id>` closes any pane by id; prefer it for teardown.
- `HERDR_PLUGIN_STATE_DIR` resolves to `~/.local/state/herdr/plugins/<plugin_id>/` (observed, 0.7.1).
- **Pane command resolves against the pane's cwd (`--cwd`, the repo), not the plugin root** — a relative `./target/...` path fails, so invoke the binary by name (`herdr-review`) on `PATH` and install it via the `[[build]]` step.

## Runtime env (plugin commands and panes)

`HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`, `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`,
`HERDR_PLUGIN_ID`, `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`,
`HERDR_PLUGIN_ENTRYPOINT_ID`, `HERDR_PLUGIN_CONTEXT_JSON`, and `HERDR_PLUGIN_EVENT_JSON` (events).
herdr runs plugin commands with a minimal `PATH`; prepend common bin dirs for `jq`/`git`.

- **Action context** (`HERDR_PLUGIN_CONTEXT_JSON`): `workspace_id`, `tab_id`, `focused_pane_id`,
  `focused_pane_cwd`, `worktree:{repo_root, checkout_path, ...}`.
- **A plugin pane inherits the context of the open that created it** (verified live, 0.7.5: nine
  running sidebars, each carrying `HERDR_PLUGIN_CONTEXT_JSON` with `invocation_source: "api"` and
  `correlation_id: "plugin-pane"`). `focused_pane_id` names the pane the sidebar was opened beside,
  never the sidebar's own `HERDR_PANE_ID`, which is what level 2 of the picker's arming ladder
  reads (`../specs/herdr-host.md`). A sidebar opened beside a non-agent pane carries that pane's
  id and no `focused_pane_agent`, so the ladder falls through to the first row.
- **`plugin action invoke` resolves context from the focused workspace**, wherever it is run — the
  calling pane's `HERDR_*` env is ignored, and `invoke <action_id> [--plugin ID]` has no workspace
  selector (verified live, 0.7.1: invoked from pane `w1X:p1`, context arrived for focused `w1B`).
- **`worktree.created` event** (`HERDR_PLUGIN_EVENT_JSON`): `.data.workspace.workspace_id`,
  `.data.workspace.worktree.checkout_path`, and `.data.worktree.{path, branch, open_workspace_id}`.

## Keybinding (user config, not the manifest)

```toml
[[keys.command]]
key = "cmd+r"
type = "plugin_action"
command = "persiyanov.reviewr.toggle"   # <plugin_id>.<action_id> — plugin_id is the manifest `id`, not `name`
```
`cmd+…` chords reach herdr; `alt+…` chords are composed into characters by macOS and don't register.

## Resolve the agent / send comments

`herdr agent list` → `{"result":{"agents":[ {pane_id, tab_id, workspace_id, agent_status, cwd, ...} ]}}`.
It takes no flags, so any filter is the caller's to apply. The row order is herdr's:
observed on 0.7.5 across 13 live agents, entries arrive grouped by workspace and by tab within a
workspace. No sample held two agents in one tab, so the order inside a tab is unverified.

- Send candidates = every agent in the sidebar's `HERDR_WORKSPACE_ID`. One sends directly,
  several open the picker (`../specs/herdr-host.md`). Turn tracking reads no pane topology at
  all: it takes every agent's `cwd` and keeps those resolving to the sidebar's git top level.
- `cwd` and `foreground_cwd` both carry the agent's working directory, and matched on every
  entry of a 10-agent sample. Each entry also carries `agent_session` (a stable UUID),
  `state_change_seq`, `focused`, and `terminal_title_stripped`, none of which reviewr reads.
- 0.7.5 lists only real agent panes. A plugin sidebar or a plain shell appears in `pane list`
  with `agent: null` and never in `agent list`, so excluding our own pane is defensive.
- `name`, `display_agent`, and `state_labels` are omitted entirely until something sets them.
  `herdr agent rename <pane> <name>` makes `name` appear; `--clear` leaves it present and null.
  Names are `[a-z0-9_-]{1,32}` and must start with a lowercase letter, so they carry no spaces.

`herdr tab list --workspace <ws>` → `{"result":{"tabs":[ {tab_id, label, number, pane_count} ]}}`.
`label` and `number` differ: a tab with `number: 4` defaults to `label: "1"`, a per-workspace
ordinal. The picker joins `label` on `tab_id`, best effort.

`herdr tab rename <tab_id> <label>` sets a tab's `label` (0.7.5). A `tab`-placement open uses it to
name the fresh tab `reviewr`.

```
herdr pane send-text <agent_pane> "<literal text>"   # writes input, no Enter
herdr agent focus    <agent_pane>                    # focus so the reviewer submits
```

**Every failing call writes a JSON envelope to stderr, never a plain sentence** (verified live,
0.7.5, across `pane send-text`, `tab list`, and `agent focus`):

```
{"error":{"code":"pane_not_found","message":"pane w8:p2 not found"},"id":"cli:request"}
```

No part of this is fit for a 40-column status line, `message` included: it names a pane id the
reviewer never saw. reviewr logs the whole payload and shows a sentence of its own
(`../specs/herdr-host.md`).

- `pane send-text` writes the literal bytes to the pane without Enter, unchanged since 0.7.0.
- herdr 0.7.5 removed `agent send` (replaced by the logical-key `agent send-keys`). On 0.7.0 both
  commands dispatched to the same server write, so `pane send-text` covers the whole range.

## Diff scopes (plain git, no herdr)

- Uncommitted: `git -C <repo> diff` + `git status --porcelain -z --untracked-files=all`.
- Branch: `git -C <repo> diff $(git merge-base origin/main HEAD)...HEAD`.
