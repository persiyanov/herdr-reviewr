# herdr API notes (verified against herdr 0.7.5)

The herdr surface herdr-reviewr depends on, confirmed live (last sweep 2026-07-31).
herdr-reviewr ships as a herdr **plugin** (`../herdr-plugin.toml`), and the binary is a plain
terminal program: any pane that runs it is a reviewr pane.

## Plugin manifest (`herdr-plugin.toml`)

Top-level: `id`, `name`, `version`, `min_herdr_version`, `platforms` (required); `description`.

```toml
[[build]]                                   # run on `plugin install`, skipped by `plugin link`
command = ["cargo", "install", "--path", "."]

[[panes]]                                   # an openable pane entrypoint
id = "pane"
placement = "split"                         # overlay (default) | split | tab | zoomed
command = ["herdr-reviewr"]                 # see "pane command" below

[[actions]]                                 # invokable command, bindable to a key
id = "toggle"
contexts = ["pane", "workspace"]
command = ["bash", "herdr/pane.sh", "toggle"]

[[events]]                                  # run a command on a herdr event
on = "worktree.created"
command = ["bash", "herdr/pane.sh", "open"]
```

Lifecycle: `herdr plugin link <dir>` (local dev, no build) · `herdr plugin install <owner>/<repo>` ·
`plugin list` · `plugin action invoke <action_id> --plugin <id>` · `plugin log list --plugin <id>`.

## Pane identity: the plain-pane surface (verified 2026-07-31)

The direct-run mode rides on four calls plus the plain-pane env, all confirmed live on 0.7.5:

- **Every pane carries `HERDR_PANE_ID`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, and
  `HERDR_SOCKET_PATH`** — plugin panes, layout panes, and hand-opened shells alike. The binary
  needs no plugin env to know its own pane.
- **`herdr pane process-info --pane <id>`** → the pane's foreground process group:

  ```json
  {"result":{"process_info":{"foreground_process_group_id":17124,"foreground_processes":[
    {"pid":17124,"name":"herdr-reviewr","argv0":"herdr-reviewr",
     "argv":["/…/bin/herdr-reviewr"],"cwd":"/…/repo"}],
    "pane_id":"w4:p5","shell_pid":81333}}}
  ```

  `name` is the rewritable process title, not the executable (a live claude pane reports
  `name: "2.1.220"`), so identity keys on the `argv0`/`argv[0]` basename. A live reviewr pane
  reports `argv0: "herdr-reviewr"` bare and `argv[0]` as the full binary path. `pane.sh` reads
  this per pane to find the workspace's reviewr panes.

  A `pane list` entry carries no foreground-process fields — its only process-adjacent keys
  are `foreground_cwd` and `terminal_title`/`terminal_title_stripped`, and the title is the
  same rewritable string as `name` above (verified live, 0.7.5). So the per-pane
  `process-info` read is required for identity; nothing in the list snapshot can replace it.

  A gone pane answers `{"error":{"code":"pane_not_found",…}}` with exit 1 from both
  `pane process-info` and plain `pane close` (verified live, 0.7.5). `pane.sh` keys its
  converge-vs-refuse branches on that code.
- **`herdr pane rename <id> [LABEL]... [--clear]`** sets and clears a pane's label. The binary
  stamps its own pane `reviewr` at startup and clears it on a normal exit — display only,
  nothing reads it back.
- **`herdr plugin config-dir <plugin_id>`** prints the plugin's config directory
  (`~/.config/herdr/plugins/config/persiyanov.reviewr`). The binary falls back to it when
  `HERDR_PLUGIN_CONFIG_DIR` is unset, so a hand-launched pane reads the same `config.toml`.
- **`herdr pane split [--pane <id>|--current] [--direction …] [--ratio …] [--cwd …] [--env K=V]
  [--focus|--no-focus]`, `pane run <id> <command>…`, `pane current`** exist for layout tooling.
  reviewr's own actions still open through `plugin pane open`; a layout plugin can use these
  directly with `command = "herdr-reviewr"` and the result is the same reviewr pane.

