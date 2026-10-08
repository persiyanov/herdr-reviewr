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
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CommitPick {
    pub oldest: String,
    pub newest: String,
}

/// The semantic namespace for reviewed files. Resolved endpoints stay in each identity, so a
/// moving ref invalidates files without creating another recoverable context.
#[derive(Clone, Default, PartialEq, Eq, Hash, Debug)]
pub enum ReviewContext {
    #[default]
    Uncommitted,
    Branch {
        base: Option<String>,
    },
    LastTurn {
        baseline: Option<String>,
    },
    Commits {
        pick: Option<CommitPick>,
    },
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
}

/// The exact comparison behind a changed-file row. It stays opaque outside Git; other code may
/// only compare and retain it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FileIdentity(std::sync::Arc<FileIdentityParts>);

#[derive(Clone, PartialEq, Eq, Hash)]
struct FileIdentityParts {
    old_endpoint: String,
    new_endpoint: String,
    kind: ChangeKind,
    path: String,
    previous_path: Option<String>,
    old_mode: String,
    new_mode: String,
    old_content: String,
    new_content: String,
    binary: bool,
    live_new_side: bool,
}

impl std::fmt::Debug for FileIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileIdentity(..)")
    }
}

/// Inputs kept at the Git boundary while constructing an opaque [`FileIdentity`].
#[derive(Clone, Copy)]
pub(crate) struct FileIdentityInput<'a> {
    pub old_endpoint: &'a str,
    pub new_endpoint: &'a str,
    pub kind: ChangeKind,
    pub path: &'a str,
    pub previous_path: Option<&'a str>,
    pub old_mode: &'a str,
    pub new_mode: &'a str,
    pub old_content: &'a str,
    pub new_content: &'a str,
    pub binary: bool,
    pub live_new_side: bool,
}

impl FileIdentity {
    pub(crate) fn from_git(input: FileIdentityInput<'_>) -> Self {
        let old_side_absent = input.old_mode == "000000";
        Self(std::sync::Arc::new(FileIdentityParts {
            old_endpoint: input.old_endpoint.to_string(),
            new_endpoint: input.new_endpoint.to_string(),
            kind: if old_side_absent { ChangeKind::Added } else { input.kind },
            path: input.path.to_string(),
            previous_path: input.previous_path.map(str::to_string),
            old_mode: input.old_mode.to_string(),
            new_mode: input.new_mode.to_string(),
            old_content: if old_side_absent {
                "absent".to_string()
            } else {
                input.old_content.to_string()
            },
            new_content: input.new_content.to_string(),
            binary: input.binary,
            live_new_side: input.live_new_side,
        }))
    }

    pub(crate) fn with_loaded_worktree_fingerprint(&self, mode: &str, content: &str) -> Self {
        if !self.uses_live_worktree() {
            return self.clone();
        }
        let mut parts = (*self.0).clone();
        parts.new_mode = mode.to_string();
        parts.new_content = content.to_string();
        Self(std::sync::Arc::new(parts))
    }

    /// The same file comparison under a newer aggregate snapshot endpoint.
    pub(crate) fn with_new_endpoint(&self, endpoint: &str) -> Self {
        let mut parts = (*self.0).clone();
        parts.new_endpoint = endpoint.to_string();
        Self(std::sync::Arc::new(parts))
    }

    /// Whether this file's two sides are unchanged, even if another file moved the snapshot tree.
    pub(crate) fn same_file_comparison(&self, other: &Self) -> bool {
        let (a, b) = (&self.0, &other.0);
        a.old_endpoint == b.old_endpoint
            && a.kind == b.kind
            && a.path == b.path
            && a.previous_path == b.previous_path
            && a.old_mode == b.old_mode
            && a.new_mode == b.new_mode
            && a.old_content == b.old_content
            && a.new_content == b.new_content
            && a.binary == b.binary
            && a.live_new_side == b.live_new_side
    }

    pub(crate) fn uses_live_worktree(&self) -> bool {
        self.0.live_new_side
    }

    /// Whether two comparisons share the same old file and can compare base-anchored edits.
    pub(crate) fn same_old_side(&self, other: &Self) -> bool {
        let (a, b) = (&self.0, &other.0);
        a.old_endpoint == b.old_endpoint
            && a.path == b.path
            && a.previous_path == b.previous_path
            && a.old_mode == b.old_mode
            && a.old_content == b.old_content
    }

    pub(crate) fn new_side_absent(&self) -> bool {
        self.0.new_mode == "000000"
    }

    pub(crate) fn is_landed_deletion(&self) -> bool {
        self.0.kind == ChangeKind::Deleted && self.new_side_absent()
    }

    pub(crate) fn live_side_certified(&self) -> bool {
        !self.uses_live_worktree() || self.is_landed_deletion() || !self.new_side_absent()
    }

    pub(crate) fn is_dirty_gitlink(&self) -> bool {
        self.new_is_gitlink() && !self.0.new_content.ends_with(":..")
    }

    #[cfg(test)]
    pub(crate) fn old_endpoint(&self) -> &str {
        &self.0.old_endpoint
    }

    #[cfg(test)]
    pub(crate) fn has_new_side(&self) -> bool {
        self.0.new_mode != "000000"
    }

    pub(crate) fn new_is_gitlink(&self) -> bool {
        self.0.new_mode == "160000"
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self::from_git(FileIdentityInput {
            old_endpoint: "fixture-old",
            new_endpoint: "fixture-new",
            kind: ChangeKind::Modified,
            path: "fixture",
            previous_path: None,
            old_mode: "100644",
            new_mode: "100644",
            old_content: "fixture-old",
            new_content: "fixture-new",
            binary: false,
            live_new_side: false,
        })
    }
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
    /// The exact comparison which produced this row.
    pub identity: FileIdentity,
}

impl ChangedFile {
    /// Whether `paths` cover this file or the source it was renamed from.
    pub fn touched_by<'a>(&self, paths: impl IntoIterator<Item = &'a String> + Clone) -> bool {
        crate::git::covered(paths.clone(), &self.path)
            || self.previous_path.as_deref().is_some_and(|p| crate::git::covered(paths, p))
    }
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
