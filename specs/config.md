---
Status: Current
Created: 2026-07-10
Last edited: 2026-07-11
---

# Configuration

How reviewr validates and applies `$HERDR_PLUGIN_CONFIG_DIR/config.toml` across the sidebar binary, actions, and events.

## Overview

The plugin config is one typed value. A valid file may set any subset of the supported keys.

```toml
theme = "tokyo-night"
base_branches = ["origin/develop", "origin/main", "main", "master"]
toggle_placement = "overlay"
toggle_direction = "down"
auto_open = false
github_host = "github.example.com"
gitlab_host = "gitlab.corp.com"
bitbucket_host = "bitbucket.corp.com"
```

| key                    | value                                                                    |
| ---------------------- | ------------------------------------------------------------------------ |
| `theme`                | one name from the theme set in `theme.md`                                 |
| `base_branches`        | non-empty array of non-empty ref names                                    |
| `toggle_placement`     | `split`, `overlay`, `zoomed`, or `tab`                                    |
| `toggle_direction`     | `right` or `down`                                                         |
| `auto_open`            | boolean                                                                  |
| `github_host`          | bare hostname outside the `github.com` and `github.com-*` namespace       |
| `gitlab_host`          | bare hostname (`gitlab.com` is always supported and needs no entry here)  |
| `bitbucket_host`       | bare hostname naming a Bitbucket Data Center instance (no default; the key is meant for Data Center hosts — setting it to Bitbucket Cloud's `bitbucket.org` passes validation and is not rejected, but Cloud's API differs from Data Center's, so fetches against it fail) |

## Behavior

| #  | Always true                                                                                  |
| -- | -------------------------------------------------------------------------------------------- |
| C1 | A missing config file uses every default.                                                     |
| C2 | An omitted key uses that key's default.                                                       |
| C3 | An unknown key makes the whole file invalid.                                                  |
| C4 | An invalid value makes the whole file invalid.                                                |
| C5 | An invalid file applies none of its keys.                                                      |
| C6 | Every sidebar, action, and event validates the whole file before doing its normal work.         |
| C7 | An entrypoint that observes an invalid file performs none of its normal work.                   |
| C8 | One operation or refresh uses one validated config snapshot.                                   |

A repository may lack every ref named by a valid `base_branches` list. That is runtime absence, not invalid configuration.

An error names the config path and the read, syntax, key, or value failure. It states the expected form when a value is invalid.

| entrypoint       | invalid config outcome                                               |
| ---------------- | -------------------------------------------------------------------- |
| sidebar binary   | shows only the config error and performs no review work               |
| manual action    | exits 1 with the config error and performs no action                   |
| plugin event     | exits 1, logs the config error, and performs no action                 |

The sidebar reads the file at startup and on every refresh. While blocked, it starts no new review work and performs the config reads needed to detect a fix.

Work started under a valid snapshot may finish after the config becomes invalid. Its result is discarded.

An action or event reads the file once at invocation. A later file change affects the next invocation, not work already started (→ C8).

Config writers must build a complete file beside `config.toml`, then replace it atomically. reviewr cannot identify a syntactically valid intermediate save as unfinished.

## Traces

**T1 — live config breaks and recovers**

1. The sidebar reads a valid file. The plugin works with that complete config.
2. The user saves an invalid value. The next read blocks the sidebar with the config error (→ C7).
3. The user invokes an action. The action refuses without a side effect (→ C7).
4. The user fixes the file. The next read applies the complete config and restores the sidebar.

**T2 — config changes during an action**

1. An action validates one config snapshot (→ C8).
2. The user edits the file while the action runs. The action finishes with its snapshot.
3. The next entrypoint reads the new file. It uses the new config or refuses it as a whole.

**T3 — atomic replacement**

1. The plugin reads the current valid file.
2. The user writes a complete replacement beside it, then atomically replaces `config.toml`.
3. A concurrent entrypoint reads either complete version. It never reads an intermediate edit.

## Failure semantics

- A missing file is valid (→ C1). Any other read failure is an invalid config.
- An invalid first read blocks the plugin exactly like a later invalid read.
- A valid later read clears the error and rebuilds the sidebar from fresh inputs without a plugin reinstall or restart.
- A valid intermediate file is indistinguishable from an intended config. Non-atomic writers can apply it.
- Concurrent entrypoints validate independently. None coordinates or persists config state.

## Related specs

- [forge host](./forge-host.md)
- [herdr host](./herdr-host.md)
- [review model](./review-model.md)
- [theme](./theme.md)
