# Search — Plan

Delivers `specs/search.md`, plus the `search` action in `specs/input.md`.

## Problem

A reviewer reading an agent's diff has no way to find anything by name: not a file, not a function, not a string. Checking how a called function is defined means leaving the pane, grepping elsewhere, and losing the review place. The pane is a long-running process sitting on the worktree, which is exactly the shape a warm search index wants.

## Goal

`/` on `All files` opens a search overlay: fuzzy file and content results from the `fff-search` engine, pick one, land in the read pane on that file or line.

## Definition of Done

- [x] `/` on `All files` opens the overlay. Typing paints `Files` and `Code` groups in engine order.
- [x] Queries run off the frame loop. A superseded result set never paints.
- [x] `enter` on a file result opens it, moves the navigator selection, expands ancestors. On a code result the read-pane cursor lands on the hit line, clamped.
- [x] `esc` closes the overlay with tab, selection, scroll, and focus untouched.
- [x] `/` on `Changes` and `PR` shows the footer status naming where search lives.
- [x] The overlay shows `indexing…` until the engine is warm, and matched-span emphasis on code rows after.
- [x] Ignored files and `.git` never appear in results.
- [x] The frecency store lands under the cache directory. The worktree gains no file, ever.
- [x] A config error closes the overlay. Recovery restores the tab without the query.
- [x] `scripts/bench_tui.py` medians are unchanged A/B against the pre-branch binary.

## Out of Scope

- Search on `Changes` or `PR`, and in-diff search. Roadmap (`specs/overview.md`).
- Symbol tables. The engine's definition ranking is the whole story (`specs/search.md` Non-goals).

## Execution Plan

1. [x] Add `fff-search = { version = "0.10", features = ["definitions"] }`. Confirm the frecency store path is redirectable to a cache dir.
2. [x] Extend the `deny.toml` license allowlist for the fff tree: `CC0-1.0` (notify), `BSL-1.0` (xxhash-rust), `MPL-2.0` (option-ext, unmodified file-level copyleft). `cargo deny check` green.
3. [x] `src/search.rs`: a search worker owning the `FilePicker` (content indexing on, watcher on), request/completion channels with latest-wins generations, mirroring `src/world.rs`. The picker spawns on first overlay open.
4. [x] `src/app.rs`: overlay state (query, caret, pick, scroll) as a new `Mode`, key routing (printable → query, `↓`/`↑`/`enter`/`esc`), and result landing: open by path, cursor to clamped line, selection and ancestors reconciled.
5. [x] `src/lib.rs`: wire search completions into the event loop beside `land_world_completion`, discarding stale generations.
6. [x] `src/ui.rs`: the centered overlay — input line, two groups, file rows in the file-list look, code rows `path:line` + line with span emphasis, `… N more`, `indexing…`, and the overlay footer. The `Changes`/`PR` footer nudge.
7. [x] Config-error path: close the overlay, drop the query (`src/lib.rs` config takeover).
8. [x] Tests in `tests/app_flow.rs` and `tests/render.rs`, per Verification.
9. [x] Bench A/B per `AGENTS.md`, interleaved runs, medians compared.

## Likely Files

| file                 | change                                                        |
| -------------------- | ------------------------------------------------------------- |
| `Cargo.toml`         | `fff-search` dependency, `definitions` feature                |
| `src/search.rs`      | new: search worker, channels, generation tags                 |
| `src/app.rs`         | overlay mode, key routing, result landing                     |
| `src/lib.rs`         | completion landing, config-takeover close                     |
| `src/ui.rs`          | overlay render, footer nudge                                  |
| `tests/app_flow.rs`  | flow tests: supersede, landing, esc, config error             |
| `tests/render.rs`    | overlay snapshot: groups, emphasis, `indexing…`               |

## Verification

- `just ci` → green.
- `superseded_result_never_paints` → a completion tagged with an old generation lands nowhere.
- `open_lands_on_clamped_line` → a code result whose file shrank opens at the last line.
- `esc_restores_place_untouched` → tab, selection, scroll, focus identical before `/` and after `esc`.
- `engine_worker_end_to_end` → real engine run: results arrive, ignored/`.git` excluded, worktree file set identical after index build and a pick (O1), frecency store under the cache dir.
- `config_error_closes_overlay_and_drops_query` → config takeover with the overlay open, recovery restores the tab, query gone.
- `python3 scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture` A/B → medians within noise of the pre-branch binary.
- Tight: everything the diff adds is exercised by a DoD line.
- Gate: promote `specs/search.md` and `specs/input.md` to Current.

## Replan

- If the frecency store cannot leave the repo's vicinity, disable frecency and edit `specs/search.md` (ranking loses the improves-with-use line).
- If first-open index build noticeably delays a large repo's first results, move picker spawn to startup, behind the paint.
- 2026-07-18: initial plan. Spike proved `fff-search` 0.10 on macOS: 15ms scan, sub-ms file search, sub-6ms grep on this repo.
- 2026-07-18: main added a cargo-deny gate → license pre-check run against the fff tree → step 2.
