//! The file-list directory tree: the scope's changed files grouped into a collapsible
//! tree of directories and files, flattened to the rows the navigator paints.
//!
//! See `specs/file-list.md`. This module is pure — it turns a `&[ChangedFile]` plus the set
//! of collapsed directory paths into a flat `Vec<Row>`; selection, expansion state, and
//! rendering live in `app.rs` and `ui.rs`.

use std::collections::{BTreeMap, HashSet};
use std::hash::BuildHasher;

use crate::model::{ChangeKind, ChangedFile};

/// A visible row of the flattened tree: a directory or a file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    /// Nesting level, for indentation.
    pub depth: usize,
    /// The segment(s) shown — a directory name, a file basename, or a collapsed chain
    /// joined with `/` (single-child directories fold into their child).
    pub name: String,
    pub kind: RowKind,
}

/// What a [`Row`] is: a directory (togglable) or a file (opens the left pane).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RowKind {
    /// A directory: its full path keys its expansion state.
    Dir { path: String, expanded: bool },
    /// A file: its index into the source `&[Entry]`, plus its annotation when changed.
    File { index: usize, annotation: Option<Annotation> },
}

/// The change a file carries in the active scope, shown inline in the tree. Absent on an
/// unchanged `All files` file (specs/file-list.md).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Annotation {
    pub change: ChangeKind,
    pub additions: u32,
    pub deletions: u32,
}

/// The navigator's source row: a path, plus the rename source and the scope annotation when
/// it has one. `Changes` annotates every entry; `All files` annotates only changed files.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub path: String,
    pub previous_path: Option<String>,
    pub annotation: Option<Annotation>,
}

impl Entry {
    /// A `Changes` entry from a changed file: annotated and rename-aware.
    pub fn from_changed(f: &ChangedFile) -> Self {
        Self {
            path: f.path.clone(),
            previous_path: f.previous_path.clone(),
            annotation: Some(Annotation {
                change: f.kind,
                additions: f.additions,
                deletions: f.deletions,
            }),
        }
    }

    /// An `All files` entry from a bare path: no rename source, no annotation.
    pub fn plain(path: String) -> Self {
        Self { path, previous_path: None, annotation: None }
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

/// One directory node: its sub-directories and the files directly in it, both keyed by name
/// so iteration is alphabetical.
#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: BTreeMap<String, usize>,
}

/// Flatten `entries` into the visible tree rows. `default_expanded` sets a directory's
/// resting state — `true` for `Changes` (expanded unless toggled), `false` for `All files`
/// (collapsed unless toggled); `toggled` holds the paths flipped from that default.
/// Single-child directories fold into their child; directories sort before files,
/// alphabetically within a parent.
pub fn build<S: BuildHasher>(
    entries: &[Entry],
    toggled: &HashSet<String, S>,
    default_expanded: bool,
) -> Vec<Row> {
    let mut root = Dir::default();
    for (i, e) in entries.iter().enumerate() {
        insert(&mut root, &e.path, i);
    }
    let mut rows = Vec::new();
    flatten(&mut rows, &root, "", 0, toggled, default_expanded, entries);
    rows
}

/// Insert `path`'s file at `index` into the tree, creating directories along the way.
fn insert(root: &mut Dir, path: &str, index: usize) {
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(base) = segments.pop() else { return };
    let mut cur = root;
    for seg in segments {
        cur = cur.dirs.entry(seg.to_string()).or_default();
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
                kind: RowKind::Dir { path: path.clone(), expanded },
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

/// Follow single-child directory links from `start`, joining names with `/`, returning the
/// display name, full path, and the node where the chain stops (a real directory or a node
/// holding a single file).
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
    Row {
        depth,
        name,
        kind: RowKind::File { index, annotation: entries[index].annotation.clone() },
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") }
}

#[cfg(test)]
mod tests {
    use super::{Annotation, Entry, RowKind, build};
    use crate::model::{ChangeKind, ChangedFile};
    use std::collections::HashSet;

    fn file(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            kind: ChangeKind::Modified,
            additions: 1,
            deletions: 0,
            previous_path: None,
        }
    }

    fn entries(files: &[ChangedFile]) -> Vec<Entry> {
        files.iter().map(Entry::from_changed).collect()
    }

    /// Render the rows as `<depth>:<dir|file>:<name>` lines, for compact assertions. Uses the
    /// `Changes` default (expanded unless toggled).
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
        assert!(matches!(
            &rows[0].kind,
            RowKind::File { annotation: Some(Annotation { change: ChangeKind::Modified, .. }), .. }
        ));
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

    #[test]
    fn a_plain_entry_carries_no_annotation() {
        // An `All files` entry from a bare path renders without a marker or stats.
        let rows = build(&[Entry::plain("a.rs".into())], &HashSet::new(), false);
        assert!(matches!(rows[0].kind, RowKind::File { annotation: None, .. }));
    }
}
