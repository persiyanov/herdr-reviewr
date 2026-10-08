//! The navigator's tree: entries grouped into collapsible directories, flattened to rows.

use std::collections::{BTreeMap, HashSet};
use std::hash::BuildHasher;

use crate::model::ChangedFile;

/// A visible row of the flattened tree: a directory or a file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    /// Nesting level, for indentation.
    pub depth: usize,
    /// A directory name, a basename, or a single-child chain joined with `/`.
    pub name: String,
    pub kind: RowKind,
    /// Whether git ignores this row's path — rendered dimmed in `All files`.
    pub ignored: bool,
}

/// What a [`Row`] is: a directory (togglable) or a file (opens the read pane).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RowKind {
    /// A directory, keyed by its path; `has_change` marks a collapsed folder holding a change.
    Dir { path: String, expanded: bool, has_change: bool },
    /// A file: its index into the source `&[Entry]`, which holds its change.
    File { index: usize },
}

/// A navigator entry: a path, and its change in the active scope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub path: String,
    pub annotation: Option<ChangedFile>,
    /// Whether git ignores this path — drives dimming in `All files`.
    pub ignored: bool,
    /// A wholly ignored directory whose children load on expand.
    pub is_dir: bool,
}

impl Entry {
    /// A `Changes` entry from a changed file: annotated and rename-aware.
    pub fn from_changed(f: &ChangedFile) -> Self {
        Self { path: f.path.clone(), annotation: Some(f.clone()), ignored: false, is_dir: false }
    }

    /// An `All files` entry from a worktree listing, annotated with its change if it has one.
    pub fn from_worktree(w: crate::git::WorktreeEntry, annotation: Option<ChangedFile>) -> Self {
        Self { path: w.path, annotation, ignored: w.ignored, is_dir: w.is_dir }
    }
}

impl Row {
    /// The source-file index when this row is a file; `None` for a directory.
    pub fn file_index(&self) -> Option<usize> {
        match self.kind {
            RowKind::File { index, .. } => Some(index),
            RowKind::Dir { .. } => None,
        }
    }

    /// The directory path when this row is a directory; `None` for a file.
    pub fn dir_path(&self) -> Option<&str> {
        match &self.kind {
            RowKind::Dir { path, .. } => Some(path),
            RowKind::File { .. } => None,
        }
    }
}

/// One directory node, its children keyed by name so they iterate alphabetically.
#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: BTreeMap<String, usize>,
    /// Created by an ignored-directory placeholder, so its row is dimmed.
    ignored: bool,
    /// Whether an annotated entry lies anywhere below, marked as entries are inserted.
    has_change: bool,
}

/// Flatten `entries` into visible rows; `toggled` holds directories flipped from `default_expanded`.
pub fn build<S: BuildHasher>(
    entries: &[Entry],
    toggled: &HashSet<String, S>,
    default_expanded: bool,
) -> Vec<Row> {
    let mut root = Dir::default();
    for (i, e) in entries.iter().enumerate() {
        insert(&mut root, e, i);
    }
    let mut rows = Vec::new();
    flatten(&mut rows, &root, "", 0, toggled, default_expanded, entries);
    rows
}

/// Insert `entry` into the tree, creating its directories; a placeholder marks its node ignored.
fn insert(root: &mut Dir, entry: &Entry, index: usize) {
    let mut segments: Vec<&str> = entry.path.split('/').filter(|s| !s.is_empty()).collect();
    if entry.is_dir {
        let mut cur = root;
        for seg in segments {
            cur = cur.dirs.entry(seg.to_string()).or_default();
        }
        cur.ignored = entry.ignored;
        return;
    }
    let Some(base) = segments.pop() else { return };
    let changed = entry.annotation.is_some();
    let mut cur = root;
    for seg in segments {
        cur = cur.dirs.entry(seg.to_string()).or_default();
        cur.has_change |= changed;
    }
    cur.files.insert(base.to_string(), index);
}

