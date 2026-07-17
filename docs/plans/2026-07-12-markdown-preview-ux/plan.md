# Markdown preview UX — Plan

Delivers `specs/markdown.md#links`, the preview title marker and position mapping in `specs/diff-view.md`, and the click-link row in `specs/tui.md`.

## Goal

The preview names itself in the pane title, the source↔preview toggle carries the reading position, and links in rendered markdown open on click — safely, since PR bodies are attacker-controlled.

## Definition of Done

- [x] The File pane title carries `· preview` while the preview is open, and drops it in source.
- [x] `m` into the preview opens at the block holding the source cursor's line, or the nearest block above it.
- [x] `m` back to source puts the cursor on the top visible block's first source line, revealed.
- [x] A round-trip with no preview scroll input restores the exact source cursor and scroll. A refresh clamp does not count as scrolling.
- [x] The horizontal offset survives every return. A forced return keeps the prior source position.
- [x] Clicking a link in the preview or the PR read pane opens its destination in the browser, and reports `opened link in browser`.
- [x] The click target spans the link text and its dim destination, across wrapped rows.
- [x] A click resolves against the painted frame's geometry.
- [x] Only trimmed, case-insensitive `http://`/`https://` destinations open. Other schemes and destinations carrying control or bidi characters are silently inert and never reach the OS.
- [x] The element table's underline promise holds: link text renders underlined.

## Out of Scope

- Keyboard link navigation and OSC 8. Non-goals in `specs/markdown.md`.
- Position mapping for the PR read pane. It has no source view.

## Execution Plan

1. [x] `src/markdown.rs`: `render` returns lines plus per-line metadata — the block's first source line, and link spans as display-column ranges with their destination. `RenderCache` stores both. Unit tests: source-line mapping across blocks, wrapped-link spans, destination column ranges.
2. [x] `src/markdown.rs` (or `src/browser.rs`): `openable_url(&str) -> Result<&str, &'static str>` — trim, case-insensitive `http://`/`https://` prefix, reject control/bidi characters. Unit tests: `HTTP://`, `https:evil`, `javascript:`, leading whitespace, bidi-carrying destination.
3. [x] `src/app.rs`: entry alignment as a pending source-line `Cell` consumed at the next preview paint; exit mapping reads the cached metadata for the top visible line; exact-restore keeps a scrolled-since-entry flag that a refresh clamp never sets; forced return leaves the source position untouched.
4. [x] `src/ui.rs`: preview paint consumes the pending alignment and notes the painted link regions (pane-relative row, column range, url) in a frame `Cell`; the PR read pane notes its regions the same way; the File pane title gains `· preview`.
5. [x] `src/lib.rs` `handle_mouse`: a click in the diff or PR read pane checks the painted link regions first; a hit opens via `browser::open` behind `openable_url` and sets the status line; a miss falls through to today's behavior.
6. [x] Integration tests — `tests/render.rs`: title suffix in preview only; a painted link region resolves a click (both panes); `tests/app_flow.rs`: entry alignment, return mapping, unscrolled exact restore, clamp-not-scroll, forced return keeps position.

## Likely Files

| file                | change                                                    |
| ------------------- | --------------------------------------------------------- |
| `src/markdown.rs`   | per-line metadata, url guard, unit tests                  |
| `src/app.rs`        | pending alignment, exit mapping, scrolled-flag, title state |
| `src/ui.rs`         | title suffix, painted link regions, alignment consumption |
| `src/lib.rs`        | click → link-region hit → guarded open                    |
| `src/browser.rs`    | reuse `open`; no change expected                          |
| `tests/render.rs`   | title, click regions                                      |
| `tests/app_flow.rs` | mapping round-trips                                       |

## Verification

- `cargo test` → all green, including the new mapping and url-guard tests.
- `cargo clippy --all-targets` and `cargo fmt --check` → clean.
- Live run: preview a long README, toggle at a deep section both ways, click a link in a PR body, click a `javascript:` link in a crafted comment → silently inert, no browser.
- Tight: everything the diff adds is exercised by a DoD line.
- Gate: promote `markdown.md`, `diff-view.md`, `tui.md` to Current.

## Replan

- If pulldown-cmark block offsets prove too coarse for headings inside lists, then map on the outermost block only and note it in `diff-view.md`.
- 2026-07-12: the mapping runs synchronously in `toggle_preview` against the noted pane width (the render is memoized, so the paint reuses it) instead of a paint-consumed pending `Cell` — same contract, simpler state. Landed in steps 3–4.
- 2026-07-12: refused destinations went from a status-line report to silently inert, on user direction. Landed in `specs/markdown.md` Links.
- 2026-07-12: initial plan.
