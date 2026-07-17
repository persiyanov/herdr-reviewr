# Customizable keybindings — Plan

Delivers `specs/config.md#keybindings` and the `specs/tui.md` Interaction keymap (issue #12).

## Goal

Users rebind the character shortcuts per action through `[keybindings]`, with multiple keys per action. This unblocks CJK IME users, who alias the character their layout produces on the same physical key.

## Definition of Done

- [x] `comment = ["c", "ㅊ"]` fires the comment action on both keys, in every context the action fires.
- [x] A bound action answers only its configured keys: `send = ["x"]` makes `s` and `S` inert.
- [x] An unbound action keeps its default keys.
- [x] An invalid `[keybindings]` value blocks the whole file per C3–C5: unknown action name, empty array, non-array value, K1-invalid key, or a duplicated character (across actions or within one array).
- [x] A collision error names each action involved.
- [x] The fixed keys act regardless of bindings (K3).
- [x] Footer and header hints show each action's first bound key, tab digits included.
- [x] The comments list closes on `esc` and the `comments` binding. `q` is inert there.
- [x] A blocked sidebar answers only the default `q`.
- [x] A keymap change applies on the next refresh. The frame on screen and the keys it answers use one snapshot.

## Out of Scope

- Modifier, named-key, and sequence notation. `specs/tui.md` non-goals.
- Replying on issue #12. The PR does that at merge.

## Execution Plan

1. [x] `src/keymap.rs` (new): an `Action` enum for the 23 keymap actions, the default keymap, resolution of config bindings over defaults, and a char → `Action` lookup. K1 and K2 validation with errors naming each action.
2. [x] `src/config.rs`: parse the `[keybindings]` table in `parse_plugin_config`, reject unknown action names (C3) and invalid values (C4), and carry the resolved keymap on `PluginConfig`.
3. [x] `src/app.rs`: `App` exposes the active keymap from its `PluginConfigState`, defaults while `Blocked`.
4. [x] `src/lib.rs` `handle_key`: dispatch the character shortcuts through the keymap in all three contexts (main panes, `Mode::List`, `Tab::Pr`). Fixed keys stay hardcoded. Drop `q` from the list-overlay closers. Confirm the blocked screen (`lib.rs:534`) answers the literal `q` only.
5. [x] `src/ui.rs`: `action_key_label` and the header `TAB_LABELS` render each action's first bound key. Composite glyphs (`u/b/t`, `n/N`, `1·2·3`) build from the bindings.
6. [x] Tests, inside this milestone: parse cases in `src/config.rs`, dispatch and hint cases in `tests/app_flow.rs` and `tests/render.rs`.

## Likely Files

| file                 | change                                                      |
| -------------------- | ----------------------------------------------------------- |
| `src/keymap.rs`      | new: actions, defaults, resolution, validation, lookup       |
| `src/config.rs`      | parse and validate `[keybindings]`, resolved keymap on config |
| `src/app.rs`         | active keymap accessor, default keymap while blocked          |
| `src/lib.rs`         | keymap-driven dispatch in `handle_key`                        |
| `src/ui.rs`          | hints from bindings: footer labels, header tab digits         |
| `tests/app_flow.rs`  | dispatch under rebinding, fixed keys, overlay closers         |
| `tests/render.rs`    | footer and header hints under rebinding                       |

## Verification

- `cargo test` → green, new cases included.
- K1 → `key_is_one_printable_codepoint` → multi-codepoint, whitespace, and control keys each invalidate the file.
- K2 → `collision_names_each_action` → a cross-action and a within-array duplicate each invalidate the file, error naming the actions.
- K3 → `fixed_keys_survive_rebinding` → arrows, `tab`, and `esc` act with every character shortcut rebound.
- Live: run the sidebar with `comment = ["c", "ㅊ"]`, press `ㅊ` on a diff line → the composer opens (the issue #12 scenario).
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- Gate: xhigh full-branch review, then promote `specs/config.md` and `specs/tui.md` to Current.

## Replan

- If terminals hold the IME jamo in preedit so the alias fires only on commit, document the terminal dependency in the README and on the issue. No design change.
- 2026-07-12: initial plan.
- 2026-07-12: review sweep → the normalized JSON output gains the resolved `keybindings`; the Out of Scope line excluding it is dropped (the "every key" test was silently pinning the omission).
