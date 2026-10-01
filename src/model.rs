//! In-memory review model: scopes, changed files, and comments.
//!
//! Comments live only for the session and are
//! removed by export or delete — never by a refresh.

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

    /// The scope's name in the specs and in config values (`default_scope`): kebab-case,
    /// unlike the header chip's spaced `label`.
    pub fn name(self) -> &'static str {
        match self {
            Scope::Uncommitted => "uncommitted",
            Scope::Branch => "branch",
            Scope::LastTurn => "last-turn",
            Scope::Commits => "commits",
        }
    }

    /// Cycle to the next scope, for the header chip click: uncommitted → branch → last turn →
    /// commits.
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

/// The `commits` scope's pick: a contiguous run from `oldest` to `newest`, both full commit
/// ids, equal for a run of one.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CommitPick {
    pub oldest: String,
    pub newest: String,
}

/// The semantic namespace in which a human reviews changed files. Resolved commit/tree
/// endpoints stay in each [`FileIdentity`], so a moving ref invalidates files without turning
/// the old generation into a separately recoverable review context.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ReviewContext {
    Uncommitted,
    Branch { base: Option<String> },
    LastTurn { baseline: Option<String> },
    Commits { pick: Option<CommitPick> },
}

impl CommitPick {
    pub fn single(sha: &str) -> Self {
        Self { oldest: sha.to_string(), newest: sha.to_string() }
    }

    pub fn is_single(&self) -> bool {
        self.oldest == self.newest
    }
}

/// Where a comment's diff was read: the worktree, or the picked run it came from. A diff
/// comment renders only while the active scope reads the same diff, both sides: a run and
/// its newest commit alone share a new side but not an old one.
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
    Untracked,
}

/// The exact comparison that produced one changed-file row.
///
/// Its representation is deliberately private: UI and authored-state code may compare and
/// retain identities, but Git remains the sole authority for constructing them.
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
        Self(std::sync::Arc::new(FileIdentityParts {
            old_endpoint: input.old_endpoint.to_string(),
            new_endpoint: input.new_endpoint.to_string(),
            kind: input.kind,
            path: input.path.to_string(),
            previous_path: input.previous_path.map(str::to_string),
            old_mode: input.old_mode.to_string(),
            new_mode: input.new_mode.to_string(),
            old_content: input.old_content.to_string(),
            new_content: input.new_content.to_string(),
            binary: input.binary,
            live_new_side: input.live_new_side,
        }))
    }

    /// Reproduce this comparison identity with the worktree side a reader actually loaded.
    /// Committed comparisons have no live side, so their identity is already exact.
    #[cfg(test)]
    pub(crate) fn with_loaded_worktree(&self, mode: &str, content: &[u8]) -> Self {
        if !self.uses_live_worktree() {
            return self.clone();
        }
        self.with_loaded_worktree_fingerprint(mode, &content_fingerprint(content))
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

    pub(crate) fn uses_live_worktree(&self) -> bool {
        self.0.live_new_side
    }

    pub(crate) fn old_endpoint(&self) -> &str {
        &self.0.old_endpoint
    }

    pub(crate) fn new_endpoint(&self) -> &str {
        &self.0.new_endpoint
    }

    pub(crate) fn has_old_side(&self) -> bool {
        self.0.old_mode != "000000"
    }

    pub(crate) fn has_new_side(&self) -> bool {
        self.0.new_mode != "000000"
    }

    pub(crate) fn old_gitlink_oid(&self) -> Option<&str> {
        (self.0.old_mode == "160000").then_some(self.0.old_content.as_str())
    }

    pub(crate) fn new_is_gitlink(&self) -> bool {
        self.0.new_mode == "160000"
    }

    pub(crate) fn committed_new_gitlink_oid(&self) -> Option<&str> {
        (self.0.new_mode == "160000" && !self.uses_live_worktree())
            .then_some(self.0.new_content.as_str())
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

pub(crate) fn content_fingerprint(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
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
    /// Git's own no-text-diff verdict for this change: binary content, or a path whose
    /// `diff` attribute `.gitattributes` unsets. The pane reads it as the `binary` notice
    /// without reading either side.
    pub binary: bool,
    /// Opaque identity of the exact old/new comparison represented by this row.
    pub identity: FileIdentity,
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
    /// (the `All files` tab). Selects how staleness is judged.
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
}
