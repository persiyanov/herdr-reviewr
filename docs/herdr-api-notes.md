# herdr API notes (verified against herdr 0.7.1)

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

`herdr agent list` → `{"result":{"agents":[ {pane_id, tab_id, workspace_id, agent_status, ...} ]}}`.
- Send target = the agent in the sidebar's `HERDR_TAB_ID`, else the sole agent in its `HERDR_WORKSPACE_ID`.
- **Caveat:** the reviewr pane itself is listed as an agent — exclude `HERDR_PANE_ID` or the real agent looks ambiguous.

```
herdr agent send  <agent_pane> "<literal text>"   # writes input, no Enter
herdr agent focus <agent_pane>                    # focus so the reviewer submits
```

## Diff scopes (plain git, no herdr)

- Uncommitted: `git -C <repo> diff` + `git status --porcelain -z --untracked-files=all`.
- Branch: `git -C <repo> diff $(git merge-base origin/main HEAD)...HEAD`.
