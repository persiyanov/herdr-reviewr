//! The structured diff model: a file's changes as rows built from its old and new
//! content, syntax-highlighted, ready to paint.
//!
//! See `specs/diff-view.md`. This module is terminal-free — a `Span` carries an RGB
//! color, and `src/ui.rs` maps it to a ratatui color.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

use similar::{ChangeTag, TextDiff};

use crate::highlight::Highlighter;

/// An 8-bit RGB color.
pub type Rgb = (u8, u8, u8);

/// A run of one line's text in a single color.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub text: String,
    pub color: Rgb,
}

/// A rendered diff row. Content rows (`Context`/`Deletion`/`Insertion`) are selectable
/// for comments; a `Fold` is a collapsed run of context lines it owns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Row {
    Context { old_no: u32, new_no: u32, spans: Vec<Span> },
    Deletion { old_no: u32, spans: Vec<Span>, emphasis: Vec<CharRange> },
    Insertion { new_no: u32, spans: Vec<Span>, emphasis: Vec<CharRange> },
    Fold { lines: Vec<Row> },
}

/// A `[start, end)` run of char indices within a line, for word-level emphasis.
pub type CharRange = (u32, u32);

impl Row {
    pub fn old_no(&self) -> Option<u32> {
        match self {
            Row::Context { old_no, .. } | Row::Deletion { old_no, .. } => Some(*old_no),
            Row::Insertion { .. } | Row::Fold { .. } => None,
        }
    }

    pub fn new_no(&self) -> Option<u32> {
        match self {
            Row::Context { new_no, .. } | Row::Insertion { new_no, .. } => Some(*new_no),
            Row::Deletion { .. } | Row::Fold { .. } => None,
        }
    }

    pub fn spans(&self) -> &[Span] {
        match self {
            Row::Context { spans, .. }
            | Row::Deletion { spans, .. }
            | Row::Insertion { spans, .. } => spans,
            Row::Fold { .. } => &[],
        }
    }

    /// The char ranges within this line that differ from its paired counterpart; empty
    /// on context, folds, and unpaired change lines.
    pub fn emphasis(&self) -> &[CharRange] {
        match self {
            Row::Deletion { emphasis, .. } | Row::Insertion { emphasis, .. } => emphasis,
            Row::Context { .. } | Row::Fold { .. } => &[],
        }
    }

    /// The diff marker for this row: `' '`, `'-'`, or `'+'`; `' '` for a fold.
    pub fn marker(&self) -> char {
        match self {
            Row::Deletion { .. } => '-',
            Row::Insertion { .. } => '+',
            Row::Context { .. } | Row::Fold { .. } => ' ',
        }
    }

    /// Whether this row anchors a comment — every kind but a fold.
    pub fn is_content(&self) -> bool {
        !matches!(self, Row::Fold { .. })
    }

    /// The hidden line count of a fold, else 0.
    pub fn hidden(&self) -> usize {
        match self {
            Row::Fold { lines } => lines.len(),
            _ => 0,
        }
    }

    /// A fold's stable identity across rebuilds: the line number of its first hidden
    /// line. `None` for any other row.
    pub fn fold_anchor(&self) -> Option<u32> {
        match self {
            Row::Fold { lines } => lines.first().and_then(|r| r.new_no().or_else(|| r.old_no())),
            _ => None,
        }
    }

    /// The line's plain text, joined from its spans.
    pub fn text(&self) -> String {
        self.spans().iter().map(|s| s.text.as_str()).collect()
    }

    /// The line as a marker-prefixed diff line, for the export snippet.
    pub fn marker_text(&self) -> String {
        format!("{}{}", self.marker(), self.text())
    }
}

/// Whether the file renders as rows, or a notice instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileState {
    Normal,
    Binary,
    TooLarge,
}

/// The selected file modeled as rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileDiff {
    pub path: String,
    /// The old path when this file was renamed, for the `old → new` header; `None` otherwise.
    pub previous_path: Option<String>,
    pub state: FileState,
    pub rows: Vec<Row>,
}

/// A file beyond either budget renders as `too_large` rather than stalling the diff —
/// the byte budget also catches a single huge line that the line budget misses.
const MAX_LINES: usize = 50_000;
const MAX_BYTES: usize = 2_000_000;

