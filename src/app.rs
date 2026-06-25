//! Application state and transitions for the Changes review TUI.
//!
//! See `specs/tui.md` and `specs/review-model.md`. This module is terminal-free:
//! every method is a pure state transition or a read-only git/export call, so the
//! whole interaction model is testable without a backend. `src/main.rs` owns the
//! terminal and maps input events onto these methods.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;

use crate::diff::{DiffCache, FileDiff, Row};
use crate::export::{ExportTarget, format_all};
use crate::file_list::{self, RowKind};
use crate::git;
use crate::highlight::Highlighter;
use crate::logln;
use crate::model::{ChangedFile, Comment, CommentStore, Scope, Side};
use crate::turn::{Status, TurnTracker};

/// The file-list pane's default width and resize bounds, as a percent of the body. The
/// bounds keep both panes usable however the reviewer drags the divider.
const DEFAULT_LIST_PCT: u16 = 32;
const MIN_LIST_PCT: u16 = 15;
const MAX_LIST_PCT: u16 = 60;

/// Which pane has the keyboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Files,
    Diff,
}

/// What the file-list cursor points at, by path, so it can be restored to the same target
/// after the tree rebuilds on a poll.
enum Anchor {
    File(String),
    Dir(String),
}

/// The interaction mode the UI is in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    /// Writing a comment; `editing` is the store index when editing an existing one.
    Composing {
        editing: Option<usize>,
    },
    /// Browsing the comments-list overlay.
    List,
}

/// The full state of the review session.
// The several bools (wrap, reveal_files, reveal_diff, resizing, should_quit) are independent
// toggles, not a state machine in disguise, so the excessive-bools lint does not apply.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct App {
    pub repo: PathBuf,
    pub base: Option<String>,
    pub scope: Scope,
    pub focus: Focus,
    pub files: Vec<ChangedFile>,
    /// The flattened directory tree over `files` — the rows the navigator paints. The
    /// `file_cursor` indexes this, not `files`.
    pub file_rows: Vec<file_list::Row>,
    pub file_cursor: usize,
    /// Top visible row of the file list, kept so `file_cursor` stays on screen when the
    /// changeset is taller than the pane.
    pub file_scroll: usize,
    /// Set by a navigation that moves `file_cursor`; consumed once per frame to scroll the
    /// cursor into view. The wheel never sets it, so wheel-scrolling moves the viewport alone.
    pub reveal_files: bool,
    /// Set by a navigation that moves `diff_cursor`; consumed once per frame to scroll the
    /// cursor into view. The wheel never sets it.
    pub reveal_diff: bool,
    /// Whether the current compose was opened from the comments-list overlay, so finishing it
    /// returns there rather than dropping to the diff.
    resume_list: bool,
    /// Directory paths the user has collapsed; every other directory is expanded. Keyed by
    /// path, so it survives a poll that rebuilds the tree.
    collapsed_dirs: HashSet<String>,
    pub diff: FileDiff,
    /// The rows actually shown: `diff.rows` with each fold collapsed to a marker or
    /// expanded to its lines. The cursor, scroll, selection, and hit-testing index this.
    pub visible: Vec<Row>,
    /// Fold anchors (first-hidden-line numbers) currently expanded; survives a poll.
    expanded_folds: HashSet<u32>,
    /// The file the open diff belongs to — the diff title, frozen with the diff
    /// while composing even if `file_cursor` drifts as the file list updates.
    pub diff_path: Option<String>,
    pub diff_cursor: usize,
    /// Top visible diff line. Sticky: only moves to keep the cursor in view, so the
    /// diff does not jump on every cursor step and drag-selection stays stable.
    pub diff_scroll: usize,
    /// Horizontal scroll, in columns, applied to the diff when wrap is off.
    pub h_scroll: usize,
    /// Whether long diff lines wrap (default) or are scrolled horizontally.
    pub wrap: bool,
    /// The file-list pane's width as a percent of the body; the diff takes the rest. The
    /// reviewer resizes it by dragging the divider or with `[` / `]`.
    pub list_pct: u16,
    /// Whether a mouse drag is currently moving the pane divider.
    pub resizing: bool,
    pub select_anchor: Option<usize>,
    pub store: CommentStore,
    pub list_cursor: usize,
    pub mode: Mode,
    pub input: String,
    /// The comment editor's caret: a char index into `input` (`0..=chars().count()`).
    pub caret: usize,
    pub status: String,
    pub should_quit: bool,
    highlighter: Highlighter,
    cache: DiffCache,
    /// The `last-turn` baseline lifecycle, driven by polling the agent's status.
    turn: TurnTracker,
    /// This worktree's key for the private baseline ref, fixed for the session.
    turn_key: String,
}

impl App {
    pub fn new(repo: PathBuf, scope: Scope, base: Option<String>) -> Self {
        // Resume any persisted turn baseline for this worktree, so `last-turn` keeps its
        // anchor across a sidebar restart (specs/herdr-host.md).
        let turn_key = git::worktree_key(&repo);
        let turn = TurnTracker::with_baseline(git::read_baseline_ref(&repo, &turn_key));
        Self {
            repo,
            base,
            scope,
            focus: Focus::Files,
            files: Vec::new(),
            file_rows: Vec::new(),
            file_cursor: 0,
            file_scroll: 0,
            reveal_files: false,
            reveal_diff: false,
            resume_list: false,
            collapsed_dirs: HashSet::new(),
            diff: FileDiff::empty(),
            visible: Vec::new(),
            expanded_folds: HashSet::new(),
            diff_path: None,
            diff_cursor: 0,
            diff_scroll: 0,
            h_scroll: 0,
            wrap: true,
            list_pct: DEFAULT_LIST_PCT,
            resizing: false,
            select_anchor: None,
            store: CommentStore::new(),
            list_cursor: 0,
            mode: Mode::Normal,
            input: String::new(),
            caret: 0,
            status: String::new(),
            should_quit: false,
            highlighter: Highlighter::new(None),
            cache: DiffCache::new(),
            turn,
            turn_key,
        }
    }

    /// Rebuild the highlighter for the named theme and drop cached diffs so they
    /// re-render in it. Unknown or unset names fall back to Catppuccin Mocha.
    pub fn set_theme(&mut self, name: Option<&str>) {
        if name.is_some() {
            self.highlighter = Highlighter::new(name);
            self.cache = DiffCache::new();
        }
    }

