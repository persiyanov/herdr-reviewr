# herdr API notes

The herdr surface herdr-reviewr depends on, confirmed live (last sweep 2026-07-31).
herdr-reviewr ships as a herdr **plugin** (`../herdr-plugin.toml`), and the binary is a plain
terminal program: any pane that runs it is a reviewr pane.

## Plugin manifest (`herdr-plugin.toml`)

Top-level: `id`, `name`, `version`, `min_herdr_version`, `platforms` (required); `description`.

```toml
[[build]]                                   # run on `plugin install`, skipped by `plugin link`
platforms = ["macos", "linux"]              # per entry; herdr runs every entry that matches
command = ["bash", "herdr/install.sh"]

[[build]]
platforms = ["windows"]
command = ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "herdr/install.ps1"]

[[panes]]                                   # an openable pane entrypoint
id = "pane"
placement = "split"                         # overlay (default) | split | tab | zoomed
command = ["bin/herdr-reviewr"]             # see "Command resolution" below

[[actions]]                                 # invokable command, bindable to a key
id = "toggle"
contexts = ["pane", "workspace"]
command = ["bin/herdr-reviewr", "--action", "toggle"]

[[events]]                                  # run a command on a herdr event
on = "worktree.created"
command = ["bin/herdr-reviewr", "--action", "auto-open"]

[[events]]
on = "worktree.opened"
command = ["bin/herdr-reviewr", "--action", "auto-open"]
```

**Command resolution** (read from herdr source, `program_for_cwd` in `src/plugin_command.rs`).
A relative `command[0]` holding a path separator joins the plugin root. Actions and events
resolve this way from 0.8.0, and panes from 0.9.0. The manifest asks for 0.9.3, which adds the event socket.
Before that, a pane command resolved against the pane's cwd (the repo under review). On
Windows the extension-less `bin/herdr-reviewr` still finds `bin\herdr-reviewr.exe`: Rust's
`Command` appends `.exe` for actions and events, and herdr's pty launcher tries `PATHEXT` for
panes. Actions and events run with the plugin root as their cwd. Seen working on
`windows-latest` with herdr 0.9.3: toggle ran `bin/herdr-reviewr --action toggle`, and the
pane it opened painted reviewr.

**Build entries** (`run_plugin_build_commands` in `src/cli/plugin.rs`, 0.9.0). Each `[[build]]`
entry takes its own `platforms`, falling back to the plugin's. herdr skips an entry whose
platforms miss the current OS and runs the rest in order, with the checkout as cwd and the
runtime env (`HERDR_SOCKET_PATH`, `HERDR_SESSION`, `HERDR_BIN_PATH`, `HERDR_PLUGIN_*`, ...)
removed.

**`plugin action invoke` answers before the action runs.** The reply carries a `log.log_id`.
The action's exit code, stdout, and stderr land later in `herdr plugin log list`.

**Plugin commands run concurrently** (herdr source, `start_plugin_command` in
`src/app/api/plugins/runtime.rs`). Every action invoke, keybinding action, and event hook is
spawned on its own thread, and nothing waits on it. There is no per-plugin queue, so the
`worktree.created` and `worktree.opened` hooks for one new workspace can run at the same time,
and so can two quick toggles.

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
  reports `argv0: "herdr-reviewr"` bare and `argv[0]` as the full binary path. The plugin
  actions (`src/actions.rs`) read this per pane to find the workspace's reviewr panes.

  herdr omits `foreground_processes` when the list is empty (`skip_serializing_if` in herdr
  source), so an answer without the key is zero processes, not a shape failure. On Windows the
  list holds exactly one process: the topmost recognized agent, else the pane's root process,
  with a path like `C:\…\bin\herdr-reviewr.exe`. herdr caches its Windows process snapshot for
  250 ms, so a pane opened inside that window answers with no processes until the cache
  catches up. That is why the open action waits for its new pane to read as reviewr.

  A `pane list` entry carries no foreground-process fields — its only process-adjacent keys
  are `foreground_cwd` and `terminal_title`/`terminal_title_stripped`, and the title is the
  same rewritable string as `name` above (verified live, 0.7.5). So the per-pane
  `process-info` read is required for identity; nothing in the list snapshot can replace it.

  A gone pane answers `{"error":{"code":"pane_not_found",…}}` with exit 1 from both
  `pane process-info` and plain `pane close` (verified live, 0.7.5). The actions key their
  converge-vs-refuse branches on that code.
