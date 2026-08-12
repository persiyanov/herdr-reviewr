---
Status: Current
Created: 2026-07-10
Last edited: 2026-08-08
---

# Configuration

How reviewr finds, validates, and applies the plugin config at every entrypoint.

## Overview

A valid file may set any subset of the supported keys. A missing file and an omitted key use the defaults.

```toml
theme = "tokyo-night"
file_markdown_renderer = "glow -s {style} -w {width} -"
default_scope = "branch"
navigator_position = "bottom"
toggle_placement = "overlay"
toggle_direction = "down"
auto_open = false
github_host = "github.example.com"
gitlab_host = "git.corp.example"
azure_devops_host = "tfs.corp.example"

[keybindings]
comment = ["c", "ㅊ"]
select  = ["v", "ㅍ"]
find    = ["ctrl+f"]
```

| key                      | value                                                                                             |
| ------------------------ | ------------------------------------------------------------------------------------------------- |
| `theme`                  | one name from the theme set in `theme.md`                                                         |
| `file_markdown_renderer` | non-empty command string with balanced double quotes, or omitted for built-in Markdown            |
| `default_scope`          | `uncommitted`, `branch`, or `last-turn`                                                           |
| `navigator_position`     | `right`, `left`, `top`, or `bottom`                                                               |
| `toggle_placement`       | `split`, `overlay`, `zoomed`, or `tab`                                                            |
| `toggle_direction`       | `right` or `down`                                                                                 |
| `auto_open`              | boolean                                                                                           |
| `github_host`            | bare hostname other than `github.com`                                                             |
| `gitlab_host`            | bare hostname other than `gitlab.com`                                                             |
| `azure_devops_host`      | bare hostname other than `dev.azure.com`                                                          |
| `keybindings`            | table of actions from the keymap in `input.md`, each a non-empty array of keys                    |

`--resolve-plugin-config` prints the validated config as JSON, every key included, the keymap resolved.

The invariants:

| code                | Always true                                                                    |
| ------------------- | ------------------------------------------------------------------------------ |
| `CFG-WHOLE-FILE`    | An unknown key or an invalid value makes the whole file invalid.               |
| `CFG-BLOCKED-INERT` | An entrypoint that observes an invalid file performs none of its normal work.  |
| `CFG-ONE-SNAPSHOT`  | One operation or refresh uses one validated config snapshot.                   |

## The file

The config file is `config.toml` in the config directory. An entrypoint resolves the directory once, at startup, and rereads only the file. The directory is `$HERDR_PLUGIN_CONFIG_DIR` when set, else the one `herdr plugin config-dir persiyanov.reviewr` prints. A resolve that fails or exceeds a bounded wait resolves no directory, and no directory is the missing-file outcome, never an invalid config.

A read failure other than a missing file is an invalid config.

A config writer builds a complete file beside `config.toml` and replaces it atomically. A syntactically valid intermediate save applies as written.

## Snapshots

| entrypoint    | snapshot                                                                  |
| ------------- | ------------------------------------------------------------------------- |
| reviewr pane  | validated before every frame, serving that frame and the next input event |
| manual action | validated once at invocation                                              |
| plugin event  | validated once at invocation                                              |
| `PR` fetch    | none of its own, the pane's current snapshot (→ CFG-ONE-SNAPSHOT)         |

A later file change affects the next frame or invocation, not work already started (→ CFG-ONE-SNAPSHOT). Work started under a valid snapshot may finish after the file turns invalid, and its result is discarded. A turn baseline ref already written stays (`herdr-host.md`).

Concurrent entrypoints validate independently. None coordinates or persists config state.

## Invalid config

An error names the config path and the read, syntax, key, or value failure. It states the expected form when a value is invalid.

| entrypoint    | invalid config outcome                                                              |
| ------------- | ----------------------------------------------------------------------------------- |
| reviewr pane  | shows the config error plus its automatic-reload remedy and performs no review work |
| manual action | exits 1 with the config error and performs no action                                |
| plugin event  | exits 1, logs the config error, and performs no action                              |

An invalid first read blocks the plugin exactly like a later invalid read. A blocked pane keeps rereading the file and answers only the default `quit` key. A valid read clears the error and rebuilds the pane from fresh inputs without a reinstall or restart (`tui.md`).

## Key semantics

`file_markdown_renderer` splits on whitespace outside double quotes. Double quotes group one argument and are removed. The first argument is the program. reviewr executes the argv directly without a shell. Shell operators such as `|`, `>`, and `&&` are ordinary arguments.

The command receives the Markdown file content on standard input. It never receives untrusted content as an argument. `{style}` inside an argument becomes `dark` or `light` from the active resolved theme. `{width}` inside an argument becomes the current preview width. The command applies only to file previews. It never changes PR descriptions or comments (`markdown.md`).

A hostname is recognized by at most one forge. A host key naming another host key's value, or any forge's built-in host, `*.visualstudio.com` included, is an invalid value (→ CFG-WHOLE-FILE).

`navigator_position` applies at startup and after config recovery. The `navigator-position` action changes the position for the session. A valid snapshot replaces the session position only when its `navigator_position` differs from the previous valid snapshot's. Recovery preserves both session navigator shares and the hidden state (`tui.md`).

## Keybindings

`[keybindings]` rebinds the action shortcuts: the resolved keymap is the default keymap with each bound action's keys replaced by its binding. A key is one printable, non-whitespace codepoint, alone or with a `ctrl+`/`alt+` prefix. A chord-only action, like `find`, rebinds like any other (`input.md`).

A binding never displaces a fixed key (`input.md`). An unknown action name is an unknown key. A malformed key and a duplicate key are invalid values (→ CFG-WHOLE-FILE). A key appears at most once across the resolved keymap. A default added by an upgrade may collide with an existing custom binding, and the collision is invalid, the error naming both actions and the shared character.

`list-wider` and `list-narrower` are accepted aliases for `navigator-grow` and `navigator-shrink`. A config naming an action and its alias is invalid as a duplicate action. Resolved output uses the canonical names.

## Related specs

- [forge host](./forge-host.md)
- [herdr host](./herdr-host.md)
- [review model](./review-model.md)
- [theme](./theme.md)
- [input](./input.md)
- [tui](./tui.md)
