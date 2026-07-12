//! In-memory review model: scopes, changed files, and comments.
//!
//! See `specs/review-model.md`. A comment's lifecycle (id, author, status) lives in
//! [`crate::comments::StoredComment`]; `CommentStore` is the TUI-session view over a `Vec` of
//! them. A refresh never drops a comment — only delete, or an external agent removing its
//! file, does (`crate::app::App::sync_comments_from_disk`).

use crate::comments::{Author, Status, StoredComment, new_id, now_iso};

/// Which set of changes the Changes view shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Uncommitted,
    Branch,
    LastTurn,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Uncommitted => "uncommitted",
            Scope::Branch => "branch",
            Scope::LastTurn => "last turn",
        }
    }

    /// Cycle to the next scope, for the header chip click: uncommitted → branch → last turn.
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Scope::Uncommitted => Scope::Branch,
            Scope::Branch => Scope::LastTurn,
            Scope::LastTurn => Scope::Uncommitted,
        }
    }
}

/// How a file changed within a scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

impl ChangeKind {
    pub fn marker(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
            ChangeKind::Renamed => 'R',
            ChangeKind::Untracked => '?',
        }
    }
}

/// A row in the Changes list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChangedFile {
    pub path: String,
    pub kind: ChangeKind,
    pub additions: u32,
    pub deletions: u32,
    /// The old path of a renamed file; `None` for every other kind. Its old content lives
    /// at this path, so a rename diffs real content instead of reading as all-insertion.
    pub previous_path: Option<String>,
}

/// Which side of the diff a comment's lines live on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    New,
    Old,
}

/// A reviewer comment anchored to a run of diff lines, carrying the snippet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Comment {
    pub file: String,
    pub side: Side,
    pub start: u32,
    pub end: u32,
    /// Verbatim diff lines the comment anchors to, each keeping its `+`/`-`/space marker.
    pub lines: String,
    pub text: String,
    /// True when anchored to a diff (the `Changes` tab); false for a File-view content comment
    /// (the `All files` tab). Selects how staleness is judged (specs/review-model.md).
    pub diff_anchored: bool,
}

impl Comment {
    /// The `path:start-end` (or `path:line`) location, with ` (removed)` when old-side.
    pub fn location(&self) -> String {
        let range = if self.start == self.end {
            format!("{}:{}", self.file, self.start)
        } else {
            format!("{}:{}-{}", self.file, self.start, self.end)
        };
        match self.side {
            Side::New => range,
            Side::Old => format!("{range} (removed)"),
        }
    }
}

/// The in-memory comment list for one worktree review session: every entry carries its
/// lifecycle metadata (id/author/status) from the moment it is written, so a TUI-authored
/// comment is indistinguishable in shape from one synced in from the on-disk store.
#[derive(Default, Debug)]
pub struct CommentStore {
    items: Vec<StoredComment>,
}

impl CommentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &StoredComment> {
        self.items.iter()
    }

    pub fn get(&self, index: usize) -> Option<&StoredComment> {
        self.items.get(index)
    }

    /// The current index of the comment with id `id`, or `None` if it no longer exists. Every
    /// action held across a poll tick (an edit in progress, an overlay keystroke) must re-resolve
    /// through this rather than trust a previously-read index — `sync_comments_from_disk` can
    /// replace and re-sort the whole set between the moment an index was read and the moment it
    /// is used, so a stale index can silently name a different (or no) comment.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|sc| sc.id == id)
    }

    /// Append a comment written just now in the TUI, wrapping it with fresh lifecycle
    /// metadata (a new id, `Author::User`, `Status::Open`) — every comment a reviewer writes
    /// starts here (`specs/review-model.md`). Returns its index.
    pub fn add(&mut self, comment: Comment) -> usize {
        self.items.push(StoredComment {
            id: new_id(),
            author: Author::User,
            status: Status::Open,
            created_at: now_iso(),
            comment,
        });
        self.items.len() - 1
    }

    /// Replace the text of the comment at `index`. Returns `false` if out of range.
    pub fn edit(&mut self, index: usize, text: String) -> bool {
        if let Some(c) = self.items.get_mut(index) {
            c.comment.text = text;
            true
        } else {
            false
        }
    }

    /// Flip the status of the comment at `index`. Returns `false` if out of range.
    pub fn set_status(&mut self, index: usize, status: Status) -> bool {
        if let Some(c) = self.items.get_mut(index) {
            c.status = status;
            true
        } else {
            false
        }
    }

    /// Remove and return the comment at `index` (the only way a comment leaves the set from
    /// the TUI side — sending no longer consumes; `specs/review-model.md`).
    pub fn take(&mut self, index: usize) -> Option<StoredComment> {
        if index < self.items.len() { Some(self.items.remove(index)) } else { None }
    }

    /// Replace the whole set — the result of `App::sync_comments_from_disk`'s merge.
    pub fn replace(&mut self, items: Vec<StoredComment>) {
        self.items = items;
    }
}