- **`herdr pane rename <id> [LABEL]... [--clear]`** sets and clears a pane's label. The binary
  stamps its own pane `reviewr` at startup and clears it on a normal exit — that label is
  display only. The send picker reads other panes' labels to name its rows.
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
  pane by id; the close sweep uses it.
- `HERDR_PLUGIN_STATE_DIR` resolves to `~/.local/state/herdr/plugins/<plugin_id>/` (observed, 0.7.1).
- **Before 0.9.0, a pane command resolved against the pane's cwd (`--cwd`, the repo), not the plugin root.** From 0.9.0 it joins the plugin root, like an action's (see "Command resolution" above).

## Runtime env (plugin commands and panes)

`HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`, `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`,
`HERDR_PLUGIN_ID`, `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`,
`HERDR_PLUGIN_ENTRYPOINT_ID`, `HERDR_PLUGIN_CONTEXT_JSON`, and `HERDR_PLUGIN_EVENT_JSON` (events).
On macOS and Linux reviewr prepends the common host bin dirs when it resolves `git` and `herdr`
(`src/proc.rs`), so a stripped `PATH` cannot hide them. Windows has no such dirs to trust.

- **Action context** (`HERDR_PLUGIN_CONTEXT_JSON`): `workspace_id`, `tab_id`, `focused_pane_id`,
  `focused_pane_cwd`, `worktree:{repo_root, checkout_path, ...}`. The open action places a
  manual open from the focused pane's cwd, else `workspace_cwd`. The review UI reads none of it.
- **`focused_pane_cwd` is the pane's *launch* cwd, not its live one** (observed, 0.7.5: a pane
  running `claude -w <worktree>` reported the main checkout it was launched from, while the
  agent process had chdir'd into the worktree). `herdr pane get <id>` carries both: `.result.pane.cwd`
  (launch) and `.result.pane.foreground_cwd` (live foreground process). A `pane list` entry carries
  `foreground_cwd` too (see above), so the open action reads it from the pane-list snapshot it
  already holds and falls back to the context cwd.
- **`plugin action invoke` resolves context from the focused workspace**, wherever it is run — the
  calling pane's `HERDR_*` env is ignored, and `invoke <action_id> [--plugin ID]` has no workspace
  selector (verified live, 0.7.1: invoked from pane `w1X:p1`, context arrived for focused `w1B`).
- **`worktree.created` event** (`HERDR_PLUGIN_EVENT_JSON`): `.data.workspace.workspace_id`,
  `.data.workspace.worktree.checkout_path`, and `.data.worktree.{path, branch, open_workspace_id}`.
- **`worktree.opened` event**: the v0.7.5 tagged serializer and event source produce this raw
  `HERDR_PLUGIN_EVENT_JSON` shape (representative values):

  ```json
  {
    "event": "worktree_opened",
    "data": {
      "type": "worktree_opened",
      "workspace": {
        "workspace_id": "w3W",
        "number": 3,
        "label": "branch-name",
        "focused": false,
        "pane_count": 1,
        "tab_count": 1,
        "active_tab_id": "w3W:t1",
        "agent_status": "idle",
        "worktree": {
          "repo_key": "repo-key",
          "repo_name": "repo",
          "repo_root": "/repo",
          "checkout_path": "/repo/.herdr/worktrees/branch-name",
          "is_linked_worktree": true
        }
      },
      "worktree": {
        "path": "/repo/.herdr/worktrees/branch-name",
        "branch": "branch-name",
        "is_bare": false,
        "is_detached": false,
        "is_prunable": false,
        "is_linked_worktree": true,
        "open_workspace_id": "w3W",
        "label": "repo"
      },
      "already_open": false
    }
  }
  ```

  `already_open = false` means the command created the workspace. `true` means the workspace was
  already live. The event hook targets `.data.workspace.workspace_id` and
  `.data.workspace.worktree.checkout_path`; the `worktree` paths remain compatible fallbacks.

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
The picker also joins `label` from `herdr pane list --workspace <ws>` on `pane_id`, best effort,
using a non-empty pane label after the agent's `name` and before its display name or kind.

`herdr tab rename <tab_id> <label>` sets a tab's `label` (0.7.5). A `tab`-placement open uses it to
name the fresh tab `reviewr`.

Right before writing, the send reads `agent list` again and refuses only a `blocked` agent.
Verified on Claude Code 2.1.287 with the same bracketed paste reviewr sends:
- **At a permission prompt (`blocked`)**, the paste is silently dropped. The prompt ignores it,
  nothing is approved, and the input is empty once the prompt closes, whether cancelled or approved.
- **Mid-turn (`working`)**, the paste lands in the input as `[Pasted text #N]` and stays there
  after the turn ends, for the reviewer to submit.

