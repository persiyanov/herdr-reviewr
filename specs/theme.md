---
Status: Current
Created: 2026-06-29
Last edited: 2026-07-10
---

# Theme

The color model: the named palettes, how a palette fills its slots from a few anchors, and how a theme is selected.

## Overview

A theme is a handful of anchor colors plus a paired syntax theme. Every other UI color is derived from the anchors, so a theme costs ~8 values, not ~20. One theme colors both the chrome and the diff, so the two never clash.

The user selects a theme by name, with the same name they use in herdr:

```toml
# $HERDR_PLUGIN_CONFIG_DIR/config.toml
theme = "tokyo-night"
```

The anchors, shown with the default `catppuccin` (Mocha) values:

| anchor     | role                                          | example   |
| ---------- | ---------------------------------------------- | --------- |
| `base`     | derivation reference and diff-fill background  | `#1e1e2e` |
| `text`     | primary foreground                             | `#cdd6f4` |
| `red`      | deletion accent                                | `#f38ba8` |
| `green`    | insertion accent                               | `#a6e3a1` |
| `yellow`   | warning and draft accent                       | `#f9e2af` |
| `peach`    | secondary accent                               | `#fab387` |
| `mauve`    | merged and keyword accent                      | `#cba6f7` |
| `lavender` | link and focus accent                          | `#b4befe` |

The derived slots:

| derived slot                  | derived from                             | use                              |
| ----------------------------- | ----------------------------------------- | -------------------------------- |
| `surface0/1/2`                | `base`, stepped toward the foreground     | fold, selection, cursor fills    |
| `overlay0/1`                  | `base`, stepped further                   | borders, dim chrome              |
| `subtext0`                    | `text`, dimmed toward `base`              | secondary text                   |
| `del_bg` / `ins_bg`           | `red` / `green` blended over `base`       | deletion / insertion row tint    |
| `emph_del_bg` / `emph_ins_bg` | `red` / `green`, a stronger blend         | word-emphasis fill               |

The theme set:

- Catppuccin: `catppuccin` (Mocha, the default), `catppuccin-latte`, `catppuccin-frappe`, `catppuccin-macchiato`.
- Dark: `dracula`, `nord`, `gruvbox`, `one-dark`, `solarized`, `monokai`, `tokyo-night`, `rose-pine`.
- Light: `gruvbox-light`, `one-light`, `solarized-light`, `github-light`, `tokyo-night-day`, `rose-pine-dawn`.

A herdr name outside this set (`kanagawa`, `kanagawa-lotus`, `vesper`, `terminal`, dark `github`) is not a valid config value until added (Non-goals).

## Behavior

### Palette derivation

- A theme lists its anchors. Derivation computes every other slot.
- Each theme declares its `appearance`, light or dark. Dark themes lighten `base` for surfaces, light themes darken it.
- The cursor, selection, and fold fills step `surface2` > `surface1` > `surface0`. The cursor is the strongest contrast, a fold the faintest.
- Both panes always fill their cursor row. The focused pane fills with `surface2`, the other with `surface1`, so which pane holds focus reads at a glance.
- `catppuccin` pins its whole palette as a literal and renders as faithful Catppuccin Mocha.

### Diff-fill legibility

- The row tints blend the accent over `base`. Emphasis blends more strongly.
- The blend starts higher on a dark base than a light one.
- It steps down until `text` keeps a minimum contrast ratio against the fill, so code on a fill stays legible on any base.

### Chrome and syntax pairing

- Selecting a theme sets the chrome palette and the syntax theme together. They never desync.
- Syntax spans contribute foreground colors only. The pane background stays transparent, so the diff sits on the terminal's own background.
- A theme reads correctly only when its `appearance` matches the terminal's. The user picks a theme matching their terminal.

### Theme selection

- Precedence: `--theme <name>` over the config `theme` over the default `catppuccin`.
- The config file is re-read on refresh. Editing `theme` and refreshing re-themes without a relaunch.
- Theme names match herdr's wherever both ship a palette, so a value copied from a herdr config resolves to the same palette.
- Standalone, with no `HERDR_PLUGIN_CONFIG_DIR`, reviewr reads no config file.

## Failure semantics

reviewr only reads the config file, never writes it, so concurrent sidebars and refresh re-reads need no coordination.

- A missing config or omitted `theme` resolves to the default. Invalid plugin config follows `config.md`.
- An unknown `--theme` override resolves to the default and logs the unsupported name.
- A theme whose syntax theme fails to load still renders its chrome. Syntax falls back to plain spans (`diff-view.md`).
- Re-reading on refresh is idempotent. An unchanged `theme` reuses the built palette. A changed value rebuilds it.

## Non-goals

- No new UI affordance. Theming changes only colors, no key, switcher, indicator, or status surface.
- No reading of herdr's own config to mirror its theme. Matching names give hand-sync instead.
- No `terminal` theme derived from the live terminal palette. Roadmap.
- No light/dark auto-detection. The default is `catppuccin` regardless of terminal appearance. Roadmap.
- No auto-switch pairing. Light and dark are separate named themes.
- No custom or user-defined palettes.
- No `kanagawa`, `kanagawa-lotus`, `vesper`, or dark `github` yet. Each needs a bundled syntax theme. Roadmap.
- No colorblind secondary cue for add/remove. Roadmap.
- No config keys beyond `theme` here. `--poll`, `--base`, and `--wrap` stay CLI-only, and the config file does not restore the removed `keep` list.

## Related specs

- [configuration](./config.md)
- [diff-view](./diff-view.md)
- [tui](./tui.md)
- [herdr-host](./herdr-host.md)
