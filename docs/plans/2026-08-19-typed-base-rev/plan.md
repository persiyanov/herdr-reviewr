# Typed base revision: Plan

Delivers `specs/review-model.md` Base branch, `specs/input.md` Base picker, and `specs/tui.md` header paint (issue #75).

## Problem

The picker stores a typed `HEAD~1` or tag as a frozen object id. A branch picked from the same list still follows. The header then shows only the hash, so `qa-pin-base` and `HEAD~1` are indistinguishable from a SHA and do not mean what git means.

## Goal

The pick is the spelling the reviewer named. It re-resolves like git. The header is `vs HEAD~1 (a1b2c3d)` unless the spelling is already that SHA. A SHA is a pin because it is the spelling.

## Definition of Done

- [x] Typing `HEAD~1` stores `HEAD~1`. The header reads `vs HEAD~1 (` plus the abbreviated SHA plus `)`. A later commit still diffs one back, and the hash in the header moves.
- [x] A tag stores the tag name and re-resolves. A unique short SHA stores that short spelling and paints once, `vs a1b2c3d`.
- [x] A listed branch still follows. Choosing the default row still clears.
- [x] The probe row is the typed spelling, marked `(a1b2c3d)` unless the spelling is already a prefix of that SHA. Enter writes the spelling, not the object id.
- [x] Reopening with a non-branch pick highlights a row named with that spelling.
- [x] A spelling that does not resolve is skipped as `· HEAD~1 missing` and reactivates when it resolves again.
- [x] `--base HEAD~1` paints the same `vs HEAD~1 (a1b2c3d)` form.
- [x] README Base branch names live spellings and that a SHA is the freeze.

## Out of Scope

- PR-target auto-follow. Open decision in `specs/review-model.md`.
- Origin-vs-local order. Open decision in `specs/review-model.md`.
- Listing tags, reflog, or recent commits. Non-goal in `specs/input.md`.
- Changing default-row-clears, the popup title, or miss copy.
- Rewriting 40-hex picks already on disk. They remain SHA spellings and stay pins.
- Release packaging.

## Execution Plan

1. [x] `src/git.rs`: `read_base_pick` admits one printable line that does not start with `-`. `HEAD~1`, a tag, and a short SHA are picks. Control bytes are still none. Rewrite `a_pick_git_could_never_have_written_is_no_pick` in `tests/git_repo.rs` so `main~5` is a spelling.

2. [x] `src/git.rs`: a branch pick uses origin then local; anything else (`HEAD~1`, a tag, a SHA) uses `resolve_commit`. `ResolvedBase.name` is the stored spelling, never the abbreviated oid. `--base` keeps the stripped spelling as the name when it is not a branch. Tests in `tests/git_repo.rs`: write `HEAD~1`, a new commit moves the merge-base, a unique short SHA still resolves to that object, a tree-ish is skipped.

3. [x] `src/app.rs`: `run_base_probe` names the Hit row with the query spelling and keeps `oid` for the marker. `base_picker_pick` writes `choice.name` for every non-default row. `open_base_picker` inserts the current pick when it is not a branch, using the stored spelling. Tests in `tests/app_flow.rs` replace pin-oid assertions with spelling assertions.

4. [x] `src/ui.rs`: `base_label` / `base_parts` paint `vs HEAD~1 (a1b2c3d)`, paint a SHA spelling once, and clip the spelling before `(sha)`. `base_trail` shows `(a1b2c3d)` on a non-branch row whose name is not a prefix of its oid. Tests in `tests/render.rs` beside the named-rev paint.

5. [x] `README.md` Base branch: typed revs keep their names and re-resolve. A SHA is the freeze.

6. [x] Tests in the same commits as the code they check. `just ci`. QA: `just qa-install`, type `HEAD~1` and `qa-pin-base`, commit, confirm the names stay and the hashes move.

## Likely Files

| file                | change                                          |
| ------------------- | ----------------------------------------------- |
| `src/git.rs`        | pick shape, pick arm name, `--base` name        |
| `src/app.rs`        | probe row spelling, write name, current-pick row |
| `src/ui.rs`         | `vs HEAD~1 (sha)`, trail marker, clip           |
| `README.md`         | Base branch                                     |
| `tests/git_repo.rs` | live `HEAD~1`, short SHA, skip                  |
| `tests/app_flow.rs` | store spelling, reopen row                      |
| `tests/render.rs`   | header paint                                    |

## Verification

- `cargo test --test git_repo` → `HEAD~1` stored as `HEAD~1`, merge-base moves after a commit, a 40-hex pick still pins, a tree-ish is skipped.
- `cargo test --test app_flow` → type `HEAD~1` writes `HEAD~1`, probe row is that spelling, reopen highlights it, default-row clear unchanged.
- `cargo test --test render` → `vs HEAD~1 (`sha`)`, SHA-once, branch `vs main` unchanged.
- No-writes: the persistence test asserts the only new ref is `refs/reviewr/base-pick`.
- `just ci` green.
- Tight: everything the diff adds is exercised by a DoD line.
- [x] Gate: promote `specs/review-model.md`, `specs/input.md`, `specs/tui.md` to Current.

## Replan

- If a live `HEAD~1` header without the hash is unreadable in QA, do not pin. Keep `(sha)` as the witness.
- 2026-08-19: store the named spelling and re-resolve. A SHA is the only pin. Replaces the pin-on-type plan.
- 2026-08-19: probe only when the filter matches no row, after typing pauses. Enter with no row checks immediately.
- 2026-08-19: initial plan (pin typed revs to a SHA).
