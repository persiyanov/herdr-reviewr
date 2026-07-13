# Markdown rendering — Plan

Delivers `specs/markdown.md`, plus the consuming edits in `specs/diff-view.md` (File view preview), `specs/tui.md` (PR bodies, description row, `preview` binding), and `specs/forge-host.md` (`body` snapshot field).

## Goal

PR comment bodies and the PR description render as styled markdown instead of raw text. A markdown file in `All files` gains a read-only rendered preview on `m`.

## Definition of Done

- [x] A PR comment body renders per the element table in `specs/markdown.md`. A finding's `snippet` stays plain `+`/`−` lines.
- [x] A non-empty PR description pins a `description` row first in the comments list. Its body renders in the read pane. An emptied description vanishes and clamps the cursor.
- [x] The snapshot carries `body`, read in the direct PR query.
- [x] `m` on a `.md`/`.markdown` file in `All files` toggles a rendered preview. It is read-only, scrolls by line, has no gutter, and survives refreshes with its scroll clamped.
- [x] Returning to source restores its cursor, scroll, and horizontal offset. Entering the preview clears a live selection.
- [x] The preview choice is tab state and holds across refreshes of the same file. Opening a file starts in source.
- [x] `preview` is a rebindable action, default `m`. K1–K3 hold.
- [x] Fenced code highlights through the existing `Highlighter`. Every color comes from the active palette.
- [x] A table renders as aligned columns, or as its source text when wider than the pane.
- [x] Control characters and bidi overrides render as visible placeholders.
- [x] The footer shows `m source` while a preview is open. The action bar never offers comment keys there.

## Out of Scope

- Commentable preview, clickable links, rendered comment cards, PR-anchor navigation. Non-goals in `specs/markdown.md` and `specs/tui.md`.
- Table layout beyond the degrade rule.

## Execution Plan

1. [x] Add `pulldown-cmark` to `Cargo.toml`.
2. [x] New `src/markdown.rs`: event walker → styled, pre-wrapped lines. Covers the element table, wrapping with hanging indents, tables with the degrade rule, nesting cap, control-character and bidi sanitization, render cache. Unit tests per element.
3. [x] `src/keymap.rs`: add `Action::Preview` (`"preview"`, `m`). Extend the keymap tests.
4. [x] `src/forge.rs`: fetch `body` in the direct PR query, add it to the snapshot. Parse test.
5. [x] `src/ui.rs` PR pane: render selected bodies through `markdown::render`, keep `snippet` plain. Description row pinned first in the nav list, read pane shows it.
6. [x] `src/app.rs` + `src/ui.rs` File view: preview state (flag, scroll) on the All files tab, toggle semantics, selection clear, source-position restore, refresh clamp, footer context.
7. [x] Integration tests in `tests/render.rs` and `tests/app_flow.rs`: toggle round-trip, tab-state restore, refresh scroll clamp, description row lifecycle.

## Likely Files

| file                 | change                                              |
| -------------------- | --------------------------------------------------- |
| `Cargo.toml`         | add `pulldown-cmark`                                 |
| `src/markdown.rs`    | new: the renderer                                    |
| `src/keymap.rs`      | `Action::Preview`                                    |
| `src/forge.rs`       | `body` in query, snapshot, parse                     |
| `src/app.rs`         | preview state, toggle, footer actions                |
| `src/ui.rs`          | PR-pane markdown, description row, preview pane      |
| `tests/render.rs`    | preview and PR-pane rendering                        |
| `tests/app_flow.rs`  | toggle, tab state, refresh, description lifecycle    |

## Verification

- `cargo test` → all green, including the new element, toggle, and lifecycle tests.
- `cargo clippy --all-targets` and `cargo fmt --check` → clean.
- Live run in a worktree with an open PR: bot comment with headings/code/links renders styled, `m` on `README.md` previews and round-trips.
- Tight: everything the diff adds is exercised by a DoD line. Delete or defer the rest.
- K2 (`config.md`): `m` unique across the default keymap → existing collision test extended.
- Gate: promote `markdown.md`, `diff-view.md`, `tui.md`, `forge-host.md` to Current.

## Replan

- If pulldown-cmark's event stream can't carry table cell boundaries cleanly, then render tables from source-line scanning instead.
- 2026-07-12: initial plan.
