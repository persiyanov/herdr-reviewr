//! Application state and transitions for the Changes review TUI.
//!
//! See `specs/tui.md` and `specs/review-model.md`. This module is terminal-free:
//! every method is a pure state transition or a read-only git/export call, so the
//! whole interaction model is testable without a backend. `src/main.rs` owns the
//! terminal and maps input events onto these methods.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;

use crate::export::{ExportTarget, format_all};
use crate::git::{self, DiffLine, DiffLineKind};
use crate::logln;
use crate::model::{ChangeKind, ChangedFile, Comment, CommentStore, Scope, Side};

/// Which pane has the keyboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Files,
    Diff,
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
#[derive(Debug)]
pub struct App {
    pub repo: PathBuf,
    pub base: Option<String>,
    pub scope: Scope,
    pub focus: Focus,
    pub files: Vec<ChangedFile>,
    pub file_cursor: usize,
    pub diff: Vec<DiffLine>,
    /// The file the open diff belongs to — the diff title, frozen with the diff
    /// while composing even if `file_cursor` drifts as the file list updates.
    pub diff_path: Option<String>,
    pub diff_cursor: usize,
    /// Top visible diff line. Sticky: only moves to keep the cursor in view, so the
    /// diff does not jump on every cursor step and drag-selection stays stable.
    pub diff_scroll: usize,
    pub select_anchor: Option<usize>,
    pub store: CommentStore,
    pub list_cursor: usize,
    pub mode: Mode,
    pub input: String,
    pub status: String,
    pub should_quit: bool,
}

impl App {
    pub fn new(repo: PathBuf, scope: Scope, base: Option<String>) -> Self {
        Self {
            repo,
            base,
            scope,
            focus: Focus::Files,
            files: Vec::new(),
            file_cursor: 0,
            diff: Vec::new(),
            diff_path: None,
            diff_cursor: 0,
            diff_scroll: 0,
            select_anchor: None,
            store: CommentStore::new(),
            list_cursor: 0,
            mode: Mode::Normal,
            input: String::new(),
            status: String::new(),
            should_quit: false,
        }
    }

    pub fn composing(&self) -> bool {
        matches!(self.mode, Mode::Composing { .. })
    }

    pub fn current_file(&self) -> Option<&ChangedFile> {
        self.files.get(self.file_cursor)
    }

    /// Reload the changed-files list and (unless composing) the open diff.
    ///
    /// Never touches the comment store or the in-progress input — that is the
    /// "a comment is never lost to a refresh" invariant (`specs/overview.md`).
    pub fn reload(&mut self) -> Result<()> {
        // Outside a git repo, show an empty state rather than failing (herdr-host.md).
        if !git::is_repo(&self.repo) {
            self.files.clear();
            self.file_cursor = 0;
            if !self.composing() {
                self.diff.clear();
                self.diff_path = None;
                self.diff_cursor = 0;
                self.diff_scroll = 0;
                self.select_anchor = None;
            }
            return Ok(());
        }
        let prev = self.current_file().map(|f| f.path.clone());
        self.files = git::changed_files(&self.repo, self.scope, self.base.as_deref())?;
        if let Some(path) = prev
            && let Some(i) = self.files.iter().position(|f| f.path == path)
        {
            self.file_cursor = i;
        }
        if self.file_cursor >= self.files.len() {
            self.file_cursor = self.files.len().saturating_sub(1);
        }
        // While composing, the open diff is frozen so the anchor under the comment
        // cannot shift beneath the writer.
        if !self.composing() {
            // A poll keeps the reader's position on the same file; only a different
            // file under the cursor resets the diff view to the top.
            if self.current_file().map(|f| f.path.as_str()) != self.diff_path.as_deref() {
                self.reset_diff_view();
            }
            self.load_diff()?;
        }
        Ok(())
    }

    /// Load the selected file's diff, clamping the existing cursor/scroll into it so a
    /// refresh keeps the reader's position rather than snapping to the top.
    fn load_diff(&mut self) -> Result<()> {
        let target = self.current_file().map(|f| (f.path.clone(), f.kind == ChangeKind::Untracked));
        self.diff_path = target.as_ref().map(|(path, _)| path.clone());
        self.diff = if let Some((path, untracked)) = target {
            let raw =
                git::file_diff(&self.repo, self.scope, &path, untracked, self.base.as_deref())?;
            git::parse_diff(&raw)
        } else {
            Vec::new()
        };
        if self.diff.is_empty() {
            self.reset_diff_view();
        } else {
            let last = self.diff.len() - 1;
            self.diff_cursor = self.diff_cursor.min(last);
            self.diff_scroll = self.diff_scroll.min(last);
            self.select_anchor = self.select_anchor.map(|a| a.min(last));
        }
        Ok(())
    }