impl FileDiff {
    /// An empty placeholder, for when no file is selected.
    pub fn empty() -> Self {
        Self {
            path: String::new(),
            previous_path: None,
            state: FileState::Normal,
            rows: Vec::new(),
        }
    }

    /// Build the model from `old` and `new` content, highlighting with `hl`. `previous_path`
    /// is the rename source, surfaced in the header; `None` for every other change.
    pub fn build(
        path: String,
        previous_path: Option<String>,
        old: &str,
        new: &str,
        hl: &Highlighter,
    ) -> Self {
        let language = language_of(&path);
        let notice = |state| Self {
            path: path.clone(),
            previous_path: previous_path.clone(),
            state,
            rows: Vec::new(),
        };
        if old.contains('\0') || new.contains('\0') {
            return notice(FileState::Binary);
        }
        if old.len() + new.len() > MAX_BYTES
            || old.lines().count() + new.lines().count() > MAX_LINES
        {
            return notice(FileState::TooLarge);
        }

        let lang = language.as_deref();
        let old_spans = hl.highlight(old, lang);
        let new_spans = hl.highlight(new, lang);
        let line = |spans: &[Vec<Span>], i: usize| spans.get(i).cloned().unwrap_or_default();

        let mut rows = Vec::new();
        for change in TextDiff::from_lines(old, new).iter_all_changes() {
            match change.tag() {
                ChangeTag::Equal => {
                    let (oi, ni) = (change.old_index().unwrap(), change.new_index().unwrap());
                    rows.push(Row::Context {
                        old_no: oi as u32 + 1,
                        new_no: ni as u32 + 1,
                        spans: line(&new_spans, ni),
                    });
                }
                ChangeTag::Delete => {
                    let oi = change.old_index().unwrap();
                    rows.push(Row::Deletion {
                        old_no: oi as u32 + 1,
                        spans: line(&old_spans, oi),
                        emphasis: Vec::new(),
                    });
                }
                ChangeTag::Insert => {
                    let ni = change.new_index().unwrap();
                    rows.push(Row::Insertion {
                        new_no: ni as u32 + 1,
                        spans: line(&new_spans, ni),
                        emphasis: Vec::new(),
                    });
                }
            }
        }
        compute_emphasis(&mut rows);
        Self { path, previous_path, state: FileState::Normal, rows: collapse_context(&rows) }
    }
}

/// Fill word-level `emphasis` on the related deletion/insertion lines of each change block
/// (a run of deletions immediately followed by a run of insertions). Rather than pairing by
/// position — which mis-pairs unrelated lines when a block rewrites several lines at once —
/// each deletion greedily searches forward for its *homolog*: the first not-yet-claimed
/// insertion similar enough to be the same line edited (see [`pair_homologs`], after
/// git-delta's `infer_edits`). Lines with no homolog stay unemphasized, carrying only their
/// red/green; emphasis then points at a real edit instead of flooding a wholesale rewrite.
fn compute_emphasis(rows: &mut [Row]) {
    let mut i = 0;
    while i < rows.len() {
        let del_start = i;
        while i < rows.len() && matches!(rows[i], Row::Deletion { .. }) {
            i += 1;
        }
        let ins_start = i;
        while i < rows.len() && matches!(rows[i], Row::Insertion { .. }) {
            i += 1;
        }
        pair_homologs(rows, del_start..ins_start, ins_start..i);
        // No change block started here; step over the context/fold row.
        if del_start == i {
            i += 1;
        }
    }
}

/// Pair each deletion in `dels` with its homolog insertion in `inss` and set both lines'
/// emphasis. Greedy forward scan: deletion `d` takes the first insertion at or after the
/// last-claimed one whose similarity clears [`MIN_SIMILARITY`]; insertions skipped along the
/// way are abandoned (they were inserts, not edits of `d`). A deletion with no qualifying
/// insertion is left unpaired. Mirrors git-delta's homolog inference.
fn pair_homologs(rows: &mut [Row], dels: std::ops::Range<usize>, inss: std::ops::Range<usize>) {
    let mut next_ins = inss.start;
    for d in dels {
        let old = rows[d].text();
        let mut p = next_ins;
        while p < inss.end {
            let new = rows[p].text();
            let (ratio, old_e, new_e) = word_emphasis(&old, &new);
            if ratio >= MIN_SIMILARITY {
                if let Row::Deletion { emphasis, .. } = &mut rows[d] {
                    *emphasis = old_e;
                }
                if let Row::Insertion { emphasis, .. } = &mut rows[p] {
                    *emphasis = new_e;
                }
                next_ins = p + 1;
                break;
            }
            p += 1;
        }
    }
}