/// Emit `dir`'s children as rows: directories first (alphabetical), then files.
fn flatten<S: BuildHasher>(
    rows: &mut Vec<Row>,
    dir: &Dir,
    prefix: &str,
    depth: usize,
    toggled: &HashSet<String, S>,
    default_expanded: bool,
    entries: &[Entry],
) {
    for (name, sub) in &dir.dirs {
        let (display, path, node) = compress(name, join(prefix, name), sub);
        if let Some((fname, &index)) = lone_file(node) {
            // A single-child chain ending in one file folds into a file row, e.g. `a/b/x.rs`.
            rows.push(file_row(depth, format!("{display}/{fname}"), index, entries));
        } else {
            let expanded = default_expanded ^ toggled.contains(&path);
            rows.push(Row {
                depth,
                name: display,
                kind: RowKind::Dir { path: path.clone(), expanded, has_change: node.has_change },
                ignored: node.ignored,
            });
            if expanded {
                flatten(rows, node, &path, depth + 1, toggled, default_expanded, entries);
            }
        }
    }
    for (fname, &index) in &dir.files {
        rows.push(file_row(depth, fname.clone(), index, entries));
    }
}

/// Follow single-child directories from `start`: the joined name, path, and the node it stops at.
fn compress<'a>(name: &str, path: String, start: &'a Dir) -> (String, String, &'a Dir) {
    let mut display = name.to_string();
    let mut path = path;
    let mut node = start;
    while node.files.is_empty() && node.dirs.len() == 1 {
        let (child_name, child) = node.dirs.iter().next().expect("len == 1");
        display = format!("{display}/{child_name}");
        path = format!("{path}/{child_name}");
        node = child;
    }
    (display, path, node)
}

/// `Some((name, index))` when `node` holds exactly one file and no sub-directories.
fn lone_file(node: &Dir) -> Option<(&String, &usize)> {
    (node.dirs.is_empty() && node.files.len() == 1).then(|| node.files.iter().next().unwrap())
}

fn file_row(depth: usize, name: String, index: usize, entries: &[Entry]) -> Row {
    Row { depth, name, kind: RowKind::File { index }, ignored: entries[index].ignored }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") }
}

#[cfg(test)]
mod tests {
    use super::{Entry, RowKind, build};
    use crate::model::{ChangeKind, ChangedFile};
    use std::collections::HashSet;

