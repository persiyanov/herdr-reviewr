# Rebindable expand, collapse, and page keys: Plan

Delivers `specs/input.md` and `specs/config.md#keybindings` (supersedes PR #68).

## Problem

Expand/collapse and the page keys are the only cursor operations with no rebindable action, so a home-row reviewer must reach for the arrows. Contributor PR #68 is the evidence: it built `expand`/`collapse` bindings but broke arrow sideways-scroll and the config round-trip, and left an abandoned `h`/`l` default in the spec.

## Goal

Named keys (`left`, `right`, `up`, `down`, `pageup`, `pagedown`) join the `[keybindings]` grammar, and six new actions (`expand`, `collapse`, `page-up`, `page-down`, `half-up`, `half-down`) make every non-structural key rebindable. Defaults stay byte-identical to today.

## Definition of Done

- [ ] `[keybindings]` accepts the six named keys, bare or behind `ctrl+`/`alt+`, and the six new action names.
- [ ] A default config behaves exactly as today: arrows fold on a collapsible and scroll sideways elsewhere, `PageUp`/`PageDown` page, `ctrl+u`/`ctrl+d` half-page. Existing tests pass unmodified.
- [ ] `expand = ["l"]` plus `comments = ["L"]` folds directories and opens folds from `l`; the freed `right` key answers nothing.
- [ ] `expand = ["l"]` alone fails whole-file, the error naming `expand`, `comments`, and `l`.
- [ ] `--resolve-plugin-config` emits named keys by name, and its keybindings re-parse as a valid config (round-trip test).
- [ ] Footer and header hints follow rebinds and paint screen labels: `→ expand` by default, `l expand` after the rebind; the `move` band's page hints follow `page-up`/`page-down`.
- [ ] The comments list and agent picker follow a `down`/`up` rebind; the base picker, comment editor, and search/find inputs are unaffected by any rebind.
- [ ] `just ci` green; `bench_tui.py` medians unchanged A/B (footer render path touched).

## Out of Scope

- `home`/`end`/`shift+` key names. The grammar admits them additively later.
- The comments-list "acts through the same bindings" scope ambiguity. Pre-existing; raised at the design gate for a separate pass.
- Persisting any state. `[keybindings]` remains config-only.

## Execution Plan

1. [ ] `src/keymap.rs`: add `KeyCode { Char, Left, Right, Up, Down, PageUp, PageDown }`; `Key.ch` becomes `Key.code` (salvages PR #68's shape). `config_str` spells names, new `label()` paints `←` `→` `↑` `↓` `PageUp` `PageDown`.
2. [ ] `src/keymap.rs`: `ACTIONS` 34 → 40 — `expand`/`collapse` default `right`/`left`, `page-up`/`page-down` default `pageup`/`pagedown`, `half-up`/`half-down` default `ctrl+u`/`ctrl+d`; `down`/`up` defaults become `[j, down]`/`[k, up]`. Unit tests: defaults bind every action, hints are `j`/`→`/`PageUp`.
3. [ ] `src/config.rs`: `parse_key` accepts the six names after the `ctrl+`/`alt+` prefix split; collision error names both actions and the shared key (generalize the upgrade-case wording). Tests: named-key parse, `pageup = ["PageUp"]` rejected, JSON round-trip.
4. [ ] `src/lib.rs` `handle_key`: map `Left`/`Right`/`Up`/`Down`/`PageUp`/`PageDown` key events into keymap lookups beside `Char`; add `Normal`-mode arms — `Expand`/`Collapse` (directory → fold → `scroll_h(±8)`), `PageUp`/`PageDown`/`HalfUp`/`HalfDown` → `move_cursor`; delete the hardcoded arrow, page, and `ctrl+u`/`ctrl+d` fallthrough arms. `PR`-tab page scroll switches to the page actions, per-focus behavior kept.
5. [ ] `src/ui.rs`: `action_key_label` — `ExpandDir`/`CollapseDir` use `hint(K::Expand/K::Collapse)`, `MovePage` uses the page-action hints; hints render via `label()`. `src/forge.rs` `retry_remedy` hint renders via `label()`.
6. [ ] `tests/app_flow.rs`: drive the DoD scenarios through `handle_key` — default arrows fold and scroll (the regression PR #68 shipped), the `l`/`L` rebind, dead freed arrows, picker-follows-rebind, base-picker unaffected.
7. [ ] `CHANGELOG.md` bullet under `## [Unreleased]`; README key table already shows defaults, only the `→ ←` row wording gains "(rebindable as `expand` / `collapse`)".
8. [ ] Bench A/B against a rebuilt main binary on a quiet system; record beside `scripts/bench-results/`.

## Likely Files

| file                | change                                              |
| ------------------- | --------------------------------------------------- |
| `src/keymap.rs`     | `KeyCode`, 6 new actions, label vs config spelling  |
| `src/config.rs`     | named-key grammar, collision error, round-trip test |
| `src/lib.rs`        | route named keys through the keymap, delete arms    |
| `src/ui.rs`         | hint-driven footer labels                           |
| `src/forge.rs`      | hint rendering in `retry_remedy`                    |
| `tests/app_flow.rs` | DoD scenarios end to end                            |
| `CHANGELOG.md`      | Unreleased bullet                                   |
| `README.md`         | one row wording                                     |

## Verification

- `just ci` → green.
- `cargo test --test app_flow` → the step-6 scenarios pass.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` A/B → medians within noise.
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- `CFG-WHOLE-FILE` → the `expand = ["l"]`-alone test → whole-file error, no partial keymap.
- Gate: promote `specs/input.md`, `specs/config.md`, and `specs/pr-tab.md` to Current; user QAs via `just qa-install` before merge; land as PR #68 itself, thanking the author at merge.

## Replan

- If terminals deliver `ctrl+`-modified named keys unreliably (crossterm), then keep the grammar but document delivery as terminal-dependent, like the comment editor's modified arrows.
- 2026-08-17: user directive → land on PR #68's branch (maintainer edits allowed), keeping the author's commits and contributor credit → gate line updated.
- 2026-08-17: initial plan.