/// Two lines below this similarity are taken to be different lines, not one line edited, so
/// they are never paired for inline emphasis (see [`pair_homologs`]). git-delta's equivalent
/// `max_line_distance` defaults to 0.6 *distance* — the complementary metric — but we sit
/// stricter because over-highlighting a rewrite is worse than missing a marginal edit: a pair
/// that only shares a syntactic skeleton (a reformat, or two different `let`s) scatters
/// unhelpful fragments. Empirically marginal pairs land near ~0.6–0.65 and genuine edits near
/// ~0.71–0.78, so the bar sits in the gap.
const MIN_SIMILARITY: f32 = 0.7;

/// The word-level similarity of `(old, new)` and the char ranges that changed: the words
/// present only in `old` (deletion emphasis) and only in `new` (insertion emphasis). The
/// caller gates on the ratio (see [`pair_homologs`]); the ranges are meaningful only once it
/// clears the bar. Adjacent changed words separated only by whitespace coalesce into one
/// block (see [`coalesce_ws_gaps`]), so a changed phrase reads as a single span, not fragments.
fn word_emphasis(old: &str, new: &str) -> (f32, Vec<CharRange>, Vec<CharRange>) {
    let diff = TextDiff::from_words(old, new);
    let (mut old_ranges, mut new_ranges) = (Vec::new(), Vec::new());
    let (mut old_pos, mut new_pos) = (0u32, 0u32);
    for change in diff.iter_all_changes() {
        let len = change.value().chars().count() as u32;
        match change.tag() {
            ChangeTag::Equal => {
                old_pos += len;
                new_pos += len;
            }
            ChangeTag::Delete => {
                push_range(&mut old_ranges, old_pos, len);
                old_pos += len;
            }
            ChangeTag::Insert => {
                push_range(&mut new_ranges, new_pos, len);
                new_pos += len;
            }
        }
    }
    let old_e = trim_range_edges(coalesce_ws_gaps(old_ranges, old), old);
    let new_e = trim_range_edges(coalesce_ws_gaps(new_ranges, new), new);
    (diff.ratio(), old_e, new_e)
}

/// Shrink each emphasis range off its leading and trailing whitespace, dropping any range
/// that is all whitespace. So a highlight hugs the changed tokens — never bare indentation
/// (a reformat that only deepened the indent) and never the space before an added trailing
/// comment. Interior whitespace from [`coalesce_ws_gaps`] survives, since it is not an edge.
fn trim_range_edges(ranges: Vec<CharRange>, text: &str) -> Vec<CharRange> {
    let chars: Vec<char> = text.chars().collect();
    ranges
        .into_iter()
        .filter_map(|(mut a, mut b)| {
            while a < b && chars[a as usize].is_whitespace() {
                a += 1;
            }
            while b > a && chars[b as usize - 1].is_whitespace() {
                b -= 1;
            }
            (a < b).then_some((a, b))
        })
        .collect()
}

/// Merge consecutive emphasis ranges whose in-between text is all whitespace, swallowing
/// that whitespace into the highlight. A run of changed words then shows as one block
/// rather than separate words with bare gaps; leading and trailing indentation, and gaps
/// holding any non-space character, are left out because they never sit between two changes.
fn coalesce_ws_gaps(ranges: Vec<CharRange>, text: &str) -> Vec<CharRange> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<CharRange> = Vec::new();
    for (start, end) in ranges {
        match out.last_mut() {
            Some(last)
                if chars[last.1 as usize..start as usize].iter().all(|c| c.is_whitespace()) =>
            {
                last.1 = end;
            }
            _ => out.push((start, end)),
        }
    }
    out
}

/// Append `[pos, pos+len)`, merging into the previous range when they touch.
fn push_range(ranges: &mut Vec<CharRange>, pos: u32, len: u32) {
    if len == 0 {
        return;
    }
    match ranges.last_mut() {
        Some(last) if last.1 == pos => last.1 = pos + len,
        _ => ranges.push((pos, pos + len)),
    }
}

