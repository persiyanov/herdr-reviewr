# Comment editor — Delivery Plan

**Specs:** ../../../specs/tui.md — the `### Comment editor` section this plan delivers.

## Milestone Map

1. **Comment editor** — single milestone; the inline comment box becomes a real caret-driven text field (move/insert/delete anywhere, word ops, bracketed paste, placeholder).

## Goal

Replace the append-only comment input with an ordinary text field: a caret you move and edit at, multi-line paste that lands intact, and a placeholder when empty — per `tui.md` › Comment editor.

## Definition of Done

- The caret moves (`←`/`→`, `↑`/`↓` across wrapped rows, `Home`/`End`+`Ctrl+A`/`Ctrl+E` on the logical line, word-jump on `Alt`/`Ctrl`+`←`/`→`) and renders as a block at its position.
- Text inserts/deletes **at the caret**: typing, `Backspace`/`Delete`, `Ctrl+W` (word back), `Ctrl+U`/`Ctrl+K` (kill to logical-line start/end), newline keys.
- A multi-line paste inserts at the caret as one unit (`\r\n`/`\r` → `\n`); a paste outside the editor is ignored.
- An empty box shows the `Leave a comment…` placeholder; `e` preloads the text with the caret at the end.
- The caret/draft survive a poll; `cargo test`, `clippy -D warnings`, `fmt` clean.

## Exit State

A **closed** list — anything not named is not built. Delivers the whole `tui.md` Comment editor section.

- `src/app.rs` — `App.caret: usize` (char index into `input`). Caret-aware input ops replacing the append-only ones: `input_insert(char)`, `input_paste(&str)`, `input_backspace`, `input_delete_forward`, `input_delete_word`, `input_kill_to_start`, `input_kill_to_end`. Caret movement: `caret_left`/`caret_right`, `caret_home`/`caret_end` (logical line), `caret_word_left`/`caret_word_right`, `caret_up`/`caret_down` (take the wrap width). `start_comment` sets `caret = 0`; `start_edit` sets `caret = input.chars().count()`.
- `src/lib.rs` — composing keymap extended with the movement + delete keys; `run()` enables bracketed paste (and disables it on teardown); the event loop routes `Event::Paste` to `input_paste` while composing; `handle_key` gains `area` (to derive the composer width for `↑`/`↓`), mirroring `handle_mouse`.
- `src/ui.rs` — `composer_lines` renders the caret block at the caret's mapped (row, column) instead of always at the end, and the empty-state placeholder. A shared char-index ↔ wrapped-(row, col) mapping over the existing `wrap_text`, used by both the renderer and `caret_up`/`caret_down`.
- No change to `Comment`, `CommentStore`, or `review-model.md` — the caret is transient UI state, never stored.

## Specs Touched

| Spec | What this plan realizes | At the gate |
| --- | --- | --- |
| `tui.md` | the whole Comment editor section | Draft → Current |

## Out of Scope

- Selection, cut/copy, undo/redo, markdown rendering, click-to-place-caret — `tui.md` non-goals; not built.

## Likely Files

- `src/app.rs` — caret field + caret-aware input/movement ops; start_comment/start_edit caret init.
- `src/lib.rs` — composing keymap, bracketed-paste enable + `Event::Paste`, `area` into `handle_key`.
- `src/ui.rs` — caret-at-position render, placeholder, the caret↔wrapped-position mapping.
- `tests/app_flow.rs` — caret movement/edit/paste unit tests.
- `tests/render.rs` — caret-block-at-position + placeholder render tests.

## Execution Plan

1. **Caret model + the wrap mapping (the shape-risk, first).** Add `caret`; build the char-index ↔ wrapped-(row, col) mapping over `wrap_text`; wire `caret_up`/`caret_down` and the caret-at-position render. Prove `↑`/`↓` land on the right column across wrapped rows before the rest.
2. Caret-aware edits: `input_insert`/`backspace`/`delete_forward`/`delete_word`/`kill_to_start`/`kill_to_end`; caret init in start_comment/start_edit.
3. Caret movement: left/right, home/end, word-left/right; the composing keymap; thread `area` into `handle_key`.
4. Bracketed paste: enable in `run()`, route `Event::Paste` → `input_paste` (normalize newlines), ignore outside composing.
5. Placeholder render when `input` is empty.
6. Tests (unit + render) and the live pass.

## Verification

- **Done:** `cargo test`; live in pane `w8:pS` — type mid-text and fix a typo without deleting the tail; `←/→/↑/↓/Home/End/Ctrl+A/E/W/U/K` behave; paste a multi-line snippet (no early submit); empty box shows the placeholder.
- **Tight:** the diff equals Exit State — only the named ops/field/render; no selection/undo/markdown surface, no `Comment` change.
- **Invariants upheld:**
  - caret is character-wise — unit tests with multi-byte/wide input assert no panic and correct positions.
  - draft + caret survive a poll (`tui.md` failure semantics) — test: set caret mid-text, `reload`, caret/text unchanged.
  - paste outside the editor is ignored (`tui.md`) — `Event::Paste` in Normal mode is a no-op.

## Replan Triggers

- If `Event::Paste` does not survive the herdr multiplexer (no paste event arrives), keep the caret work and record the paste limitation in `tui.md` rather than faking it with raw keystrokes; revisit with an explicit paste key reading the clipboard.
- If the caret↔wrapped-position mapping proves to need geometry the renderer computes per-frame, cache the composer width on `App` (set in the event loop) instead of threading `area` through `handle_key`.

## Replan Log

- 2026-06-25: initial plan from the approved `tui.md` Comment editor Draft. Single milestone (cohesive single-area feature, no commitment boundary); paste-delivery unknown is a replan trigger, not a cut, since nothing builds on it.