    pub fn composing(&self) -> bool {
        matches!(self.mode, Mode::Composing { .. })
    }

    /// The changed file under the cursor when the cursor is on a file row; `None` on a
    /// directory row (or an empty list).
    pub fn current_file(&self) -> Option<&ChangedFile> {
        self.file_under_cursor_index().map(|i| &self.files[i])
    }

    /// The `files` index of the file row under the cursor, or `None` on a directory row.
    fn file_under_cursor_index(&self) -> Option<usize> {
        self.file_rows.get(self.file_cursor).and_then(file_list::Row::file_index)
    }

    /// The visible-row index of the file at `path`, for restoring selection across a poll.
    fn file_row_of_path(&self, path: &str) -> Option<usize> {
        self.file_rows
            .iter()
            .position(|r| r.file_index().is_some_and(|i| self.files[i].path == path))
    }

    /// The visible-row index of the first file row, the initial selection so a diff shows
    /// at once even when the tree opens on a directory.
    fn first_file_row(&self) -> Option<usize> {
        self.file_rows.iter().position(|r| r.file_index().is_some())
    }

    /// Rebuild the flattened tree from `files` and the collapsed-directory set.
    fn rebuild_file_rows(&mut self) {
        self.file_rows = file_list::build(&self.files, &self.collapsed_dirs);
    }

    /// What the cursor currently points at — a file (by path) or a directory (by path) — so
    /// the cursor can be put back on the same target after the tree rebuilds.
    fn cursor_anchor(&self) -> Option<Anchor> {
        self.file_rows.get(self.file_cursor).map(|r| match &r.kind {
            RowKind::File { index, .. } => Anchor::File(self.files[*index].path.clone()),
            RowKind::Dir { path, .. } => Anchor::Dir(path.clone()),
        })
    }

    /// The visible-row index matching `anchor`, for restoring the cursor after a rebuild.
    fn row_of_anchor(&self, anchor: &Anchor) -> Option<usize> {
        self.file_rows.iter().position(|r| match (anchor, &r.kind) {
            (Anchor::File(p), RowKind::File { index, .. }) => &self.files[*index].path == p,
            (Anchor::Dir(p), RowKind::Dir { path, .. }) => path == p,
            _ => false,
        })
    }

    /// The file whose diff the pane shows: the file under the cursor, or — when the cursor
    /// rests on a directory — the already-open file (matched by `diff_path`), so scanning the
    /// tree never blanks the diff. `None` only when nothing is open.
    fn shown_file(&self) -> Option<ChangedFile> {
        if let Some(f) = self.current_file() {
            return Some(f.clone());
        }
        let open = self.diff_path.as_deref()?;
        self.files.iter().find(|f| f.path == open).cloned()
    }

    /// Reload the changed-files list and (unless composing) the open diff.
    ///
    /// Never touches the comment store or the in-progress input — that is the
    /// "a comment is never lost to a refresh" invariant (`specs/overview.md`).
    pub fn reload(&mut self) -> Result<()> {
        // Outside a git repo, show an empty state rather than failing (herdr-host.md).
        if !git::is_repo(&self.repo) {
            self.files.clear();
            self.file_rows.clear();
            self.file_cursor = 0;
            self.file_scroll = 0;
            if !self.composing() {
                self.diff = FileDiff::empty();
                self.diff_path = None;
                self.visible.clear(); // keep `visible` mirroring `diff` so no stale rows paint
                self.reset_diff_view();
            }
            return Ok(());
        }
        // Keep the cursor on the same row target across the rebuild; fall back to the open
        // file, then the first file. The collapsed-directory set survives untouched.
        let anchor = self.cursor_anchor();
        let open = self.diff_path.clone();
        self.files = match self.scope {
            // last-turn diffs the captured baseline; with none yet, it is empty until a
            // turn start is observed (specs/review-model.md).
            Scope::LastTurn => match self.turn.baseline() {
                Some(t) => git::changed_against_tree(&self.repo, t)?,
                None => Vec::new(),
            },
            _ => git::changed_files(&self.repo, self.scope, self.base.as_deref())?,
        };
        self.rebuild_file_rows();
        self.file_cursor = anchor
            .and_then(|a| self.row_of_anchor(&a))
            .or_else(|| open.as_deref().and_then(|p| self.file_row_of_path(p)))
            .or_else(|| self.first_file_row())
            .unwrap_or(0)
            .min(self.file_rows.len().saturating_sub(1));
        // A poll preserves the file-list wheel scroll — it does not reveal the cursor.
        // Explicit actions (navigation, a scope switch) request their own reveal.
        // While a modal is open — composing a comment, or the comments-list overlay — the
        // open diff is frozen, so a poll can't shift the anchor beneath the writer or reset
        // the scroll/selection under the overlay. The file list still updates above.
        if !self.composing() && self.mode != Mode::List {
            // A poll keeps the reader on the same file; only a different shown file resets
            // the diff view to the top.
            if self.shown_file().map(|f| f.path) != self.diff_path {
                self.reset_diff_view();
            }
            self.load_diff();
        }
        Ok(())
    }

    /// Build the shown file's diff (the file under the cursor, or the open file), flatten
    /// folds into the visible rows, then clamp the cursor/scroll so a refresh keeps position.
    fn load_diff(&mut self) {
        if let Some(file) = self.shown_file() {
            self.set_diff(file);
        } else {
            self.diff = FileDiff::empty();
            self.diff_path = None;
            self.visible.clear();
            self.reset_diff_view();
        }
    }

    /// Build the diff for a specific `file` regardless of whether its row is visible in the
    /// tree — so editing a comment can surface its file even from a collapsed directory.
    fn set_diff(&mut self, file: ChangedFile) {
        // A different file opens with all folds collapsed. `expanded_folds` is keyed by line
        // number, so without this a fold in the new file whose first hidden line matches an
        // expanded one in the old file would render pre-expanded. A same-file poll keeps them.
        if self.diff_path.as_deref() != Some(file.path.as_str()) {
            self.expanded_folds.clear();
        }
        self.diff_path = Some(file.path.clone());
        let (old, new) = self.content_sides(&file);
        self.diff = self.cache.get(file.path, file.previous_path, &old, &new, &self.highlighter);
        self.rebuild_visible();

        if self.visible.is_empty() {
            self.reset_diff_view();
        } else {
            let last = self.visible.len() - 1;
            let clamped = self.diff_cursor.min(last);
            // A poll preserves the wheel scroll: only re-settle when a shrunk diff forced the
            // cursor to move. Revealing unconditionally here would snap the viewport back to
            // the cursor on every refresh, undoing a wheel scroll.
            if clamped != self.diff_cursor {
                self.reveal_diff = true;
            }
            self.diff_cursor = clamped;
            self.diff_scroll = self.diff_scroll.min(last);
            self.select_anchor = self.select_anchor.map(|a| a.min(last));
        }
    }