/// Context lines kept adjacent to each change; longer unchanged runs collapse to a fold.
const FOLD_MARGIN: usize = 3;

/// Replace each run of unchanged `Context` rows that exceeds the margin with a single
/// `Fold` owning the hidden rows, keeping `FOLD_MARGIN` lines next to every change and
/// at the file head and tail.
fn collapse_context(rows: &[Row]) -> Vec<Row> {
    let n = rows.len();
    let mut keep = vec![false; n];
    for (i, row) in rows.iter().enumerate() {
        if matches!(row, Row::Context { .. }) {
            continue;
        }
        let lo = i.saturating_sub(FOLD_MARGIN);
        let hi = (i + FOLD_MARGIN).min(n - 1);
        keep[lo..=hi].iter_mut().for_each(|k| *k = true);
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if keep[i] {
            out.push(rows[i].clone());
            i += 1;
            continue;
        }
        let start = i;
        while i < n && !keep[i] {
            i += 1;
        }
        // A single hidden line is shown as-is — a `⋯ 1 line` fold would save nothing.
        if i - start > 1 {
            out.push(Row::Fold { lines: rows[start..i].to_vec() });
        } else {
            out.extend(rows[start..i].iter().cloned());
        }
    }
    out
}

/// The extension used to pick a syntax, e.g. `rs` for `src/app.rs`; `None` when the
/// file name has no extension.
fn language_of(path: &str) -> Option<String> {
    Path::new(path).extension().and_then(|e| e.to_str()).map(str::to_string)
}

/// Caches built `FileDiff`s by content, so an unchanged poll skips diffing and
/// highlighting.
#[derive(Default, Debug)]
pub struct DiffCache {
    entries: HashMap<String, (u64, FileDiff)>,
}

/// Cap the cache so a long session browsing many files cannot grow it without bound;
/// at the cap it is cleared (only the open file is ever rebuilt).
const CACHE_CAP: usize = 256;

impl DiffCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached diff when `old`/`new`/`previous_path` are unchanged for `path`,
    /// else build, cache, and return it. `previous_path` (the rename source) is part of the
    /// key, so the same path appearing as a rename in one scope and plain in another never
    /// returns a stale header; the cache is also cleared on a scope switch (`set_scope`).
    pub fn get(
        &mut self,
        path: String,
        previous_path: Option<String>,
        old: &str,
        new: &str,
        hl: &Highlighter,
    ) -> FileDiff {
        let key = content_hash(previous_path.as_deref(), old, new);
        if let Some((cached, diff)) = self.entries.get(&path)
            && *cached == key
        {
            return diff.clone();
        }
        let diff = FileDiff::build(path.clone(), previous_path, old, new, hl);
        if self.entries.len() >= CACHE_CAP {
            self.entries.clear();
        }
        self.entries.insert(path, (key, diff.clone()));
        diff
    }
}