    /// Snap the diff view back to the top, clearing any pending selection.
    fn reset_diff_view(&mut self) {
        self.diff_cursor = 0;
        self.diff_scroll = 0;
        self.select_anchor = None;
    }

    /// Keep `diff_scroll` so the cursor stays within a `height`-row viewport, scrolling
    /// only when the cursor would leave it. Called once per frame before drawing.
    pub fn clamp_diff_scroll(&mut self, height: usize) {
        if height == 0 || self.diff.is_empty() {
            self.diff_scroll = 0;
            return;
        }
        if self.diff_cursor < self.diff_scroll {
            self.diff_scroll = self.diff_cursor;
        } else if self.diff_cursor >= self.diff_scroll + height {
            self.diff_scroll = self.diff_cursor + 1 - height;
        }
        self.diff_scroll = self.diff_scroll.min(self.diff.len().saturating_sub(height));
    }

    /// Switch the changeset scope and reload. A no-op while composing, so a comment
    /// in progress is never stranded against a different diff.
    pub fn set_scope(&mut self, scope: Scope) -> Result<()> {
        if self.scope != scope && !self.composing() {
            self.scope = scope;
            self.file_cursor = 0;
            self.reset_diff_view();
            self.reload()?;
        }
        Ok(())
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Files => Focus::Diff,
            Focus::Diff => Focus::Files,
        };
    }

    /// Move the cursor in the focused pane; in the files pane this reloads the diff.
    pub fn move_cursor(&mut self, delta: isize) -> Result<()> {
        match self.focus {
            Focus::Files => {
                if !self.files.is_empty() {
                    let prev = self.file_cursor;
                    self.file_cursor = step(self.file_cursor, delta, self.files.len());
                    if self.file_cursor != prev {
                        self.reset_diff_view();
                        self.load_diff()?;
                    }
                }
            }
            Focus::Diff => {
                if !self.diff.is_empty() {
                    self.diff_cursor = step(self.diff_cursor, delta, self.diff.len());
                }
            }
        }
        Ok(())
    }

    /// Select the file at `index` (mouse click), reset the diff view, and load its diff.
    pub fn select_file(&mut self, index: usize) -> Result<()> {
        if index < self.files.len() {
            self.focus = Focus::Files;
            self.file_cursor = index;
            self.reset_diff_view();
            self.load_diff()?;
        }
        Ok(())
    }

    /// Move the diff cursor by `delta` lines (page keys, mouse wheel) regardless of
    /// which pane is focused; the sticky scroll follows. Does not steal focus.
    pub fn scroll_diff(&mut self, delta: isize) {
        if !self.diff.is_empty() {
            self.diff_cursor = step(self.diff_cursor, delta, self.diff.len());
        }
    }

    /// Extend a mouse drag-selection to the diff line at `index`, anchoring on first drag.
    pub fn drag_select_to(&mut self, index: usize) {
        if index < self.diff.len() {
            self.focus = Focus::Diff;
            if self.select_anchor.is_none() {
                self.select_anchor = Some(self.diff_cursor);
            }
            self.diff_cursor = index;
        }
    }

    /// Toggle a range-selection anchor at the current diff line.
    pub fn toggle_select(&mut self) {
        if self.focus == Focus::Diff && !self.diff.is_empty() {
            self.select_anchor = match self.select_anchor {
                Some(_) => None,
                None => Some(self.diff_cursor),
            };
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
            self.input.clear();
            self.mode = Mode::Composing { editing: None };
        }
    }

    pub fn start_edit(&mut self) {
        let Some(i) = self.target_comment() else { return };
        let Some(c) = self.store.get(i) else { return };
        let (file, side, start, end, text) =
            (c.file.clone(), c.side, c.start, c.end, c.text.clone());

        // Bring the comment's file into the diff and land the cursor on its line, so the
        // inline edit box opens over the comment — even when editing from the list.
        if self.diff_path.as_deref() != Some(file.as_str())
            && let Some(fi) = self.files.iter().position(|f| f.path == file)
        {
            self.file_cursor = fi;
            self.reset_diff_view();
            let _ = self.load_diff();
        }
        // Only move the cursor when the open diff is actually the comment's file, so a
        // stale comment (file gone from the changeset) never jumps the cursor onto a
        // same-numbered line in a different file.
        if self.diff_path.as_deref() == Some(file.as_str())
            && let Some(idx) = self.diff.iter().position(|dl| {
                let no = match side {
                    Side::New => dl.new_no,
                    Side::Old => dl.old_no,
                };
                no.is_some_and(|n| start <= n && n <= end)
            })
        {
            self.diff_cursor = idx;
            self.select_anchor = None;
        }
        self.focus = Focus::Diff;
        self.input = text;
        self.mode = Mode::Composing { editing: Some(i) };
    }

    pub fn input_push(&mut self, ch: char) {
        if self.composing() {
            self.input.push(ch);
        }
    }

    pub fn input_backspace(&mut self) {
        if self.composing() {
            self.input.pop();
        }
    }

    pub fn cancel_comment(&mut self) {
        self.input.clear();
        self.mode = Mode::Normal;
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
        self.input.clear();
        self.select_anchor = None;
        self.mode = Mode::Normal;
    }

    /// Whether the selection has at least one line a comment can attach to
    /// (added/removed/context, not a meta or hunk-header row).
    fn has_anchorable_selection(&self) -> bool {
        let (lo, hi) = self.selection_range();
        self.diff.get(lo..=hi).is_some_and(|s| s.iter().any(is_anchorable))
    }

    /// The `(side, start, end, snippet)` the current selection anchors to.
    fn selection_anchor(&self) -> Option<(Side, u32, u32, String)> {
        let (lo, hi) = self.selection_range();
        let selected: Vec<&DiffLine> = self.diff.get(lo..=hi)?.iter().collect();
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

    /// Diff-line indices on the open diff's file that a comment anchors to.
    pub fn commented_lines(&self) -> HashSet<usize> {
        let Some(file) = self.diff_path.clone() else {
            return HashSet::new();
        };
        self.diff
            .iter()
            .enumerate()
            .filter(|(_, dl)| self.store.iter().any(|c| c.file == file && line_in(c, dl)))
            .map(|(i, _)| i)
            .collect()
    }

    /// The store index to act on: the comment under the diff cursor, or — in the
    /// list overlay — the highlighted row.
    fn target_comment(&self) -> Option<usize> {
        if self.mode == Mode::List {
            return (self.list_cursor < self.store.len()).then_some(self.list_cursor);
        }
        self.comment_under_cursor()
    }

    /// The store index of a comment whose range covers the current diff line, if any.
    fn comment_under_cursor(&self) -> Option<usize> {
        let file = self.diff_path.as_deref()?;
        let dl = self.diff.get(self.diff_cursor)?;
        self.store.iter().position(|c| c.file == file && line_in(c, dl))
    }

    pub fn delete_comment(&mut self) {
        if let Some(i) = self.target_comment() {
            logln!("comment delete [{i}]");
            self.store.take(i);
            self.clamp_list_cursor();
            self.status = "comment deleted".to_string();
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
            self.diff_cursor = t;
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

fn is_anchorable(l: &DiffLine) -> bool {
    matches!(l.kind, DiffLineKind::Added | DiffLineKind::Removed | DiffLineKind::Context)
}

fn line_in(c: &Comment, dl: &DiffLine) -> bool {
    let no = match c.side {
        Side::New => dl.new_no,
        Side::Old => dl.old_no,
    };
    no.is_some_and(|n| c.start <= n && n <= c.end)
}

/// Compute `(side, start, end, snippet)` for a selection of diff lines.
///
/// New-side numbers win when present (added/context lines); a pure deletion
/// anchors to the old side. Meta and hunk-header lines are dropped from the snippet.
fn anchor(selected: &[&DiffLine]) -> Option<(Side, u32, u32, String)> {
    let body: Vec<&DiffLine> = selected.iter().copied().filter(|l| is_anchorable(l)).collect();
    if body.is_empty() {
        return None;
    }
    let snippet = body.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
    let new_nos: Vec<u32> = body.iter().filter_map(|l| l.new_no).collect();
    if let (Some(&min), Some(&max)) = (new_nos.iter().min(), new_nos.iter().max()) {
        return Some((Side::New, min, max, snippet));
    }
    let old_nos: Vec<u32> = body.iter().filter_map(|l| l.old_no).collect();
    let min = *old_nos.iter().min()?;
    let max = *old_nos.iter().max()?;
    Some((Side::Old, min, max, snippet))
}