## Open / close a reviewr pane

```
herdr plugin pane open --plugin reviewr --entrypoint pane \
  --placement split --direction right --target-pane <pane> --cwd <repo> --no-focus
herdr plugin pane close <pane_id>
```
- A `split` (or `zoomed`) pane **must** pass `--target-pane` (it implies the workspace); `--workspace` alone errors.
- New pane id: `.result.plugin_pane.pane.pane_id`. The pane is auto-labeled with the entrypoint `title`.
- The same pane object carries `tab_id` (verified across 10 live plugin panes, 0.7.5). A `tab`-placement open reads `.result.plugin_pane.pane.tab_id` to rename the fresh tab.
- **`plugin pane close` only closes panes in the in-memory plugin-pane registry** — after a herdr
  restart it refuses a still-live pane with `plugin_pane_not_found` (observed, 0.7.1), and a
  layout-launched pane was never registered at all. Plain `herdr pane close <pane_id>` closes any
  pane by id; `pane.sh` sweeps with it.
- `HERDR_PLUGIN_STATE_DIR` resolves to `~/.local/state/herdr/plugins/<plugin_id>/` (observed, 0.7.1).
- **Pane command resolves against the pane's cwd (`--cwd`, the repo), not the plugin root** — a relative `./target/...` path fails, so the manifest invokes the binary by absolute path under `$HERDR_PLUGIN_ROOT`.

## Runtime env (plugin commands and panes)

`HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`, `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`,
`HERDR_PLUGIN_ID`, `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`,
`HERDR_PLUGIN_ENTRYPOINT_ID`, `HERDR_PLUGIN_CONTEXT_JSON`, and `HERDR_PLUGIN_EVENT_JSON` (events).
herdr runs plugin commands with a minimal `PATH`; prepend common bin dirs for `jq`/`git`.

- **Action context** (`HERDR_PLUGIN_CONTEXT_JSON`): `workspace_id`, `tab_id`, `focused_pane_id`,
  `focused_pane_cwd`, `worktree:{repo_root, checkout_path, ...}`. `pane.sh` places a manual
  open from the focused pane's cwd; the binary reads none of it.
- **`focused_pane_cwd` is the pane's *launch* cwd, not its live one** (observed, 0.7.5: a pane
  running `claude -w <worktree>` reported the main checkout it was launched from, while the
  agent process had chdir'd into the worktree). `herdr pane get <id>` carries both: `.result.pane.cwd`
  (launch) and `.result.pane.foreground_cwd` (live foreground process). A `pane list` entry carries
  `foreground_cwd` too (see above), so `pane.sh` reads it from the pane-list snapshot it already
  holds and falls back to the context cwd.
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

- Send candidates = every agent in the reviewr pane's `HERDR_WORKSPACE_ID`. One sends directly,
  several open the picker. Turn tracking reads no pane topology at
  all: it takes every agent's `cwd` and keeps those resolving to the reviewr pane's git top level.
- `cwd` and `foreground_cwd` both carry the agent's working directory, and matched on every
  entry of a 10-agent sample. Each entry also carries `agent_session` (a stable UUID),
  `state_change_seq`, `focused`, and `terminal_title_stripped`, none of which reviewr reads.
- 0.7.5 lists only real agent panes. A reviewr pane or a plain shell appears in `pane list`
  without an `agent` key and never in `agent list`, so excluding our own pane is defensive.
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
reviewer never saw. reviewr logs the whole payload and shows a sentence of its own.

- `pane send-text` writes the literal bytes to the pane without Enter, unchanged since 0.7.0.
- herdr 0.7.5 removed `agent send` (replaced by the logical-key `agent send-keys`). On 0.7.0 both
  commands dispatched to the same server write, so `pane send-text` covers the whole range.

## Diff scopes (plain git, no herdr)

- Uncommitted: `git -C <repo> diff` + `git status --porcelain -z --untracked-files=all`.
- Branch: `git -C <repo> diff $(git merge-base origin/main HEAD)...HEAD`.