fn content_hash(previous_path: Option<&str>, old: &str, new: &str) -> u64 {
    let mut h = DefaultHasher::new();
    previous_path.hash(&mut h);
    old.hash(&mut h);
    new.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::{DiffCache, FileDiff, FileState, Row, language_of};
    use crate::highlight::Highlighter;

    fn build(old: &str, new: &str) -> FileDiff {
        let hl = Highlighter::new(None);
        FileDiff::build("a.rs".into(), None, old, new, &hl)
    }

    #[test]
    fn cache_keys_on_previous_path_so_a_rename_and_a_plain_edit_differ() {
        let hl = Highlighter::new(None);
        let mut cache = DiffCache::new();
        // Same path + same content, but one is a rename (carries a previous_path) and one is
        // not. The cache must not return the rename's build for the plain edit.
        let renamed = cache.get("f.rs".into(), Some("old.rs".into()), "x\n", "y\n", &hl);
        let plain = cache.get("f.rs".into(), None, "x\n", "y\n", &hl);
        assert_eq!(renamed.previous_path.as_deref(), Some("old.rs"));
        assert_eq!(plain.previous_path, None);
    }

    #[test]
    fn rows_carry_sides_numbers_and_markers() {
        let d = build("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(d.state, FileState::Normal);
        let del = d.rows.iter().find(|r| matches!(r, Row::Deletion { .. })).unwrap();
        let ins = d.rows.iter().find(|r| matches!(r, Row::Insertion { .. })).unwrap();
        assert_eq!(del.old_no(), Some(2));
        assert_eq!(del.new_no(), None);
        assert_eq!(ins.new_no(), Some(2));
        assert_eq!(del.marker_text(), "-beta");
        assert_eq!(ins.marker_text(), "+BETA");
        // The whole file is shown — context rows surround the change.
        assert!(d.rows.iter().filter(|r| matches!(r, Row::Context { .. })).count() >= 2);
    }

    #[test]
    fn long_unchanged_runs_collapse_to_a_fold() {
        use std::fmt::Write as _;
        let mut old = String::new();
        for i in 0..40 {
            writeln!(old, "line {i}").unwrap();
        }
        let new = old.replace("line 20", "LINE 20");
        let d = build(&old, &new);
        // The middle is one change with 3 context lines each side; the long head and tail
        // unchanged runs each collapse to a fold.
        let folds = d.rows.iter().filter(|r| matches!(r, Row::Fold { .. })).count();
        assert_eq!(folds, 2, "leading and trailing runs fold");
        let change = d.rows.iter().find(|r| matches!(r, Row::Insertion { .. })).unwrap();
        assert_eq!(change.new_no(), Some(21)); // line 20 is 1-based line 21
    }

    #[test]
    fn word_emphasis_marks_only_the_changed_words() {
        let d = build("let x = foo(a);\n", "let x = bar(a, b);\n");
        let del = d.rows.iter().find(|r| matches!(r, Row::Deletion { .. })).unwrap();
        let ins = d.rows.iter().find(|r| matches!(r, Row::Insertion { .. })).unwrap();
        // Both lines share the `let x = ` and `(a` prefix; `foo`→`bar` and the `, b` are
        // the only emphasized spans, never the whole line.
        assert!(!del.emphasis().is_empty() && !ins.emphasis().is_empty());
        let covers = |row: &Row, needle: &str| {
            let text = row.text();
            row.emphasis().iter().any(|&(a, b)| {
                let seg: String = text.chars().skip(a as usize).take((b - a) as usize).collect();
                seg.contains(needle)
            })
        };
        assert!(covers(del, "foo"), "deletion emphasizes the removed word");
        assert!(covers(ins, "bar"), "insertion emphasizes the new word");
        // `let x = ` is shared, so it is never emphasized.
        assert!(!covers(del, "let"));
    }

    #[test]
    fn adjacent_changed_words_coalesce_across_whitespace() {
        // `Hi You` → `Hello There`: two changed words split by a space. The emphasis is a
        // single block spanning the space, not two fragments — the Word-Alt look.
        let d = build("greet Hi You here\n", "greet Hello There here\n");
        let del = d.rows.iter().find(|r| matches!(r, Row::Deletion { .. })).unwrap();
        let ins = d.rows.iter().find(|r| matches!(r, Row::Insertion { .. })).unwrap();
        let seg = |row: &Row, &(a, b): &(u32, u32)| -> String {
            row.text().chars().skip(a as usize).take((b - a) as usize).collect()
        };
        assert_eq!(del.emphasis().len(), 1, "the removed phrase is one block");
        assert_eq!(seg(del, &del.emphasis()[0]), "Hi You");
        assert_eq!(ins.emphasis().len(), 1, "the new phrase is one block");
        assert_eq!(seg(ins, &ins.emphasis()[0]), "Hello There");
    }

    #[test]
    fn emphasis_pairs_a_deletion_with_its_homolog_not_its_position() {
        // One line edited, with a new line inserted above it. Positional pairing would pair
        // the deletion with the inserted comment (dissimilar → nothing); homolog search skips
        // the comment and pairs with the real edit, so `compute`→`computeSum` lights up and
        // the unrelated inserted line stays plain.
        let d = build("let total = compute();\n", "// added\nlet total = computeSum();\n");
        let seg = |row: &Row| -> String {
            let (a, b) = row.emphasis()[0];
            row.text().chars().skip(a as usize).take((b - a) as usize).collect()
        };
        let del = d.rows.iter().find(|r| matches!(r, Row::Deletion { .. })).unwrap();
        let comment = d.rows.iter().find(|r| r.text() == "// added").unwrap();
        let edited = d.rows.iter().find(|r| r.text() == "let total = computeSum();").unwrap();
        assert_eq!(seg(del), "compute();", "the deletion emphasizes its real edit");
        assert_eq!(seg(edited), "computeSum();", "its homolog insertion is the one emphasized");
        assert!(comment.emphasis().is_empty(), "the unrelated inserted line stays plain");
    }

    #[test]
    fn emphasis_hugs_the_tokens_not_surrounding_whitespace() {
        // Adding a trailing comment: the highlight is `// note`, not ` // note` — leading
        // whitespace is trimmed off the range so it never paints bare spaces.
        let d = build("    let x = 1;\n", "    let x = 1; // note\n");
        let ins = d.rows.iter().find(|r| matches!(r, Row::Insertion { .. })).unwrap();
        assert_eq!(ins.emphasis().len(), 1);
        let (a, b) = ins.emphasis()[0];
        let seg: String = ins.text().chars().skip(a as usize).take((b - a) as usize).collect();
        assert_eq!(seg, "// note", "emphasis hugs the comment, no leading space");
    }

    #[test]
    fn a_reformat_or_unrelated_pair_is_not_emphasized() {
        // A one-liner reformatted to multi-line, and two different statements sharing a
        // `let … ;` skeleton, both fall below the similarity bar — they scatter unhelpful
        // fragments otherwise. Each keeps only its line-level red/green.
        let reformat = build(
            "    rows.push(Row::Deletion { old_no: oi + 1, spans: s });\n",
            "    rows.push(Row::Deletion {\n        old_no: oi + 1,\n        spans: s,\n    });\n",
        );
        let unrelated = build(
            "    let start = scroll.min(len.sub(height));\n",
            "    let target = (row - inner.y) as usize;\n",
        );
        for d in [reformat, unrelated] {
            assert!(
                d.rows.iter().all(|r| r.emphasis().is_empty()),
                "no inline emphasis on a sub-threshold pair"
            );
        }
    }

    #[test]
    fn a_wholesale_line_rewrite_gets_no_word_emphasis() {
        // Two unrelated lines that merely share `///` and punctuation must not light up:
        // the line-level red/green already says they changed, and full-line emphasis on a
        // dissimilar pair is noise. The similarity gate suppresses it.
        let d = build(
            "/// Keep diff_scroll so the cursor stays within the viewport\n",
            "/// Scroll the diff horizontally by delta columns\n",
        );
        let del = d.rows.iter().find(|r| matches!(r, Row::Deletion { .. })).unwrap();
        let ins = d.rows.iter().find(|r| matches!(r, Row::Insertion { .. })).unwrap();
        assert!(del.emphasis().is_empty(), "dissimilar deletion is not emphasized");
        assert!(ins.emphasis().is_empty(), "dissimilar insertion is not emphasized");
    }

    #[test]
    fn an_unpaired_change_line_has_no_emphasis() {
        // One deletion, two insertions: line 0 pairs; the extra insertion stays plain.
        let d = build("alpha\n", "ALPHA\nbeta\n");
        let extra = d
            .rows
            .iter()
            .find(|r| matches!(r, Row::Insertion { .. }) && r.text() == "beta")
            .unwrap();
        assert!(extra.emphasis().is_empty(), "the unpaired insertion is not emphasized");
    }

    #[test]
    fn binary_content_is_flagged_not_rowed() {
        let d = build("ok\n", "bin\0ary\n");
        assert_eq!(d.state, FileState::Binary);
        assert!(d.rows.is_empty());
    }

    #[test]
    fn language_comes_from_the_extension() {
        assert_eq!(language_of("src/app.rs").as_deref(), Some("rs"));
        assert_eq!(language_of("Makefile"), None);
        assert_eq!(language_of("a/b.tar.gz").as_deref(), Some("gz"));
    }

    #[test]
    fn cache_reuses_an_unchanged_build() {
        let hl = Highlighter::new(None);
        let mut cache = DiffCache::new();
        let d1 = cache.get("a.rs".into(), None, "x\n", "y\n", &hl);
        let d2 = cache.get("a.rs".into(), None, "x\n", "y\n", &hl);
        assert_eq!(d1, d2);
    }
}
