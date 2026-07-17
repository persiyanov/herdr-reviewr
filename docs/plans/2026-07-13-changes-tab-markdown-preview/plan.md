# Changes-tab markdown preview — Plan

Delivers `specs/diff-view.md#markdown-preview`, `specs/markdown.md`, `specs/tui.md`.

## Goal

The `m` toggle previews a markdown file from the Diff view too, so a reviewer can read a doc rendered without leaving the Changes tab.

## Definition of Done

- [ ] `m` on a markdown file's diff in `Changes` opens the rendered preview, title suffixed `· preview`.
- [ ] `m` in the preview returns to the diff with cursor, scroll, and folds untouched.
- [ ] Entry aligns to the block of the cursor's current-content line. A deletion or fold row aligns by the nearest row above with one, or the top.
- [ ] A deleted markdown file's toggle is inert, and its footer shows no `m preview`.
- [ ] The preview holds across a same-file refresh and a scope switch. A degrade to a notice force-returns to source.
- [ ] The preview choice stays per tab.

## Out of Scope

- Position carry from the preview back into the diff. Rejected in brainstorming, recorded in the PR description.

## Execution Plan

1. [ ] `src/app.rs` `set_diff`: on a different file, reset the preview fields as `set_file_view` does. Fill `preview_text` from the new side when the file is markdown and `state` is `Normal`, clear it otherwise.
2. [ ] `src/app.rs` `preview_active` and `toggle_preview`: replace the `Tab::AllFiles` gate with a file-tab gate plus non-empty `preview_text`.
3. [ ] `src/app.rs` `align_preview_to_cursor`: target the cursor row's new-side line, falling back to the nearest row above with one, then to the top. File view rows keep working through the same lookup.
4. [ ] `src/app.rs` `return_from_preview`: skip the block-to-cursor mapping in `Changes`.
5. [ ] `src/ui.rs` footer: key the `m preview` hint on previewability, not the tab.
6. [ ] Flip the two `Changes never previews` assertions in `tests/app_flow.rs` (`the_markdown_preview_toggles_only_on_markdown_files_in_all_files`, `a_tab_switch_restores_the_preview_choice`) to the new contract.
7. [ ] New tests in `tests/app_flow.rs`: entry alignment from a deletion row and a leading fold, exact restore on return, deleted-file inertness, scope-switch hold, per-tab independence.
8. [ ] New test in `tests/render.rs`: no `m preview` in the footer on a deleted markdown file's diff line.

## Likely Files

| file                | change                                                  |
| ------------------- | ------------------------------------------------------- |
| `src/app.rs`        | preview gates, `set_diff` render input, entry alignment |
| `src/ui.rs`         | footer hint keyed on previewability                     |
| `tests/app_flow.rs` | flipped and new toggle/lifecycle tests                  |
| `tests/render.rs`   | footer hint on a deleted markdown file                  |

## Verification

- `cargo test` → all green, including the flipped assertions.
- Live run in a repo with an edited `README.md`: `m` from `Changes` renders it, `m` returns to the unchanged diff position.
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- Gate: xhigh full-branch code review, then promote `diff-view.md`, `markdown.md`, `tui.md` to Current.

## Replan

- If the scope-switch reset code clears preview state in a way the hold cannot ride, then surface it before working around it.
- 2026-07-13: initial plan.
