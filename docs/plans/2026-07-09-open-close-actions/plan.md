# Open and close actions — Plan

Delivers `specs/herdr-host.md#sidebar-actions` (issue #9).

## Goal

Layout plugins and scripts open and close the reviewr sidebar deterministically, without toggle semantics. The sidebar's identity moves from a state file to the live `reviewr` pane label, which also heals duplicates and survives herdr restarts.

## Definition of Done

- [x] `herdr plugin action invoke open --plugin persiyanov.reviewr` opens the sidebar, and does nothing when one is open (A1).
- [x] `open` opens with `auto_open = false` and with any configured placement (A2).
- [x] `close` removes every `reviewr`-labeled pane, including one herdr's plugin registry forgot (A7).
- [x] `toggle` works with no state file read or written, and closes a sidebar the event or a script opened (A5).
- [x] A refused action exits 1 with one stderr line. A success prints the pane it acted on (A4).
- [x] The `worktree.created` event behavior is unchanged (P4, P5, P8).
- [x] `README.md` documents the actions and the layout-plugin pattern. `CHANGELOG.md` has the entry.
- [x] Version is 0.10.0 in `Cargo.toml` and `herdr-plugin.toml`. `cargo test` and `cargo clippy` are green.

## Out of Scope

- Per-invocation placement or workspace targeting. herdr has no argument channel on invoke. The spec's failure semantics own the boundary.
- Cleanup of legacy `pane-*` state files. They are inert once nothing reads them.
- The reply on issue #9. It goes out with the release.

## Execution Plan

1. [x] `herdr/sidebar.sh`: replace the state-file check with a label query over `herdr pane list --workspace`. Modes `toggle`, `open`, `close`, and `auto-open` (the event). Close via `herdr pane close`, sweeping every labeled pane. Refusals to stderr with exit 1, successes to stdout.
2. [x] `herdr-plugin.toml`: add the `open` and `close` actions. Point the event at the `auto-open` mode.
3. [x] Live verification battery in this workspace (`w1X`), driving the script directly with `HERDR_WORKSPACE_ID` set: each DoD line, plus one real `herdr plugin action invoke` end-to-end.
4. [x] `README.md`: the three actions, the keybind names, the layout-plugin pattern with the invoke-while-focused caveat.
5. [x] `CHANGELOG.md` Unreleased entry citing `specs/herdr-host.md` and #9. Version bump in `Cargo.toml` and `herdr-plugin.toml`.

## Likely Files

| file                  | change                                              |
| --------------------- | ---------------------------------------------------- |
| `herdr/sidebar.sh`    | label discovery, four modes, loud refusals           |
| `herdr-plugin.toml`   | two new actions, event mode rename, version          |
| `README.md`           | actions section, layout-plugin pattern               |
| `CHANGELOG.md`        | Unreleased entry                                     |
| `Cargo.toml`          | version 0.10.0                                       |
| `specs/herdr-host.md` | promote to Current at the gate                       |

## Verification

The change is a shell script and manifest, so verification is live against the running herdr, driven from this pane's workspace. `cargo test` guards the untouched binary.

- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- Gate: promote `specs/herdr-host.md` to Current.

| spec ref | bound to                                                        | signal                                  |
| -------- | ---------------------------------------------------------------- | ---------------------------------------- |
| A1       | `open` twice in `w1X`                                            | one pane after both, second exits 0      |
| A2       | `open` with `auto_open = false` in config                        | the sidebar opens                        |
| A3       | `open` with `split` placement                                    | the agent pane keeps `focused: true`     |
| A4       | `open` with no workspace env, and in a non-repo cwd              | exit 1, one stderr line each             |
| A4       | successful `open`                                                | stdout names the new pane id             |
| A5, A7   | a second labeled pane made via raw `plugin pane open`, then `close` | both panes gone                       |
| P4, P8   | `auto-open` mode with `zoomed` placement, and with `auto_open = false` | nothing opens                      |
| A6, T4   | `invoke open` for real, then the toggle key                      | opens in the focused workspace, then closes by hand |

## Replan

- If the `reviewr` label proves unreliable for any placement in live use, reopen the state-model decision in the spec via brainstorming.
- 2026-07-09: initial plan from the approved contract.
- 2026-07-09: the installed plugin is a GitHub release, not a link of this checkout → the end-to-end `invoke open` check (A6, T4) waits for the release, the env-driven battery covers the rest → Verification.
- 2026-07-09: linked the dev checkout instead → the end-to-end invoke check ran and passed (open no-op, close, open), nothing waits for the release → Verification.