So every other `agent_status` sends, `working` included. On 0.8.2, `agent list` answers in under
10 ms. The read and the write are two calls, and herdr has no atomic send-if-ready.

The write goes over herdr's socket API, and the focus stays on the CLI:

```
{"id":"reviewr:send","method":"pane.send_text","params":{"pane_id":"<agent_pane>","text":"<paste>"}}
herdr agent focus <agent_pane>   # focus so the reviewer submits
```

**Every failing call writes a JSON envelope to stderr, never a plain sentence** (verified live,
0.7.5, across `pane send-text`, `tab list`, and `agent focus`):

```
{"error":{"code":"pane_not_found","message":"pane w8:p2 not found"},"id":"cli:request"}
```

No part of this is fit for a 40-column status line, `message` included: it names a pane id the
reviewer never saw. reviewr logs the whole payload and shows a sentence of its own.

### The send over the socket (read from herdr source, `origin/master`, 2026-10-03)

**Why not the CLI.** `herdr pane send-text <pane> <text>` takes the review as one argument.
Windows caps a command line at 32,767 characters, so a longer review fails to send there. The CLI
is itself a socket client (`pane_send_text` in `src/cli/pane.rs` sends `pane.send_text`), so the
socket request is the same call without the argv.

**Transport** (`docs/next/website/src/content/docs/socket-api.mdx`, `src/api/server.rs`, `src/ipc.rs`):

- Newline-delimited JSON on `HERDR_SOCKET_PATH`, which every pane carries. On unix it is a Unix
  domain socket. On Windows it is the path of a marker file, and the named pipe is that path
  verbatim under `\\.\pipe\` (`connect_local_stream` maps it through interprocess's
  `GenericNamespaced`). reviewr connects the same way, through interprocess, and waits for a
  busy pipe only until the send's deadline.
- One request per connection. The server reads one line, answers with one line, and returns,
  which closes the connection. A few long-lived methods, such as `events.subscribe`, keep it open.
- A success echoes the id: `{"id":"reviewr:send","result":{"type":"ok"}}`. An error echoes it too,
  with the same `error` envelope the CLI prints. A transport error closes without a reply.
- **The cap is 1 MiB per request line, newline excluded** (`MAX_INITIAL_REQUEST_BYTES`). Past it
  the server stops reading and drops the connection unanswered. It applies to the first line of
  a connection, which for `pane.send_text` is the only one, so it caps the whole send.
- The cap that binds is time. The server gives up reading a request 5 s after the connection
  opens (`INITIAL_REQUEST_TIMEOUT`), reads one byte per call, and sleeps 100 ms whenever the
  socket is momentarily empty. On macOS's 8 KB socket buffers about 660 KB got through in the
  window. A Windows named pipe has no read window, but its 512-byte buffer moved about 90 KiB a
  second (Windows 11 ARM64, herdr's x64 build): 512 KiB took 5.7 s and 256 KiB 2.9 s. reviewr
  checks the serialized request against 256 KiB before connecting and refuses a review over it.
- reviewr waits 7 s for the reply: herdr's 5 s, plus 2 s for the answer. On unix every read and
  write on the connection ends at that deadline too. A Windows pipe takes no I/O timeout, so a
  herdr that accepts and never answers holds reviewr's worker thread until it closes the pipe.

**`pane.send_text`** (`handle_pane_send_text` in `src/app/api/panes.rs`) pushes `text` to the
pane's input channel as raw bytes: no bracketing, no newline conversion, no Enter. herdr's own
paste (`paste_payload` in `src/pane.rs`) converts newlines to CRLF on Windows
(`prepare_paste_text_for_pty_platform`) and leaves them alone elsewhere. reviewr encodes its
paste the same way, then brackets it always (`pasted` in `src/herdr.rs` says why).

- Only a `result` reply consumes the comments. An error reply is a refusal carrying its code, and
  only `pane_not_found` reads as the pane gone. A dropped or unanswered connection reads as herdr
  not answering. Both keep every comment, though in the second case the paste may still have
  landed.
- No `HERDR_SOCKET_PATH` is no herdr, the same refusal as a missing herdr binary.
- herdr 0.7.5 removed `agent send` (replaced by the logical-key `agent send-keys`). The literal,
  no-Enter write has been `pane send-text` since 0.7.0.

## Diff scopes (plain git, no herdr)

- Uncommitted: `git -C <repo> diff` + `git status --porcelain -z --untracked-files=all`.
- Branch: `git -C <repo> diff $(git merge-base origin/main HEAD)...HEAD`.