#[cfg(test)]
mod tests {
    use super::{Author, Comment, CommentStore, Scope, Side, Status};

    fn comment(file: &str, start: u32, end: u32, text: &str) -> Comment {
        Comment {
            file: file.into(),
            side: Side::New,
            start,
            end,
            lines: "+x".into(),
            text: text.into(),
            diff_anchored: true,
        }
    }

    #[test]
    fn scope_cycles_and_labels() {
        // The chip click cycles through all three scopes and wraps.
        assert_eq!(Scope::Uncommitted.cycle(), Scope::Branch);
        assert_eq!(Scope::Branch.cycle(), Scope::LastTurn);
        assert_eq!(Scope::LastTurn.cycle(), Scope::Uncommitted);
        assert_eq!(Scope::Uncommitted.label(), "uncommitted");
        assert_eq!(Scope::LastTurn.label(), "last turn");
    }

    #[test]
    fn location_formats_range_single_and_removed() {
        let mut c = comment("a.rs", 40, 52, "x");
        assert_eq!(c.location(), "a.rs:40-52");
        c.end = 40;
        assert_eq!(c.location(), "a.rs:40");
        c.side = Side::Old;
        assert_eq!(c.location(), "a.rs:40 (removed)");
    }

    #[test]
    fn add_get_edit() {
        let mut s = CommentStore::new();
        let i = s.add(comment("a.rs", 1, 1, "first"));
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(i).unwrap().comment.text, "first");
        assert!(s.edit(i, "second".into()));
        assert_eq!(s.get(i).unwrap().comment.text, "second");
        assert!(!s.edit(99, "nope".into()));
    }

    #[test]
    fn add_wraps_fresh_lifecycle_metadata() {
        let mut s = CommentStore::new();
        let i = s.add(comment("a.rs", 1, 1, "first"));
        let sc = s.get(i).unwrap();
        assert!(sc.id.starts_with("c-"), "a fresh id: {}", sc.id);
        assert_eq!(sc.author, Author::User);
        assert_eq!(sc.status, Status::Open);
    }

    #[test]
    fn set_status_flips_in_place_and_reports_out_of_range() {
        let mut s = CommentStore::new();
        let i = s.add(comment("a.rs", 1, 1, "one"));
        assert!(s.set_status(i, Status::Resolved));
        assert_eq!(s.get(i).unwrap().status, Status::Resolved);
        assert!(!s.set_status(99, Status::Open));
    }

    #[test]
    fn take_removes_one_and_replace_rebuilds_the_set() {
        let mut s = CommentStore::new();
        s.add(comment("a.rs", 1, 1, "one"));
        s.add(comment("b.rs", 2, 2, "two"));
        let taken = s.take(0).unwrap();
        assert_eq!(taken.comment.text, "one");
        assert_eq!(s.len(), 1);
        assert!(s.take(5).is_none());
        s.replace(Vec::new());
        assert!(s.is_empty());
    }
}
