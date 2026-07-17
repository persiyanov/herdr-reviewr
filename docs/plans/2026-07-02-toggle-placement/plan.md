# Configurable toggle placement — Delivery Plan

**Specs:** ../../../specs/ — the living reference this plan delivers

## Milestone Map

1. **Configurable toggle placement** — single milestone; the toggle opens reviewr in the `toggle_placement`/`toggle_direction` from `config.toml`, and `worktree.created` auto-opens only for the non-covering placements.

## Goal

The `toggle` action opens reviewr with the placement and direction the user sets in `config.toml`, defaulting to today's right split.

## Definition of Done

- The toggle opens `split`, `overlay`, `zoomed`, or `tab` per `toggle_placement`; `toggle_direction` orients the split (`right`/`down`).
- The `worktree.created` event auto-opens reviewr for `split`/`tab` and does nothing for `overlay`/`zoomed`.
- Focus follows `herdr-host.md` P5/P6: split ambient, covering/tab focused on manual toggle, event never steals focus.
- An unknown or missing key falls back to its default without error.
- `README.md` documents both keys; `herdr-plugin.toml` comment no longer claims a fixed right split.

## Exit State

| Artifact | Kind | Exercised by | Spec |
| --- | --- | --- | --- |
| `read_config()` in `herdr/sidebar.sh` | shell function | reads `toggle_placement`/`toggle_direction` from `$HERDR_PLUGIN_CONFIG_DIR/config.toml` each run | `herdr-host.md#sidebar-placement` |
| placement branch in `herdr/sidebar.sh` | shell logic | maps placement to the `plugin pane open` selector, direction, and focus flag | `herdr-host.md#sidebar-placement` |
| auto-open gate in `herdr/sidebar.sh` | shell guard | `open` mode exits early unless placement is `split`/`tab` | `herdr-host.md#sidebar-placement` |
| `README.md` Configuration rows | docs | lists `toggle_placement`/`toggle_direction`, values, defaults | `herdr-host.md#sidebar-placement` |
| `herdr-plugin.toml` toggle comment | comment | describes the configurable placement | `herdr-host.md#sidebar-placement` |
| `CHANGELOG.md` unreleased entry | changelog | records the new config keys | `herdr-host.md#sidebar-placement` |

## Specs Touched

| Spec | What this plan realizes | At the gate |
| --- | --- | --- |
| `herdr-host.md` | the whole `### Sidebar placement` section (P1–P7, T1, T2) | Draft → Current |

## Out of Scope

- `toggle_focus` key — focus is derived from placement, not user-set.
- Overlay sizing/alignment — herdr exposes no such flags.
- Explicit `tab` rename — the derived tab name is accepted.

## Likely Files

- `herdr/sidebar.sh` — touched: config read, placement branch, auto-open gate.
- `README.md` — touched: Configuration section.
- `herdr-plugin.toml` — touched: toggle comment.
- `CHANGELOG.md` — touched: unreleased entry.

## Execution Plan

1. [x] Add `read_config()` reading `toggle_placement`/`toggle_direction` from `config.toml` with per-key default fallback (`split`/`right`).
2. [x] Branch the `plugin pane open` call by placement: `split`/`zoomed` → `--target-pane`, `tab` → `--workspace`, `overlay` → no selector; `--direction` for `split` only.
3. [x] Apply focus: `--no-focus` when mode is `open` or placement is `split`; else focus.
4. [x] Gate the `open` mode to exit early for `overlay`/`zoomed`.
5. [x] Document both keys in `README.md`; fix the `herdr-plugin.toml` comment; add the `CHANGELOG.md` entry.

## Verification

- **Done:** drive each placement live under herdr — `toggle_placement` = split/overlay/zoomed/tab, toggle, observe the pane shape from `../../docs/herdr-api-notes.md`.
- **Tight:** row-check the `sidebar.sh`/README/manifest/changelog diff against the Exit State table — every added branch and doc row is exercised, nothing beyond the two keys.
- **Invariants upheld** — driven live under herdr (the repo has no shell test harness):

| Spec ref | Bound to | Signal |
| --- | --- | --- |
| `P1` toggle opens the configured placement | toggle with each `toggle_placement` value | the named placement opens |
| `P2` each key defaults independently | toggle with a garbage value in one key | that key falls back, no error |
| `P3` direction changes only split | `split` + `toggle_direction = down` | reviewr opens below the pane; inert for other placements |
| `P4` event opens only split/tab | `sidebar.sh open` with each placement | opens for split/tab, nothing for overlay/zoomed |
| `P5` event never takes focus | `sidebar.sh open` for split/tab | keyboard stays on the agent |
| `P6` toggle focus by placement | toggle split vs overlay/zoomed/tab | split keeps agent focus; others focus reviewr |
| `P7` one pane per workspace across a config change | toggle split, edit config to overlay, toggle | the split closes; a third toggle opens overlay (T1) |

## Replan Triggers

- If closing a `zoomed` reviewr leaves the agent pane maximized in a jarring way, then revisit the `zoomed` focus/close handling in the spec.
- If `herdr plugin pane close` on a `tab` root pane does not remove the tab cleanly, then adjust the close path and note it in the spec.

## Replan Log

- 2026-07-02: initial plan from approved contract.
- 2026-07-02: review → accept single-quoted TOML values and add a default `case` arm in `sidebar.sh` → landed in code, no contract change.
- 2026-07-02: QA passed live (all placements + direction + fallback + tab auto-open); `herdr-host.md` promoted Draft → Current.
