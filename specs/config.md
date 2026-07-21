---
Status: Current
Created: 2026-07-10
Last edited: 2026-07-18
---

# Configuration

How reviewr validates and applies `$HERDR_PLUGIN_CONFIG_DIR/config.toml` across the sidebar binary, actions, and events.

## Overview

The plugin config is one typed value. A valid file may set any subset of the supported keys.

```toml
theme = "tokyo-night"
base_branches = ["develop", "main", "master"]
default_scope = "branch"
navigator_position = "bottom"
toggle_placement = "overlay"
toggle_direction = "down"
auto_open = false
github_host = "github.example.com"

[keybindings]
comment = ["c", "ㅊ"]
select  = ["v", "ㅍ"]
```

| key                  | value                                                                              |
| -------------------- | ---------------------------------------------------------------------------------- |
| `theme`              | one name from the theme set in `theme.md`                                          |
| `base_branches`      | non-empty array of non-empty branch names, `origin/` and `refs/` prefixes accepted |
| `default_scope`      | `uncommitted`, `branch`, or `last-turn`                                            |
| `navigator_position` | `right` (default), `left`, `top`, or `bottom`                                      |
| `toggle_placement`   | `split`, `overlay`, `zoomed`, or `tab`                                             |
| `toggle_direction`   | `right` or `down`                                                                  |
| `auto_open`          | boolean                                                                            |
| `github_host`        | bare hostname other than `github.com`                                              |
| `keybindings`        | table of actions from the keymap in `input.md`, each a non-empty array of keys     |

## Behavior

The cross-entrypoint invariants, coded for citation:

| code                   | Always true                                                                    |
| ---------------------- | ------------------------------------------------------------------------------ |
| `CFG-MISSING-DEFAULTS` | A missing config file uses every default.                                      |
| `CFG-WHOLE-FILE`       | An unknown key or an invalid value makes the whole file invalid.               |
| `CFG-BLOCKED-INERT`    | An entrypoint that observes an invalid file performs none of its normal work.  |
| `CFG-ONE-SNAPSHOT`     | One operation or refresh uses one validated config snapshot.                   |

An omitted key uses that key's default. An invalid file applies none of its keys. Every sidebar frame, manual action, and plugin event validates the whole file first.

Each `base_branches` entry canonicalizes to one bare branch name: a leading `refs/heads/`, `refs/remotes/origin/`, or `origin/` prefix is stripped. Duplicate entries collapse to the first occurrence. Every consumer resolves an entry through `refs/remotes/origin/<name>`, then `refs/heads/<name>`. The `--base` flag resolves verbatim first, then as a canonical entry. `origin/HEAD` backstops an unresolvable list (`review-model.md`).

A repository may lack every ref named by a valid `base_branches` list. That is runtime absence, not invalid configuration.

An error names the config path and the read, syntax, key, or value failure. It states the expected form when a value is invalid.

| entrypoint       | invalid config outcome                                               |
| ---------------- | -------------------------------------------------------------------- |
| sidebar binary   | shows the config error plus its automatic-reload remedy; performs no review work |
| manual action    | exits 1 with the config error and performs no action                   |
| plugin event     | exits 1, logs the config error, and performs no action                 |

The sidebar reads the file at startup and on every refresh. While blocked, it starts no new review work and performs the config reads needed to detect a fix.

`navigator_position` sets the position at startup and after config recovery. The `navigator-position` action may change it for the current session. A later valid config snapshot replaces the session position only when its `navigator_position` differs from the previous valid snapshot. An unchanged reread or an edit to another config key preserves the session position. Recovery reapplies the configured position and preserves both session navigator shares.

The `PR` tab's fetch is not a config read. It runs under the sidebar's current snapshot (→ CFG-ONE-SNAPSHOT).

`--resolve-plugin-config` prints the validated config as JSON, every key included, the keymap resolved.

Work started under a valid snapshot may finish after the config becomes invalid. Its result is discarded. A turn baseline ref already written stays: it records a true observation of the worktree (`herdr-host.md`).

An action or event reads the file once at invocation. A later file change affects the next invocation, not work already started (→ CFG-ONE-SNAPSHOT).

Config writers must build a complete file beside `config.toml`, then replace it atomically. reviewr cannot identify a syntactically valid intermediate save as unfinished.

### Keybindings

`[keybindings]` rebinds the character shortcuts. The resolved keymap is the default keymap with each bound action's characters replaced by its binding.

`list-wider` and `list-narrower` remain accepted aliases for `navigator-grow` and `navigator-shrink`. A config that names an action and its alias is invalid as a duplicate action. Resolved config output uses the canonical names.

An existing custom binding does not displace a newly added default. If an upgrade creates a collision in the resolved keymap, the config is invalid under `CFG-KEY-UNIQUE`. The error names both actions and the shared character.

The sidebar validates before drawing each frame. That frame and the next input event use the resulting config and layout snapshot. A file change after drawing affects the following frame.

| code                | Always true                                                          |
| ------------------- | -------------------------------------------------------------------- |
| `CFG-KEY-PRINTABLE` | A key is one codepoint, printable and not whitespace.                |
| `CFG-KEY-UNIQUE`    | A character appears at most once across the resolved keymap's lists. |

A binding never displaces a fixed key (`input.md`). An unknown action name is an unknown key (→ CFG-WHOLE-FILE). A `CFG-KEY-PRINTABLE` or `CFG-KEY-UNIQUE` violation is an invalid value (→ CFG-WHOLE-FILE). A collision error names each action involved.

A blocked sidebar answers only the default `quit` key.

## Traces

**Live config breaks and recovers**

1. The sidebar reads a valid file. The plugin works with that complete config.
2. The user saves an invalid value. The next read blocks the sidebar with the config error (→ CFG-BLOCKED-INERT).
3. The user invokes an action. The action refuses without a side effect (→ CFG-BLOCKED-INERT).
4. The user fixes the file. The next read applies the complete config and restores the sidebar.

**Config changes during an action**

1. An action validates one config snapshot (→ CFG-ONE-SNAPSHOT).
2. The user edits the file while the action runs. The action finishes with its snapshot.
3. The next entrypoint reads the new file. It uses the new config or refuses it as a whole.

**Atomic replacement**

1. The plugin reads the current valid file.
2. The user writes a complete replacement beside it, then atomically replaces `config.toml`.
3. A concurrent entrypoint reads either complete version. It never reads an intermediate edit.

## Failure semantics

- A missing file is valid (→ CFG-MISSING-DEFAULTS). Any other read failure is an invalid config.
- An invalid first read blocks the plugin exactly like a later invalid read.
- A valid later read clears the error and rebuilds the sidebar from fresh inputs without a plugin reinstall or restart.
- A valid intermediate file is indistinguishable from an intended config. Non-atomic writers can apply it.
- Concurrent entrypoints validate independently. None coordinates or persists config state.

## Related specs

- [forge host](./forge-host.md)
- [herdr host](./herdr-host.md)
- [review model](./review-model.md)
- [theme](./theme.md)
- [input](./input.md)
- [tui](./tui.md)
