# Diff viewer — visual polish backlog

Tuning and polish within the current stack (ratatui + syntect/two-face + Catppuccin). Not design changes — knobs and small additions. Fold into milestone 2 or a dedicated polish pass. ⭐ marks high niceness-per-effort.

## Structural palette (constants in `src/ui.rs`)

- ⭐ Retune the add/remove row tints (`DEL_BG`, `INS_BG`) — current values read muddy; align to Catppuccin diff conventions so changes pop without shouting. Tune live.
- ⭐ Emphasize the current line's number (brighter/bold), the way editors do.
- Swap/tune the change-bar colors, cursor-line bg (surface1), selection bg (surface0).
- Dim unchanged context slightly, or brighten changes, so the eye lands on the diff.

## Chrome / borders

- ⭐ Rounded borders (`BorderType::Rounded`) instead of square.
- Border + title colors in Catppuccin tones (focused vs unfocused); inner padding; a subtle header bar (mantle/crust) instead of the bright cyan.

## Nerd Font glyphs (terminal already uses JetBrainsMono Nerd Font)

- ⭐⭐ File-type icons (devicons) in the file list — a small extension→glyph map.
- Git branch glyph on the scope chip; a comment glyph in the gutter on commented lines; a nicer fold chevron.

## Gutter & extras

- ⭐ A subtle scrollbar / position rail on the diff (ratatui `Scrollbar`), optionally marking comment positions (mini-minimap).
- Thin separator between gutter and code; a small left margin so code doesn't hug the border.
- Dual old|new number columns even in unified view (currently single).

## Approach

Color values are see-it-live decisions — wire a batch, push to the pane, tune by eye ("warmer / dimmer / more contrast") rather than guessing hex in the abstract.

## All files — deferred from the UX-review pass (2026-06-26)

The UX review of the All files tab fixed the high-value items (A–H); these were consciously deferred.

- **Git-blob/diff-side read cap.** `set_file_view` now checks on-disk size before reading (`app.rs`), but the diff path (`git::file_content` → full `git show` stdout; `worktree_content` on the new side) still reads before `build`'s budget trips. Mirror the metadata guard with `git cat-file -s` on the blob side. Pre-existing, less acute than the one-keystroke File-view path.
- **File-view per-frame height/wrap recompute.** `diff_row_heights` re-wraps every visible row each frame; the File view disables folding, so `visible` is the whole file (up to the 50k-line cap), making `j`-scroll on a big file laggy. Memoize heights by `(content-hash, width, wrap)`, or measure only the scroll window. Same class as the M5-deferred "single measure+paint pass".
- **`set_scope` reveals the cursor.** A scope switch sets `reveal_files = true`, which can yank a wheel-scrolled All files viewport back to the cursor, against file-list.md's "scroll holds". Minor; only after a wheel scroll.
- **Header count vs. visible markers.** The count is the changeset (e.g. `3 changed`) while a deletion in that set has no row in the worktree tree, so the number can exceed the marked rows. Spec-sanctioned; no change.
- **Symlinks render target content.** `worktree_content`'s `fs::read` follows links. Acceptable; a broken symlink reads empty → "empty file".
- **`is_stale` stats the disk per content comment per frame** while the `l` comments overlay is open (`app.rs`). Minor syscall churn (overlay-only, content-anchored comments only). Cache existence per reload rather than per frame if it ever matters. (Garfield review, 2026-06-26.)
- **`TabStash` is a 13-field manual swap.** Adding a per-tab field to `App` without adding it to `swap_active_with_stash` silently leaks state across tabs. A `[TabState; 2]` indexed by `tab` removes the footgun but touches every field access — a refactor, not a local fix. A warning comment guards it for now. (Garfield review, 2026-06-26.)
- **`All files` poll shells out to git twice.** Each ~2s poll computes the changeset (`changed_files`) and `all_files` (`ls-files` + `status`) — ~3 subprocesses, re-listing the whole worktree even when nothing changed. Refresh the cheap changeset each poll and cache the `all_files` listing, recomputing only on explicit refresh or a structural-change signal. (Code-review, 2026-06-26.)
- **`set_file_view` re-reads the open file every poll.** A quiescent open `All files` file is read whole off disk and content-hashed each poll only to hit the cache and no-op (the cache avoids re-highlight, not re-read). Gate on the `metadata` (mtime/len) already stat'd and skip the read+hash when unchanged. (Code-review, 2026-06-26.)

## Efficiency — deferred from the simplify pass (2026-06-26)

These add caching/gating logic to hot paths (not behavior-preserving dedups), so they were recorded rather than applied in the simplify pass. (Simplify efficiency agent, 2026-06-26.)

- **Two worktree snapshots per `last-turn` poll.** While a turn candidate is pending, each ~2s poll runs `snapshot_worktree` twice — once in `apply_agent_status`'s divergence check (`app.rs`) and again in `reload`→`changed_against_tree` (`git.rs`). Each forks `git add -A` + `git write-tree` over the whole worktree. Compute one snapshot per poll and feed both the divergence check and the changeset diff from it.
- **`comment_cards()` rebuilt twice (and `commented_lines()` a third scan) per frame.** `diff_row_heights` and `render_diff_view` each call `comment_cards()` independently — each an O(store×visible) `rposition` scan — and `commented_lines()` is a third O(visible×store) pass over the same store. Do one pass over the store per frame, producing the per-row card lists and the commented-row set together, shared with the height pass.
- **`diff_row_heights` builds full styled cards just to count lines.** Per visible row it calls `comment_card_lines(c, width).len()`, allocating a `Vec<Line>` of styled, word-wrapped spans only to take `.len()`. Add a count-only `comment_card_height(c, width)` (`1 + Σ wrap_text(line).len() + 1`) that doesn't allocate the spans.
- **Changes diff sides re-read and re-hashed every poll.** A quiescent open `Changes` file re-runs `git show HEAD:<path>` (a subprocess) plus a worktree `fs::read` each poll via `content_sides`, then hashes both sides, only to hit `DiffCache` and no-op. Sibling of the `set_file_view` re-read above, but also forks a git process. Gate on a cheap signal (worktree mtime/len + HEAD oid) before reading/hashing.
- **Render builds Lines for the whole tail of the file, then truncates.** `display(app.diff_scroll..rows)` flat-maps `row_lines` over scroll-to-EOF and `collect()`s before `out.truncate(height)`; on a large File view scrolled near the top this materializes tens of thousands of off-screen rows per frame. Stop accumulating once `height` display rows are reached. (Overlaps the deferred "single measure+paint pass"; note it fires on the Changes tab too.)