    /// Flatten `diff.rows` into `visible`: an expanded fold becomes its lines, a
    /// collapsed fold stays a single marker row.
    fn rebuild_visible(&mut self) {
        self.visible = self
            .diff
            .rows
            .iter()
            .flat_map(|row| match row {
                Row::Fold { lines }
                    if row.fold_anchor().is_some_and(|a| self.expanded_folds.contains(&a)) =>
                {
                    lines.clone()
                }
                _ => vec![row.clone()],
            })
            .collect();
    }

    /// Expand the fold under the cursor, revealing its hidden lines. Expansion is
    /// permanent for the session — an expand is taken as intentional, so there is no
    /// collapse-back.
    /// Expand the fold under the cursor, keeping the viewport visually still. Where the fold
    /// sits decides which way it grows: a fold in the top half of the diff expands upward (the
    /// lines below it hold their screen position); one in the bottom half expands downward (the
    /// lines above hold theirs). `heights`/`viewport` are this frame's pre-expand diff geometry.
    pub fn expand_fold(&mut self, heights: &[usize], viewport: usize) {
        let fold_idx = self.diff_cursor;
        let Some(anchor) = self.visible.get(fold_idx).and_then(Row::fold_anchor) else {
            return;
        };
        // Expanding replaces the 1 fold row with N context rows; rows below it shift by N-1.
        let shift = self.visible[fold_idx].hidden().saturating_sub(1);
        // Display rows between the viewport top and the fold; < half ⇒ top half. When the fold
        // is wheeled above the viewport (fold_idx < diff_scroll), the range is empty → above 0 →
        // top half, which is correct: the inserted rows land above the viewport, so advancing
        // diff_scroll by `shift` holds the visible content in place.
        let above: usize = heights.get(self.diff_scroll..fold_idx).map_or(0, |s| s.iter().sum());
        let top_half = above < viewport / 2;
        self.expanded_folds.insert(anchor);
        self.rebuild_visible();
        if top_half {
            self.diff_scroll += shift; // hold the content below the fold; grow upward
        }
        // bottom half: leave diff_scroll — the content above the fold stays put, grow downward
    }

    /// The old and new content of `file` for the current scope: old from `HEAD` (or the
    /// merge-base on the branch scope), new from the worktree (or `HEAD` on branch). A rename
    /// reads its old side from `previous_path`, so the diff shows real edits, not a wholesale
    /// delete-and-add.
    fn content_sides(&self, file: &ChangedFile) -> (String, String) {
        let new_path = file.path.as_str();
        let old_path = file.previous_path.as_deref().unwrap_or(new_path);
        match self.scope {
            Scope::Uncommitted => {
                let old = git::file_content(&self.repo, "HEAD", old_path);
                let new = worktree_content(&self.repo, new_path);
                (old, new)
            }
            Scope::Branch => {
                let mb = git::merge_base(&self.repo, self.base.as_deref());
                let old =
                    mb.map(|m| git::file_content(&self.repo, &m, old_path)).unwrap_or_default();
                (old, git::file_content(&self.repo, "HEAD", new_path))
            }
            Scope::LastTurn => {
                let old = self
                    .turn
                    .baseline()
                    .map(|b| git::file_content(&self.repo, b, old_path))
                    .unwrap_or_default();
                (old, worktree_content(&self.repo, new_path))
            }
        }
    }

    /// Whether the `last-turn` scope is active but no baseline has been captured yet — the
    /// cold-start (or no-herdr) state the UI paints as `waiting for the agent's next turn`.
    pub fn awaiting_turn(&self) -> bool {
        self.scope == Scope::LastTurn && !self.turn.has_baseline()
    }

    /// Sample the agent's status and advance the `last-turn` baseline. Reads the resolved
    /// agent's status over the herdr CLI; absence or ambiguity pauses tracking. Never
    /// propagates — a missing herdr is normal, so failures only log.
    pub fn track_turn(&mut self) {
        let status = crate::herdr::resolved_agent_status().ok().flatten();
        self.apply_agent_status(status.as_deref());
    }

