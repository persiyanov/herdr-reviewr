//! The review model: scopes, changed files, and the session's comments, never lost to a refresh.

/// Which set of changes the Changes view shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Uncommitted,
    Branch,
    LastTurn,
    /// A picked run of commits, diffed `A^` against `B`.
    Commits,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Uncommitted => "uncommitted",
            Scope::Branch => "branch",
            Scope::LastTurn => "last turn",
            Scope::Commits => "commits",
        }
    }

    /// The kebab-case name config values use (`default_scope`).
    pub fn name(self) -> &'static str {
        match self {
            Scope::Uncommitted => "uncommitted",
            Scope::Branch => "branch",
            Scope::LastTurn => "last-turn",
            Scope::Commits => "commits",
        }
    }

    /// The next scope, for a click on the header chip.
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Scope::Uncommitted => Scope::Branch,
            Scope::Branch => Scope::LastTurn,
            Scope::LastTurn => Scope::Commits,
            Scope::Commits => Scope::Uncommitted,
        }
    }
}

/// The `commits` pick: the run `oldest..=newest`, full ids, equal for a run of one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommitPick {
    pub oldest: String,
    pub newest: String,
}

impl CommitPick {
    pub fn single(sha: &str) -> Self {
        Self { oldest: sha.to_string(), newest: sha.to_string() }
    }

    pub fn is_single(&self) -> bool {
        self.oldest == self.newest
    }
}

/// Where a comment's diff was read; it renders only while the scope reads that same diff.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Rev {
    Worktree,
    Commit(CommitPick),
}

/// How a file changed within a scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
}

impl ChangeKind {
    pub fn marker(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
            ChangeKind::Renamed => 'R',
            ChangeKind::Copied => 'C',
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
    /// The source path of a renamed or copied file, whose old content lives there.
    pub previous_path: Option<String>,
    /// git's no-text-diff verdict, read as the binary notice.
    pub binary: bool,
    /// The bytes git stores on the old side, 0 where there is none.
    pub old_size: u64,
    /// The bytes git stores on the new side, 0 where there is none; `None` for the worktree.
    pub new_size: Option<u64>,
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
    /// A diff comment (`Changes`), else a content comment (`All files`).
    pub diff_anchored: bool,
    /// Where the new side was read.
    pub rev: Rev,
    /// Set on a rework note: the agent rewords this PR draft note. `lines` quotes its body.
    pub draft: Option<DraftRef>,
}

/// The PR draft note a rework note targets.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DraftRef {
    pub forge: crate::git::Forge,
    pub number: u64,
    pub draft_id: u64,
    /// The draft's `path:line` anchor. `None` for a general draft.
    pub anchor: Option<String>,
    pub reply: bool,
}

impl DraftRef {
    /// The rework note for this draft: `body` quoted, `text` the reviewer's suggestion.
    pub fn note(self, body: &str, text: String) -> Comment {
        let lines = body
            .lines()
            .map(|line| format!("> {line}").trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        Comment {
            file: String::new(),
            side: Side::New,
            start: 0,
            end: 0,
            lines,
            text,
            diff_anchored: false,
            rev: Rev::Worktree,
            draft: Some(self),
        }
    }

    fn location(&self) -> String {
        let (n, id) = (self.number, self.draft_id);
        let mut out = match (self.forge, self.reply) {
            (crate::git::Forge::GitLab, false) => format!("MR !{n} draft note {id}"),
            (crate::git::Forge::GitLab, true) => format!("MR !{n} draft reply {id}"),
            (_, false) => format!("PR #{n} pending comment {id}"),
            (_, true) => format!("PR #{n} pending reply {id}"),
        };
        if let Some(anchor) = &self.anchor {
            out.push_str(" at ");
            out.push_str(anchor);
        }
        out
    }
}

impl Comment {
    /// The `path:start-end` (or `path:line`) location, with ` (removed)` when old-side.
    pub fn location(&self) -> String {
        if let Some(draft) = &self.draft {
            return draft.location();
        }
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

/// The session's comments, each with an id never reused, so a reference never lands on another.
#[derive(Default, Debug)]
pub struct CommentStore {
    items: Vec<Comment>,
    ids: Vec<u64>,
    next_id: u64,
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
        self.ids.push(self.next_id);
        self.next_id += 1;
        self.items.len() - 1
    }

    /// The id of the comment at `index`.
    pub fn id(&self, index: usize) -> Option<u64> {
        self.ids.get(index).copied()
    }

    /// The index of the comment with id `id`, while the store holds it.
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.ids.iter().position(|&i| i == id)
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
        if index < self.items.len() {
            self.ids.remove(index);
            Some(self.items.remove(index))
        } else {
            None
        }
    }

    /// Remove and return every comment (consume-all on a successful export).
    pub fn take_all(&mut self) -> Vec<Comment> {
        self.ids.clear();
        std::mem::take(&mut self.items)
    }
}

#[cfg(test)]
mod tests {
    use super::{Comment, CommentStore, Rev, Scope, Side};

    fn comment(file: &str, start: u32, end: u32, text: &str) -> Comment {
        Comment {
            file: file.into(),
            side: Side::New,
            start,
            end,
            lines: "+x".into(),
            text: text.into(),
            diff_anchored: true,
            rev: Rev::Worktree,
            draft: None,
        }
    }

    #[test]
    fn scope_cycles_and_labels() {
        // The chip click cycles through all four scopes and wraps.
        assert_eq!(Scope::Uncommitted.cycle(), Scope::Branch);
        assert_eq!(Scope::Branch.cycle(), Scope::LastTurn);
        assert_eq!(Scope::LastTurn.cycle(), Scope::Commits);
        assert_eq!(Scope::Commits.cycle(), Scope::Uncommitted);
        assert_eq!(Scope::Uncommitted.label(), "uncommitted");
        assert_eq!(Scope::LastTurn.label(), "last turn");
        assert_eq!(Scope::Commits.label(), "commits");
        assert_eq!(Scope::Commits.name(), "commits");
    }

    #[test]
    fn a_pick_of_one_is_single() {
        let pick = super::CommitPick::single("abc");
        assert!(pick.is_single());
        assert!(!super::CommitPick { oldest: "a".into(), newest: "b".into() }.is_single());
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

    #[test]
    fn ids_are_never_reused_and_survive_an_edit() {
        let mut s = CommentStore::new();
        s.add(comment("a.rs", 1, 1, "one"));
        let two = s.add(comment("a.rs", 2, 2, "two"));
        let id = s.id(two).unwrap();
        s.take(0);
        assert_eq!(s.index_of(id), Some(0), "the list shifted, the id followed");
        s.edit(0, "edited".into());
        assert_eq!(s.id(0), Some(id));
        s.take_all();
        let three = s.add(comment("a.rs", 3, 3, "three"));
        assert_ne!(s.id(three), Some(id), "a fresh comment never takes an old id");
        assert_eq!(s.index_of(id), None);
    }
}