    fn file(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            kind: ChangeKind::Modified,
            additions: 1,
            deletions: 0,
            previous_path: None,
            binary: false,
            old_size: 0,
            new_size: None,
            identity: crate::model::FileIdentity::fixture(),
        }
    }

    fn entries(files: &[ChangedFile]) -> Vec<Entry> {
        files.iter().map(Entry::from_changed).collect()
    }

    /// The rows as `<depth>:<dir|file>:<name>`, expanded unless toggled.
    fn shape(files: &[ChangedFile], collapsed: &HashSet<String>) -> Vec<String> {
        shape_rows(&build(&entries(files), collapsed, true))
    }

    fn shape_rows(rows: &[super::Row]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                let kind = if r.file_index().is_some() { "file" } else { "dir" };
                format!("{}:{}:{}", r.depth, kind, r.name)
            })
            .collect()
    }

    #[test]
    fn groups_files_into_directories_dirs_before_files() {
        let files = [file("src/app.rs"), file("src/ui.rs"), file("Cargo.toml")];
        let rows = shape(&files, &HashSet::new());
        assert_eq!(
            rows,
            ["0:dir:src", "1:file:app.rs", "1:file:ui.rs", "0:file:Cargo.toml"],
            "src/ groups before the top-level file"
        );
    }

    #[test]
    fn a_single_child_chain_folds_into_the_file() {
        // A chain of one-child directories collapses into one file row.
        let files = [file("docs/plans/2026/plan.md")];
        assert_eq!(shape(&files, &HashSet::new()), ["0:file:docs/plans/2026/plan.md"]);
    }

    #[test]
    fn a_single_child_directory_folds_but_a_branch_does_not() {
        // `a/b/` collapses (one child each) until `c/` branches into two files.
        let files = [file("a/b/c/one.rs"), file("a/b/c/two.rs")];
        let rows = shape(&files, &HashSet::new());
        assert_eq!(rows, ["0:dir:a/b/c", "1:file:one.rs", "1:file:two.rs"]);
    }

    #[test]
    fn a_collapsed_directory_hides_its_children() {
        let files = [file("src/app.rs"), file("src/ui.rs")];
        let collapsed: HashSet<String> = ["src".to_string()].into_iter().collect();
        assert_eq!(shape(&files, &collapsed), ["0:dir:src"], "children are hidden");
    }

    #[test]
    fn a_file_row_carries_its_source_index_and_stats() {
        let files = [file("z.rs"), file("a.rs")];
        let rows = build(&entries(&files), &HashSet::new(), true);
        // Sorted alphabetically: a.rs first → source index 1, then z.rs → index 0.
        assert_eq!(rows[0].file_index(), Some(1));
        assert_eq!(rows[1].file_index(), Some(0));
    }

    #[test]
    fn all_files_collapses_directories_by_default() {
        // default_expanded = false: src/ is collapsed unless toggled, so its children hide.
        let files = [file("src/app.rs"), file("src/ui.rs")];
        assert_eq!(shape_rows(&build(&entries(&files), &HashSet::new(), false)), ["0:dir:src"]);
        // Toggling src/ into the set expands it under the collapse-default policy.
        let toggled: HashSet<String> = ["src".to_string()].into_iter().collect();
        assert_eq!(
            shape_rows(&build(&entries(&files), &toggled, false)),
            ["0:dir:src", "1:file:app.rs", "1:file:ui.rs"]
        );
    }

    fn plain(path: &str) -> Entry {
        Entry { path: path.into(), annotation: None, ignored: false, is_dir: false }
    }

    fn has_change(row: &super::Row) -> bool {
        matches!(row.kind, RowKind::Dir { has_change: true, .. })
    }

    #[test]
    fn a_directory_row_marks_a_change_beneath_it() {
        // One changed file among unchanged siblings marks the folder.
        let mut es = entries(&[file("src/ui.rs")]);
        es.push(plain("src/app.rs"));
        es.push(plain("docs/a.md"));
        es.push(plain("docs/b.md"));
        let rows = build(&es, &HashSet::new(), false);
        assert_eq!(shape_rows(&rows), ["0:dir:docs", "0:dir:src"]);
        assert!(!has_change(&rows[0]), "docs/ holds no change");
        assert!(has_change(&rows[1]), "src/ holds ui.rs");

        // The mark is on the row in both states.
        let toggled: HashSet<String> = ["src".to_string()].into_iter().collect();
        let rows = build(&es, &toggled, false);
        assert_eq!(shape_rows(&rows), ["0:dir:docs", "0:dir:src", "1:file:app.rs", "1:file:ui.rs"]);
        assert!(has_change(&rows[1]), "expanded src/ still carries the mark");

        // A zero-line change of any kind (a pure rename, a binary) still counts.
        let mut zero = file("bin/x.png");
        zero.kind = ChangeKind::Renamed;
        zero.additions = 0;
        let mut es = vec![Entry::from_changed(&zero)];
        es.push(plain("bin/y.png"));
        let rows = build(&es, &HashSet::new(), false);
        assert!(has_change(&rows[0]), "a zero-line annotation marks the folder");

        // A change deep in a subtree marks every ancestor folder row.
        let mut es = entries(&[file("a/b/deep.rs")]);
        es.push(plain("a/top.rs"));
        es.push(plain("a/b/other.rs"));
        let toggled: HashSet<String> = ["a".to_string()].into_iter().collect();
        let rows = build(&es, &toggled, false);
        assert_eq!(shape_rows(&rows), ["0:dir:a", "1:dir:b", "1:file:top.rs"]);
        assert!(has_change(&rows[0]) && has_change(&rows[1]), "a/ and a/b/ both marked");
    }

    fn ignored_dir(path: &str) -> Entry {
        Entry { path: path.into(), annotation: None, ignored: true, is_dir: true }
    }

    #[test]
    fn an_ignored_dir_placeholder_renders_as_a_collapsed_ignored_row() {
        // A wholly ignored directory is one dimmed row until expanded.
        let rows = build(&[ignored_dir("target")], &HashSet::new(), false);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ignored, "the placeholder row is marked ignored (dimmed)");
        assert!(matches!(rows[0].kind, RowKind::Dir { expanded: false, has_change: false, .. }));
    }

    #[test]
    fn an_ignored_file_marks_its_row_ignored() {
        let entry =
            Entry { path: "build.log".into(), annotation: None, ignored: true, is_dir: false };
        let rows = build(&[entry], &HashSet::new(), false);
        assert!(rows[0].ignored, "an ignored file row is dimmed");
        assert!(matches!(rows[0].kind, RowKind::File { .. }));
    }
}