    /// Advance the baseline from one status sample — the core [`track_turn`](Self::track_turn)
    /// wraps, and the seam tests drive without herdr. On a turn start (a resting→`working`
    /// edge) it snapshots the worktree as the candidate; while a candidate is pending it
    /// promotes once the worktree diverges from it, persisting the new baseline. Git errors
    /// only log, so a transient git failure never crashes the poll.
    pub fn apply_agent_status(&mut self, status: Option<&str>) {
        let Some(status) = status else { return };
        if self.turn.observe(Status::parse(status)) {
            match git::snapshot_worktree(&self.repo) {
                Ok(sha) => self.turn.set_candidate(sha),
                Err(e) => logln!("turn snapshot failed: {e}"),
            }
        }
        // Promote the pending candidate once the turn has changed a file. Compare full
        // snapshots so a new untracked file counts as a change (specs/herdr-host.md).
        let Some(candidate) = self.turn.candidate().map(str::to_string) else { return };
        match git::snapshot_worktree(&self.repo) {
            Ok(now) if now != candidate => {
                self.turn.promote();
                if let Err(e) = git::write_baseline_ref(&self.repo, &self.turn_key, &candidate) {
                    logln!("turn baseline ref write failed: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => logln!("turn divergence check failed: {e}"),
        }
    }

    /// Snap the diff view back to the top, clearing any pending selection.
    fn reset_diff_view(&mut self) {
        self.diff_cursor = 0;
        self.diff_scroll = 0;
        self.h_scroll = 0;
        self.select_anchor = None;
    }

    /// Scroll the diff horizontally by `delta` columns, clamped at the left edge. A no-op
    /// while wrap is on, since the renderer ignores `h_scroll` when wrapping — so the offset
    /// never silently accumulates and then jumps the view when wrap is toggled off.
    pub fn scroll_h(&mut self, delta: isize) {
        if self.wrap {
            return;
        }
        self.h_scroll = if delta >= 0 {
            self.h_scroll + delta as usize
        } else {
            self.h_scroll.saturating_sub(delta.unsigned_abs())
        };
    }

    /// Toggle line wrap; reset the horizontal scroll, which only applies with wrap off.
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.h_scroll = 0;
    }

    /// Widen (`+`) or narrow (`-`) the file-list pane by `delta` percent, clamped so neither
    /// pane collapses. Bound to `]` / `[`.
    pub fn resize_list(&mut self, delta: i16) {
        let next = (self.list_pct as i16 + delta).clamp(MIN_LIST_PCT as i16, MAX_LIST_PCT as i16);
        self.list_pct = next as u16;
    }

    /// Set the file-list width so the divider sits at body column `x` (a mouse drag). `x` is
    /// measured from the body's left edge; the list spans from there to the right edge.
    pub fn drag_divider(&mut self, body_width: u16, x: u16) {
        if body_width == 0 {
            return;
        }
        let list_cols = body_width.saturating_sub(x.min(body_width));
        let pct = (u32::from(list_cols) * 100 / u32::from(body_width)) as u16;
        self.list_pct = pct.clamp(MIN_LIST_PCT, MAX_LIST_PCT);
    }

    // --- Scroll model (shared by both panes) ---------------------------------------
    //
    // Each pane has a cursor (selection) and a scroll offset (viewport top). They are
    // independent: keyboard navigation moves the cursor and requests a reveal; the wheel
    // moves the offset and requests nothing. Every frame the event loop reveals the cursor
    // *only if a move requested it* (so the wheel can leave the cursor off screen) and then
    // bounds the offset (so an over-scroll never shows a blank tail). Both panes run the
    // same `keep_in_view` + `bound`; the file list passes all-height-1 rows.

    /// Scroll the file list so `file_cursor` is on screen — the minimal nudge. Called once
    /// per frame when a navigation requested a reveal, not on a wheel scroll.
    pub fn reveal_file_cursor(&mut self, viewport: usize) {
        if self.file_rows.is_empty() {
            self.file_scroll = 0;
            return;
        }
        let cursor = self.file_cursor.min(self.file_rows.len() - 1);
        let heights = vec![1usize; self.file_rows.len()];
        self.file_scroll = keep_in_view(cursor, self.file_scroll, &heights, viewport);
    }

    /// Clamp `file_scroll` within range (no blank tail). Called every frame.
    pub fn bound_file_scroll(&mut self, viewport: usize) {
        self.file_scroll = bound(self.file_scroll, self.file_rows.len(), viewport);
    }

    /// Scroll the diff so `diff_cursor`'s row fits the `viewport`-display-row window —
    /// `heights` is each visible row's display height (wrap + comment cards). Called once
    /// per frame when a navigation requested a reveal, not on a wheel scroll.
    pub fn reveal_diff_cursor(&mut self, heights: &[usize], viewport: usize) {
        if self.visible.is_empty() {
            self.diff_scroll = 0;
            return;
        }
        let cursor = self.diff_cursor.min(self.visible.len() - 1);
        self.diff_scroll = keep_in_view(cursor, self.diff_scroll, heights, viewport);
    }

    /// Clamp `diff_scroll` within range (no blank tail). Called every frame. Height-aware:
    /// the cap is the offset that shows the LAST row at the bottom — computed from `heights`,
    /// not the row count, so a wrapped diff (tall rows) stays fully reachable. A row-count cap
    /// would stop short of the bottom whenever rows span more than one display line.
    pub fn bound_diff_scroll(&mut self, heights: &[usize], viewport: usize) {
        if heights.is_empty() {
            self.diff_scroll = 0;
            return;
        }
        let max_top = keep_in_view(heights.len() - 1, self.diff_scroll, heights, viewport);
        self.diff_scroll = self.diff_scroll.min(max_top);
    }

    /// Switch the changeset scope and reload. A no-op while composing, so a comment
    /// in progress is never stranded against a different diff.
    pub fn set_scope(&mut self, scope: Scope) -> Result<()> {
        if self.scope != scope && !self.composing() {
            self.scope = scope;
            self.file_cursor = 0;
            // A file's old side can differ between scopes at the same path+new-content, so
            // drop cached diffs rather than risk returning the other scope's build. Fold
            // expansions are keyed by line number within a diff, so drop them too.
            self.cache = DiffCache::new();
            self.expanded_folds.clear();
            self.reset_diff_view();
            self.reload()?;
            // An explicit scope switch resets to the top, so reveal the reset cursor (a poll,
            // which also calls reload, deliberately does not).
            self.reveal_files = true;
        }
        Ok(())
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Files => Focus::Diff,
            Focus::Diff => Focus::Files,
        };
    }

    /// Move the cursor in the focused pane. In the files pane the cursor steps over the
    /// tree's visible rows; landing on a file row opens its diff, while a directory row
    /// keeps the current diff so scanning the tree never blanks the pane.
    pub fn move_cursor(&mut self, delta: isize) -> Result<()> {
        match self.focus {
            Focus::Files => {
                if !self.file_rows.is_empty() {
                    self.file_cursor = step(self.file_cursor, delta, self.file_rows.len());
                    self.open_cursor_file();
                    // Reveal even when the index clamps unchanged (e.g. `k` at the top), so a
                    // navigation always pulls the cursor back after a wheel scroll.
                    self.reveal_files = true;
                }
            }
            Focus::Diff => {
                if !self.visible.is_empty() {
                    let mut target = step(self.diff_cursor, delta, self.visible.len());
                    if let Some(a) = self.select_anchor {
                        target = self.fold_clamped(a, target);
                    }
                    self.diff_cursor = target;
                    self.reveal_diff = true;
                }
            }
        }
        Ok(())
    }

    /// Open the diff for the file under the cursor when it differs from the one shown; a
    /// no-op on a directory row, so the current diff stays put.
    fn open_cursor_file(&mut self) {
        if let Some(i) = self.file_under_cursor_index()
            && Some(self.files[i].path.as_str()) != self.diff_path.as_deref()
        {
            self.reset_diff_view();
            self.load_diff();
        }
    }

    /// Act on the file-list row at `index` (a mouse click): a file opens its diff, a
    /// directory toggles its expansion.
    pub fn select_file(&mut self, index: usize) -> Result<()> {
        if index >= self.file_rows.len() {
            return Ok(());
        }
        self.focus = Focus::Files;
        self.file_cursor = index;
        self.reveal_files = true;
        match self.file_rows[index].kind {
            RowKind::File { .. } => self.open_cursor_file(),
            RowKind::Dir { .. } => self.toggle_dir(),
        }
        Ok(())
    }

    /// Collapse or expand the directory under the cursor, then rebuild the tree. The cursor
    /// stays on the directory row (still present, now toggled).
    fn toggle_dir(&mut self) {
        let Some(path) = self.dir_under_cursor() else { return };
        if !self.collapsed_dirs.remove(&path) {
            self.collapsed_dirs.insert(path);
        }
        self.apply_dir_change();
    }

    /// Whether the cursor is on a directory row in the focused file list — the rows `←`/`→`
    /// collapse and expand (elsewhere those keys scroll the diff).
    pub fn on_folder(&self) -> bool {
        self.focus == Focus::Files
            && self.file_rows.get(self.file_cursor).is_some_and(|r| r.dir_path().is_some())
    }

    /// Whether the diff cursor is on a fold row — the row `→` expands (elsewhere `→` scrolls
    /// the diff sideways). Folds are expand-only, so `←` never collapses one.
    pub fn on_fold(&self) -> bool {
        self.focus == Focus::Diff
            && self.visible.get(self.diff_cursor).and_then(Row::fold_anchor).is_some()
    }

    /// Expand the directory under the cursor (`→`); a no-op if it is a file or already open.
    pub fn expand_dir(&mut self) {
        if let Some(path) = self.dir_under_cursor()
            && self.collapsed_dirs.remove(&path)
        {
            self.apply_dir_change();
        }
    }

    /// Collapse the directory under the cursor (`←`); a no-op if it is a file or already shut.
    pub fn collapse_dir(&mut self) {
        if let Some(path) = self.dir_under_cursor()
            && self.collapsed_dirs.insert(path)
        {
            self.apply_dir_change();
        }
    }

    /// The path of the directory row under the cursor, if any.
    fn dir_under_cursor(&self) -> Option<String> {
        self.file_rows.get(self.file_cursor).and_then(|r| r.dir_path()).map(str::to_string)
    }

    /// Rebuild the tree after a directory's expansion changed, keeping the cursor in range.
    fn apply_dir_change(&mut self) {
        self.rebuild_file_rows();
        self.file_cursor = self.file_cursor.min(self.file_rows.len().saturating_sub(1));
        self.reveal_files = true; // the row may have moved off-screen; pull it back
    }

    /// Page the diff: move `diff_cursor` by `delta` lines and reveal it. The page/half-page
    /// keys when the diff is focused; the view follows the cursor like `j`/`k`.
    pub fn page_diff(&mut self, delta: isize) {
        if !self.visible.is_empty() {
            let mut target = step(self.diff_cursor, delta, self.visible.len());
            if let Some(a) = self.select_anchor {
                target = self.fold_clamped(a, target); // a selection cannot cross a fold
            }
            self.diff_cursor = target;
            self.reveal_diff = true;
        }
    }

    /// Page the file list: move `file_cursor` by `delta`, open the file it lands on, and
    /// reveal it. The page/half-page keys when the file list is focused.
    pub fn page_files(&mut self, delta: isize) {
        if !self.file_rows.is_empty() {
            self.file_cursor = step(self.file_cursor, delta, self.file_rows.len());
            self.open_cursor_file();
            self.reveal_files = true;
        }
    }

    /// Wheel-scroll the diff's viewport, leaving `diff_cursor` (the comment anchor) put —
    /// so wheeling to read context never moves what a comment will attach to. The upper
    /// bound is applied each frame by `bound_diff_scroll`.
    pub fn wheel_diff(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        self.diff_scroll = offset_by(self.diff_scroll, delta);
    }

    /// Wheel-scroll the file list's viewport, leaving the selection and the open diff
    /// untouched — so browsing the list never reloads a diff. Bounded each frame.
    pub fn wheel_files(&mut self, delta: isize) {
        if self.file_rows.is_empty() {
            return;
        }
        self.file_scroll = offset_by(self.file_scroll, delta);
    }

    /// Extend a mouse drag-selection to the diff line at `index`, anchoring on first drag.
    pub fn drag_select_to(&mut self, index: usize) {
        if index < self.visible.len() {
            self.focus = Focus::Diff;
            let anchor = *self.select_anchor.get_or_insert(self.diff_cursor);
            self.diff_cursor = self.fold_clamped(anchor, index);
            self.reveal_diff = true;
        }
    }

    /// Clamp `target` so the inclusive range from `anchor` to `target` crosses no fold: a
    /// selection treats a fold as a hard boundary, so its line range and snippet always agree
    /// (never bracketing hidden lines the snippet omits). Stops the moving end shy of the fold.
    fn fold_clamped(&self, anchor: usize, target: usize) -> usize {
        if target > anchor {
            (anchor + 1..=target).find(|&i| !self.visible[i].is_content()).map_or(target, |i| i - 1)
        } else {
            (target..anchor)
                .rev()
                .find(|&i| !self.visible[i].is_content())
                .map_or(target, |i| i + 1)
        }
    }

    /// Toggle a range-selection anchor at the current diff line.
    pub fn toggle_select(&mut self) {
        if self.focus == Focus::Diff && !self.visible.is_empty() {
            self.select_anchor = match self.select_anchor {
                Some(_) => None,
                None => Some(self.diff_cursor),
            };
            self.reveal_diff = true;
        }
    }

    /// The inclusive `[lo, hi]` diff-line range currently selected.
    pub fn selection_range(&self) -> (usize, usize) {
        match self.select_anchor {
            Some(a) => (a.min(self.diff_cursor), a.max(self.diff_cursor)),
            None => (self.diff_cursor, self.diff_cursor),
        }
    }

    pub fn start_comment(&mut self) {
        if self.focus == Focus::Diff && self.has_anchorable_selection() {
            // Anchor the cursor at the selection's last line so the scroll keeps it (and
            // the box drawn beneath it) in view.
            self.diff_cursor = self.selection_range().1;
            self.reveal_diff = true; // scroll the anchored line into view before the box opens
            self.input.clear();
            self.caret = 0;
            self.resume_list = false; // a fresh diff comment returns to the diff, not the list
            self.mode = Mode::Composing { editing: None };
        }
    }

    pub fn start_edit(&mut self) {
        // Editing from the comments-list overlay returns there on finish (else to the diff).
        let from_list = self.mode == Mode::List;
        let Some(i) = self.target_comment() else { return };
        let Some(c) = self.store.get(i) else { return };
        let (file, side, start, end, text) =
            (c.file.clone(), c.side, c.start, c.end, c.text.clone());

        // Bring the comment's file into the diff and land the cursor on its line, so the
        // inline edit box opens over the comment — even when editing from the list, and even
        // when the file's row is hidden inside a collapsed directory (load it by path, not by
        // tree row). Move the list cursor onto its row when one exists.
        if self.diff_path.as_deref() != Some(file.as_str())
            && let Some(f) = self.files.iter().find(|f| f.path == file).cloned()
        {
            self.reset_diff_view();
            self.set_diff(f);
            if let Some(fi) = self.file_row_of_path(&file) {
                self.file_cursor = fi;
            }
        }
        // Only move the cursor when the open diff is actually the comment's file, so a
        // stale comment (file gone from the changeset) never jumps the cursor onto a
        // same-numbered line in a different file.
        if self.diff_path.as_deref() == Some(file.as_str())
            && let Some(idx) = self.visible.iter().position(|row| {
                let no = match side {
                    Side::New => row.new_no(),
                    Side::Old => row.old_no(),
                };
                no.is_some_and(|n| start <= n && n <= end)
            })
        {
            self.diff_cursor = idx;
            self.select_anchor = None;
        }
        self.focus = Focus::Diff;
        self.reveal_diff = true; // scroll the edited line into view before the box opens
        self.caret = text.chars().count(); // edit opens with the caret at the end
        self.input = text;
        self.resume_list = from_list;
        self.mode = Mode::Composing { editing: Some(i) };
    }

    // --- comment editor: a character caret into `input`; edits happen at the caret ---------
    // `caret` is a char index in `0..=input.chars().count()`. Edits round-trip through a
    // `Vec<char>` (comments are short), so every op is character-wise and multi-byte safe.

    /// Insert `ch` at the caret.
    pub fn input_push(&mut self, ch: char) {
        if !self.composing() {
            return;
        }
        let mut v: Vec<char> = self.input.chars().collect();
        let at = self.caret.min(v.len());
        v.insert(at, ch);
        self.caret = at + 1;
        self.input = v.into_iter().collect();
    }

    /// Insert pasted `text` at the caret as one unit, normalizing `\r\n`/`\r` to `\n`.
    pub fn input_paste(&mut self, text: &str) {
        if !self.composing() {
            return;
        }
        let norm: Vec<char> = text.replace("\r\n", "\n").replace('\r', "\n").chars().collect();
        let mut v: Vec<char> = self.input.chars().collect();
        let at = self.caret.min(v.len());
        let n = norm.len();
        v.splice(at..at, norm);
        self.caret = at + n;
        self.input = v.into_iter().collect();
    }

    /// Delete the character before the caret.
    pub fn input_backspace(&mut self) {
        if !self.composing() || self.caret == 0 {
            return;
        }
        let mut v: Vec<char> = self.input.chars().collect();
        let at = self.caret.min(v.len());
        if at == 0 {
            return;
        }
        v.remove(at - 1);
        self.caret = at - 1;
        self.input = v.into_iter().collect();
    }

    /// Delete the character at the caret (`Delete`).
    pub fn input_delete_forward(&mut self) {
        if !self.composing() {
            return;
        }
        let mut v: Vec<char> = self.input.chars().collect();
        if self.caret < v.len() {
            v.remove(self.caret);
            self.input = v.into_iter().collect();
        }
    }

    /// Delete the word before the caret (`Ctrl+W`): the trailing whitespace, then the run of
    /// non-whitespace before it, so one press clears one word.
    pub fn input_delete_word(&mut self) {
        if !self.composing() {
            return;
        }
        let mut v: Vec<char> = self.input.chars().collect();
        let end = self.caret.min(v.len());
        let start = word_start(&v, end);
        v.drain(start..end);
        self.caret = start;
        self.input = v.into_iter().collect();
    }

    /// Delete from the start of the logical line to the caret (`Ctrl+U`).
    pub fn input_kill_to_start(&mut self) {
        if !self.composing() {
            return;
        }
        let mut v: Vec<char> = self.input.chars().collect();
        let end = self.caret.min(v.len());
        let start = line_start(&v, end);
        v.drain(start..end);
        self.caret = start;
        self.input = v.into_iter().collect();
    }

    /// Delete from the caret to the end of the logical line (`Ctrl+K`).
    pub fn input_kill_to_end(&mut self) {
        if !self.composing() {
            return;
        }
        let mut v: Vec<char> = self.input.chars().collect();
        let start = self.caret.min(v.len());
        let end = line_end(&v, start);
        v.drain(start..end);
        self.input = v.into_iter().collect();
    }

    /// Move the caret one character left / right.
    pub fn caret_left(&mut self) {
        if self.composing() {
            self.caret = self.caret.saturating_sub(1);
        }
    }
    pub fn caret_right(&mut self) {
        if self.composing() {
            self.caret = (self.caret + 1).min(self.input.chars().count());
        }
    }

    /// Move the caret to the start / end of the logical line (between newlines).
    pub fn caret_home(&mut self) {
        if self.composing() {
            let v: Vec<char> = self.input.chars().collect();
            self.caret = line_start(&v, self.caret.min(v.len()));
        }
    }
    pub fn caret_end(&mut self) {
        if self.composing() {
            let v: Vec<char> = self.input.chars().collect();
            self.caret = line_end(&v, self.caret.min(v.len()));
        }
    }

    /// Move the caret one word left / right.
    pub fn caret_word_left(&mut self) {
        if self.composing() {
            let v: Vec<char> = self.input.chars().collect();
            self.caret = word_start(&v, self.caret.min(v.len()));
        }
    }
    pub fn caret_word_right(&mut self) {
        if self.composing() {
            let v: Vec<char> = self.input.chars().collect();
            self.caret = word_end(&v, self.caret.min(v.len()));
        }
    }

    pub fn cancel_comment(&mut self) {
        self.leave_compose();
    }

    /// Leave compose mode, returning to the comments-list overlay if the compose was opened
    /// from it (and any comments remain), else to Normal.
    fn leave_compose(&mut self) {
        self.input.clear();
        self.caret = 0;
        let resume = std::mem::take(&mut self.resume_list);
        if resume && !self.store.is_empty() {
            self.list_cursor = self.list_cursor.min(self.store.len() - 1);
            self.mode = Mode::List;
        } else {
            self.mode = Mode::Normal;
        }
    }

    /// Save the in-progress comment — editing the existing one or anchoring a new one
    /// to the selection — then leave compose mode. Blank text cancels instead.
    pub fn submit_comment(&mut self) {
        let Mode::Composing { editing } = self.mode else { return };
        let text = self.input.trim().to_string();
        if text.is_empty() {
            self.cancel_comment();
            return;
        }
        match editing {
            Some(i) => {
                logln!("comment edit [{i}] :: {text}");
                self.store.edit(i, text);
                self.status = "comment updated".to_string();
            }
            None => {
                if let Some(c) = self.build_comment(text) {
                    logln!("comment add {} :: {}", c.location(), c.text);
                    self.store.add(c);
                    self.status = "comment added".to_string();
                }
            }
        }
        self.select_anchor = None;
        self.leave_compose();
    }

    /// Whether the selection has at least one content row a comment can attach to —
    /// a fold marker does not qualify.
    fn has_anchorable_selection(&self) -> bool {
        let (lo, hi) = self.selection_range();
        self.visible.get(lo..=hi).is_some_and(|s| s.iter().any(Row::is_content))
    }

    /// The `(side, start, end, snippet)` the current selection anchors to.
    fn selection_anchor(&self) -> Option<(Side, u32, u32, String)> {
        let (lo, hi) = self.selection_range();
        let selected: Vec<&Row> = self.visible.get(lo..=hi)?.iter().collect();
        anchor(&selected)
    }

    fn build_comment(&self, text: String) -> Option<Comment> {
        // Anchor to the file the open diff belongs to (`diff_path`), not the file-list
        // selection — they diverge if the list shifts under a comment in progress.
        let file = self.diff_path.clone()?;
        let (side, start, end, lines) = self.selection_anchor()?;
        Some(Comment { file, side, start, end, lines, text })
    }

    /// The `path:line` the composer is anchored to (selection for a new comment,
    /// the existing location when editing). `None` when not composing.
    pub fn pending_location(&self) -> Option<String> {
        match self.mode {
            Mode::Composing { editing: Some(i) } => self.store.get(i).map(Comment::location),
            Mode::Composing { editing: None } => {
                let file = self.diff_path.clone()?;
                let (side, start, end, _) = self.selection_anchor()?;
                Some(
                    Comment { file, side, start, end, lines: String::new(), text: String::new() }
                        .location(),
                )
            }
            Mode::Normal | Mode::List => None,
        }
    }

    /// Row indices on the open diff's file that a comment anchors to.
    pub fn commented_lines(&self) -> HashSet<usize> {
        let Some(file) = self.diff_path.clone() else {
            return HashSet::new();
        };
        self.visible
            .iter()
            .enumerate()
            .filter(|(_, row)| self.store.iter().any(|c| c.file == file && line_in(c, row)))
            .map(|(i, _)| i)
            .collect()
    }

    /// For each visible diff row, the store indices of comments whose card renders after it.
    /// A comment's card sits under the last visible row its line range covers, so the renderer
    /// can splice it inline (always visible) and the geometry stays anchored to a real row.
    pub fn comment_cards(&self) -> Vec<Vec<usize>> {
        let mut cards = vec![Vec::new(); self.visible.len()];
        let Some(file) = self.diff_path.as_deref() else { return cards };
        for (ci, c) in self.store.iter().enumerate() {
            if c.file == file
                && let Some(last) = self.visible.iter().rposition(|row| line_in(c, row))
            {
                cards[last].push(ci);
            }
        }
        cards
    }

    /// The store index to act on: the comment under the diff cursor, or — in the
    /// list overlay — the highlighted row.
    fn target_comment(&self) -> Option<usize> {
        if self.mode == Mode::List {
            return (self.list_cursor < self.store.len()).then_some(self.list_cursor);
        }
        self.comment_under_cursor()
    }

    /// The store index of a comment whose range covers the current diff row, if any.
    fn comment_under_cursor(&self) -> Option<usize> {
        let file = self.diff_path.as_deref()?;
        let row = self.visible.get(self.diff_cursor)?;
        self.store.iter().position(|c| c.file == file && line_in(c, row))
    }

    pub fn delete_comment(&mut self) {
        if let Some(i) = self.target_comment() {
            logln!("comment delete [{i}]");
            self.store.take(i);
            self.clamp_list_cursor();
            self.status = "comment deleted".to_string();
            // Don't strand the user in an empty "Comments (0)" overlay, matching `export`.
            if self.store.is_empty() {
                self.close_list();
            }
        }
    }

    /// Move the diff cursor to the next (`dir >= 0`) or previous commented line.
    pub fn jump_comment(&mut self, dir: isize) {
        let mut idxs: Vec<usize> = self.commented_lines().into_iter().collect();
        if idxs.is_empty() {
            return;
        }
        idxs.sort_unstable();
        self.focus = Focus::Diff;
        let cur = self.diff_cursor;
        let target = if dir >= 0 {
            idxs.iter().copied().find(|&i| i > cur).or_else(|| idxs.first().copied())
        } else {
            idxs.iter().rev().copied().find(|&i| i < cur).or_else(|| idxs.last().copied())
        };
        if let Some(t) = target {
            self.select_anchor = None; // a comment jump is navigation, not a selection extend
            self.diff_cursor = t;
            self.reveal_diff = true;
        }
    }

    pub fn open_list(&mut self) {
        if !self.store.is_empty() {
            self.list_cursor = 0;
            self.mode = Mode::List;
        }
    }

    pub fn close_list(&mut self) {
        if self.mode == Mode::List {
            self.mode = Mode::Normal;
        }
    }

    pub fn list_move(&mut self, delta: isize) {
        if self.mode == Mode::List && !self.store.is_empty() {
            self.list_cursor = step(self.list_cursor, delta, self.store.len());
        }
    }

    /// Send/copy every written comment to `target`; consume the whole set only on
    /// success. A failed export leaves all comments in place (`specs/review-model.md`).
    pub fn export(&mut self, target: &dyn ExportTarget) {
        if self.store.is_empty() {
            self.status = "no comments to send".to_string();
            return;
        }
        let refs: Vec<&Comment> = self.store.iter().collect();
        let text = format_all(&refs);
        let n = refs.len();
        logln!("export ({n}) -> {} ::\n{text}", target.label());
        match target.export(&text) {
            Ok(()) => {
                self.store.take_all();
                self.status = format!("sent {n} comment(s) to {}", target.label());
                logln!("export OK");
            }
            Err(e) => {
                self.status = format!("{} failed: {e}", target.label());
                logln!("export ERR: {e}");
            }
        }
        self.clamp_list_cursor();
        if self.store.is_empty() {
            self.close_list();
        }
    }

    /// Files that carry comments but are no longer in the changeset (anchors may be stale).
    pub fn stale_files(&self) -> HashSet<String> {
        let present: HashSet<&str> = self.files.iter().map(|f| f.path.as_str()).collect();
        self.store
            .iter()
            .map(|c| c.file.clone())
            .filter(|f| !present.contains(f.as_str()))
            .collect()
    }

    fn clamp_list_cursor(&mut self) {
        if self.list_cursor >= self.store.len() {
            self.list_cursor = self.store.len().saturating_sub(1);
        }
    }
}

/// Step `cur` by `delta` within `0..n`, clamping at both ends.
fn step(cur: usize, delta: isize, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let max = n - 1;
    if delta >= 0 {
        (cur + delta as usize).min(max)
    } else {
        cur.saturating_sub(delta.unsigned_abs())
    }
}

/// Move `scroll` the minimal amount so the row at `cursor` fits within a `viewport`-tall
/// window, given each row's display `heights`. Scrolls up when the cursor is above the top,
/// advances the top until the cursor's row fits, then pulls back so the bottom isn't left
/// blank — the shared "keep the cursor visible" rule for both panes (the file list passes
/// all-height-1 rows, where this degenerates to plain row arithmetic).
fn keep_in_view(cursor: usize, scroll: usize, heights: &[usize], viewport: usize) -> usize {
    if viewport == 0 || heights.is_empty() {
        return 0;
    }
    let cursor = cursor.min(heights.len() - 1);
    let mut top = scroll.min(cursor);
    while top < cursor && heights[top..=cursor].iter().sum::<usize>() > viewport {
        top += 1;
    }
    while top > 0 && heights[top - 1..].iter().sum::<usize>() <= viewport {
        top -= 1;
    }
    top
}

/// Clamp a scroll offset so a `viewport`-tall window over `total` rows shows no blank tail
/// (and 0 when the content fits). Called every frame after any reveal.
fn bound(scroll: usize, total: usize, viewport: usize) -> usize {
    scroll.min(total.saturating_sub(viewport))
}

/// The start of the logical line (after the previous `\n`, or 0) containing char `caret`.
fn line_start(v: &[char], caret: usize) -> usize {
    v[..caret].iter().rposition(|&c| c == '\n').map_or(0, |p| p + 1)
}

/// The end of the logical line (the next `\n`, or the end) containing char `caret`.
fn line_end(v: &[char], caret: usize) -> usize {
    v[caret..].iter().position(|&c| c == '\n').map_or(v.len(), |p| caret + p)
}

/// The start of the word before `caret`: skip trailing whitespace, then the word run.
fn word_start(v: &[char], caret: usize) -> usize {
    let mut i = caret;
    while i > 0 && v[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !v[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// The end of the word after `caret`: skip leading whitespace, then the word run.
fn word_end(v: &[char], caret: usize) -> usize {
    let mut i = caret;
    while i < v.len() && v[i].is_whitespace() {
        i += 1;
    }
    while i < v.len() && !v[i].is_whitespace() {
        i += 1;
    }
    i
}

/// Move a scroll offset by `delta` rows, saturating at 0. The upper bound is applied
/// separately by `bound` once the frame's viewport is known.
fn offset_by(scroll: usize, delta: isize) -> usize {
    if delta >= 0 {
        scroll.saturating_add(delta.unsigned_abs())
    } else {
        scroll.saturating_sub(delta.unsigned_abs())
    }
}

/// The working-tree content of `path`, lossily as UTF-8; empty when the file is
/// absent (a deletion) or unreadable.
fn worktree_content(repo: &std::path::Path, path: &str) -> String {
    std::fs::read(repo.join(path))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn line_in(c: &Comment, row: &Row) -> bool {
    let no = match c.side {
        Side::New => row.new_no(),
        Side::Old => row.old_no(),
    };
    no.is_some_and(|n| c.start <= n && n <= c.end)
}

/// Compute `(side, start, end, snippet)` for a selection of diff rows.
///
/// New-side numbers win when present (insertion/context rows); a pure deletion
/// anchors to the old side. The snippet keeps each row's `+`/`−`/space marker.
fn anchor(selected: &[&Row]) -> Option<(Side, u32, u32, String)> {
    // A selection may straddle a collapsed fold; anchor only over its content rows.
    let selected: Vec<&Row> = selected.iter().copied().filter(|r| r.is_content()).collect();
    if selected.is_empty() {
        return None;
    }
    let snippet = selected.iter().map(|r| r.marker_text()).collect::<Vec<_>>().join("\n");
    let new_nos: Vec<u32> = selected.iter().filter_map(|r| r.new_no()).collect();
    if let (Some(&min), Some(&max)) = (new_nos.iter().min(), new_nos.iter().max()) {
        return Some((Side::New, min, max, snippet));
    }
    let old_nos: Vec<u32> = selected.iter().filter_map(|r| r.old_no()).collect();
    let min = *old_nos.iter().min()?;
    let max = *old_nos.iter().max()?;
    Some((Side::Old, min, max, snippet))
}
