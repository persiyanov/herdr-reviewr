//! In-memory review model: scopes, changed files, and comments.
//!
//! See `specs/review-model.md`. Comments live only for the session and are
//! removed by export or delete — never by a refresh.

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
    pub fn toggled(self) -> Self {
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

/// The in-memory comment list for one worktree review session.
#[derive(Default, Debug)]
pub struct CommentStore {
    items: Vec<Comment>,
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

    pub fn iter(&self) -> impl Iterator<Item = &Comment> {
        self.items.iter()
    }

    pub fn get(&self, index: usize) -> Option<&Comment> {
        self.items.get(index)
    }

    /// Append a comment; returns its index.
    pub fn add(&mut self, comment: Comment) -> usize {
        self.items.push(comment);
        self.items.len() - 1
    }

    /// Replace the text of the comment at `index`. Returns `false` if out of range.
    pub fn edit(&mut self, index: usize, text: String) -> bool {
        if let Some(c) = self.items.get_mut(index) {
            c.text = text;
            true
        } else {
            false
        }
    }

    /// Remove and return the comment at `index` (delete, or consume one on export).
    pub fn take(&mut self, index: usize) -> Option<Comment> {
        if index < self.items.len() { Some(self.items.remove(index)) } else { None }
    }

    /// Remove and return every comment (consume-all on a successful export).
    pub fn take_all(&mut self) -> Vec<Comment> {
        std::mem::take(&mut self.items)
    }
}

#[cfg(test)]
mod tests {
    use super::{Comment, CommentStore, Scope, Side};

    fn comment(file: &str, start: u32, end: u32, text: &str) -> Comment {
        Comment {
            file: file.into(),
            side: Side::New,
            start,
            end,
            lines: "+x".into(),
            text: text.into(),
        }
    }

    #[test]
    fn scope_toggles_and_labels() {
        // The chip click cycles through all three scopes and wraps.
        assert_eq!(Scope::Uncommitted.toggled(), Scope::Branch);
        assert_eq!(Scope::Branch.toggled(), Scope::LastTurn);
        assert_eq!(Scope::LastTurn.toggled(), Scope::Uncommitted);
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
        assert_eq!(s.get(i).unwrap().text, "first");
        assert!(s.edit(i, "second".into()));
        assert_eq!(s.get(i).unwrap().text, "second");
        assert!(!s.edit(99, "nope".into()));
    }

    #[test]
    fn take_one_and_take_all_consume() {
        let mut s = CommentStore::new();
        s.add(comment("a.rs", 1, 1, "one"));
        s.add(comment("b.rs", 2, 2, "two"));
        let taken = s.take(0).unwrap();
        assert_eq!(taken.text, "one");
        assert_eq!(s.len(), 1);
        let rest = s.take_all();
        assert_eq!(rest.len(), 1);
        assert!(s.is_empty());
        assert!(s.take(0).is_none());
    }
}
