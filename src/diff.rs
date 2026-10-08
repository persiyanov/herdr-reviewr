//! The diff model: a file's changes as highlighted rows, terminal-free.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

use similar::{Algorithm, ChangeTag, DiffTag, TextDiff, capture_diff_slices};

use crate::highlight::Highlighter;
use crate::model::FileIdentity;
use crate::text::{line_body, lines};

/// An 8-bit RGB color.
pub type Rgb = (u8, u8, u8);

/// A run of one line's text in a single color.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub text: String,
    pub color: Rgb,
}

/// A diff row: a content row takes comments, a `Fold` owns the context lines it hides.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Row {
    Context {
        old_no: u32,
        new_no: u32,
        spans: Vec<Span>,
    },
    /// `cr`: the line's kept CR changed, painted as a marker, never part of the text.
    Deletion {
        old_no: u32,
        spans: Vec<Span>,
        emphasis: Vec<CharRange>,
        cr: bool,
    },
    Insertion {
        new_no: u32,
        spans: Vec<Span>,
        emphasis: Vec<CharRange>,
        cr: bool,
    },
    Fold {
        lines: Vec<Row>,
    },
    /// One pre-wrapped line of a rendered markdown view; `src..=src_end` is its unit's source range.
    Rendered {
        src: u32,
        src_end: u32,
        text: String,
        kind: RenderedKind,
    },
}

/// One changed line's stable identity within a diff against the same old side.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum DiffLine {
    Old(u32),
    New(u32),
}

impl DiffLine {
    pub(crate) fn of(row: &Row) -> Option<Self> {
        match row {
            Row::Deletion { old_no, .. } => Some(Self::Old(*old_no)),
            Row::Insertion { new_no, .. } => Some(Self::New(*new_no)),
            Row::Context { .. } | Row::Fold { .. } | Row::Rendered { .. } => None,
        }
    }
}

/// A base-anchored replacement retained when its file is marked reviewed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ReviewedEdit {
    old_start: u32,
    old_len: u32,
    new_fingerprints: Vec<u64>,
}

/// One current edit and the rows that paint it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct DiffEdit {
    reviewed: ReviewedEdit,
    old_lines: std::ops::Range<u32>,
    new_lines: std::ops::Range<u32>,
}

/// What a rendered row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderedKind {
    /// A block's line. `source` and `wrap` (`None` for the gap above) are its identity across
    /// rebuilds; `hides` counts changed lines under a collapsed `<details>`.
    Block { source: (u32, u32), wrap: Option<u32>, line: u32, bar: Option<Bar>, hides: Option<u32> },
    /// A marker for `lines` changed lines no block shows; `gone` when a block was deleted whole.
    Marker { kind: MarkerKind, lines: u32, gone: bool },
}

/// A changed block's bar: `Added` when it only gained lines, `Modified` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bar {
    Added,
    Modified,
}

/// A marker row's kind: a block deleted whole, or changed source that renders nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MarkerKind {
    Removed,
    Unrendered,
}

/// A `[start, end)` run of char indices within a line, for word-level emphasis.
pub type CharRange = (u32, u32);

impl Row {
    pub fn old_no(&self) -> Option<u32> {
        match self {
            Row::Context { old_no, .. } | Row::Deletion { old_no, .. } => Some(*old_no),
            Row::Insertion { .. } | Row::Fold { .. } | Row::Rendered { .. } => None,
        }
    }

    pub fn new_no(&self) -> Option<u32> {
        match self {
            Row::Context { new_no, .. } | Row::Insertion { new_no, .. } => Some(*new_no),
            // A rendered line is no source line: its unit names its range.
            Row::Deletion { .. } | Row::Fold { .. } | Row::Rendered { .. } => None,
        }
    }

    pub fn spans(&self) -> &[Span] {
        match self {
            Row::Context { spans, .. }
            | Row::Deletion { spans, .. }
            | Row::Insertion { spans, .. } => spans,
            Row::Rendered { .. } | Row::Fold { .. } => &[],
        }
    }

    /// The char ranges that differ from the paired line; empty when unpaired.
    pub fn emphasis(&self) -> &[CharRange] {
        match self {
            Row::Deletion { emphasis, .. } | Row::Insertion { emphasis, .. } => emphasis,
            Row::Context { .. } | Row::Fold { .. } | Row::Rendered { .. } => &[],
        }
    }

    /// Whether the paint ends this line in a CR marker: its ending changed.
    pub fn cr_marker(&self) -> bool {
        matches!(self, Row::Deletion { cr: true, .. } | Row::Insertion { cr: true, .. })
    }

    /// The diff marker for this row: `' '`, `'-'`, or `'+'`; `' '` for a fold.
    pub fn marker(&self) -> char {
        match self {
            Row::Deletion { .. } => '-',
            Row::Insertion { .. } => '+',
            Row::Context { .. } | Row::Fold { .. } | Row::Rendered { .. } => ' ',
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

    /// A fold's identity across rebuilds: its first hidden line's number.
    pub fn fold_anchor(&self) -> Option<u32> {
        match self {
            Row::Fold { lines } => lines.first().and_then(|r| r.new_no().or_else(|| r.old_no())),
            _ => None,
        }
    }

    /// The line's plain text, joined from its spans.
    pub fn text(&self) -> String {
        match self {
            Row::Rendered { text, .. } => text.clone(),
            _ => self.spans().iter().map(|s| s.text.as_str()).collect(),
        }
    }

    /// The line as a marker-prefixed diff line, for the export snippet.
    pub fn marker_text(&self) -> String {
        format!("{}{}", self.marker(), self.text())
    }
}

/// A notice that stands in for a file's rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Notice {
    Binary,
    TooLarge,
    /// A side failed to read, through git or from disk; the next refresh tries again.
    Unreadable,
}

impl Notice {
    /// What the read pane says in the file's place.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Binary => "binary file · no line comments",
            Self::TooLarge => "file too large to show",
            Self::Unreadable => "couldn't read this file",
        }
    }
}

/// How the pane renders the model: the `Changes` diff, or the `All files` whole-file content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// Old-versus-new with change rows and folds.
    Diff,
    /// The whole current file as `Context` rows, no folds — the File view.
    File,
}

/// The selected file modeled as rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileDiff {
    pub path: String,
    /// The old path when this file was renamed, for the `old → new` header; `None` otherwise.
    pub previous_path: Option<String>,
    /// The notice shown in place of rows, `None` when the file renders.
    pub notice: Option<Notice>,
    pub view: View,
    pub rows: Vec<Row>,
    /// The exact landed comparison whose content this view loaded.
    pub identity: Option<FileIdentity>,
    /// The `(old, new)` line numbers of each line edited in place.
    pub pairs: Vec<(u32, u32)>,
    /// Base-anchored edit blocks, before context folding.
    edits: Vec<DiffEdit>,
}

/// The line budget; the byte budget below catches one huge line.
pub(crate) const MAX_LINES: usize = 50_000;
/// The byte budget. A file larger than this renders as a `too_large` notice.
pub(crate) const MAX_BYTES: usize = 2_000_000;

/// Whether a file of `len` bytes is over the size budget.
#[must_use]
pub fn over_byte_budget(len: usize) -> bool {
    len > MAX_BYTES
}

impl Default for FileDiff {
    fn default() -> Self {
        Self::empty()
    }
}

impl FileDiff {
    /// An empty placeholder, for when no file is selected.
    pub fn empty() -> Self {
        Self::rowless(String::new(), None, None, View::Diff)
    }

    /// A model with no rows: a notice, or the empty placeholder.
    fn rowless(
        path: String,
        previous_path: Option<String>,
        notice: Option<Notice>,
        view: View,
    ) -> Self {
        Self {
            path,
            previous_path,
            notice,
            view,
            rows: Vec::new(),
            identity: None,
            pairs: Vec::new(),
            edits: Vec::new(),
        }
    }

    /// Build the diff of `old` to `new`; `previous_path` is a rename or copy source.
    pub fn build(
        path: String,
        previous_path: Option<String>,
        old: &str,
        new: &str,
        hl: &Highlighter,
    ) -> Self {
        Self::build_inner(path, previous_path, old, new, None, hl)
    }

    /// Build from the same sides as [`Self::build`] and attach the identity reproduced from
    /// the worktree bytes and mode the caller read alongside `new`.
    pub(crate) fn build_identified(
        path: String,
        previous_path: Option<String>,
        old: &str,
        new: &str,
        identity: FileIdentity,
        hl: &Highlighter,
    ) -> Self {
        Self::build_inner(path, previous_path, old, new, Some(identity), hl)
    }

    fn build_inner(
        path: String,
        previous_path: Option<String>,
        old: &str,
        new: &str,
        identity: Option<FileIdentity>,
        hl: &Highlighter,
    ) -> Self {
        let language = language_of(&path);
        let notice = |n| {
            let mut diff = Self::rowless(path.clone(), previous_path.clone(), Some(n), View::Diff);
            diff.identity.clone_from(&identity);
            diff
        };
        if old.contains('\0') || new.contains('\0') {
            return notice(Notice::Binary);
        }
        if over_byte_budget(old.len() + new.len()) {
            return notice(Notice::TooLarge);
        }
        // One split feeds both the diff and the highlighter, so row `i` paints line `i`.
        let (old_lines, new_lines) = (lines(old), lines(new));
        if old_lines.len() + new_lines.len() > MAX_LINES {
            return notice(Notice::TooLarge);
        }

        let lang = language.as_deref();
        let old_spans = hl.highlight_lines(&old_lines, lang);
        let new_spans = hl.highlight_lines(&new_lines, lang);
        let line = |spans: &[Vec<Span>], i: usize| spans.get(i).cloned().unwrap_or_default();

        let mut rows = Vec::new();
        for change in TextDiff::from_slices(&old_lines, &new_lines).iter_all_changes() {
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
                        cr: false,
                    });
                }
                ChangeTag::Insert => {
                    let ni = change.new_index().unwrap();
                    rows.push(Row::Insertion {
                        new_no: ni as u32 + 1,
                        spans: line(&new_spans, ni),
                        emphasis: Vec::new(),
                        cr: false,
                    });
                }
            }
        }
        // Pair on text alone, then mark endings, so an ending-only change pairs with its twin.
        let paired = compute_emphasis(&mut rows);
        mark_crs(&mut rows, &old_lines, &new_lines, &paired);
        let edits = diff_edits(&rows, &new_lines);
        let mut pairs: Vec<(u32, u32)> =
            paired.iter().filter_map(|&(d, i)| rows[d].old_no().zip(rows[i].new_no())).collect();
        pairs.sort_unstable();
        Self {
            path,
            previous_path,
            notice: None,
            view: View::Diff,
            rows: collapse_context(&rows),
            identity,
            pairs,
            edits,
        }
    }

    /// Build the File view: all of `content` as `Context` rows, under the same budgets.
    fn build_file(path: String, content: &str, hl: &Highlighter) -> Self {
        let notice = |n| Self::rowless(path.clone(), None, Some(n), View::File);
        if content.contains('\0') {
            return notice(Notice::Binary);
        }
        if over_byte_budget(content.len()) {
            return notice(Notice::TooLarge);
        }
        let lines = lines(content);
        if lines.len() > MAX_LINES {
            return notice(Notice::TooLarge);
        }
        let rows = hl
            .highlight_lines(&lines, language_of(&path).as_deref())
            .into_iter()
            .enumerate()
            .map(|(i, spans)| {
                let no = i as u32 + 1;
                Row::Context { old_no: no, new_no: no, spans }
            })
            .collect();
        Self { rows, ..Self::rowless(path, None, None, View::File) }
    }

    /// A notice for a file the caller declines, or failed, to read.
    pub fn notice(path: String, previous_path: Option<String>, notice: Notice, view: View) -> Self {
        Self::rowless(path, previous_path, Some(notice), view)
    }

    pub(crate) fn notice_identified(
        path: String,
        previous_path: Option<String>,
        notice: Notice,
        identity: FileIdentity,
    ) -> Self {
        let mut diff = Self::notice(path, previous_path, notice, View::Diff);
        diff.identity = Some(identity);
        diff
    }

    /// The edit script captured by a whole-file review action.
    pub(crate) fn reviewed_edits(&self) -> Vec<ReviewedEdit> {
        self.edits.iter().map(|edit| edit.reviewed.clone()).collect()
    }

    /// Changed rows that remain unchanged within the same base-anchored edit.
    pub(crate) fn reviewed_lines(&self, reviewed: &[ReviewedEdit]) -> HashSet<DiffLine> {
        let reviewed: HashMap<_, _> =
            reviewed.iter().map(|edit| ((edit.old_start, edit.old_len), edit)).collect();
        let mut lines = HashSet::new();
        for edit in &self.edits {
            let key = (edit.reviewed.old_start, edit.reviewed.old_len);
            let Some(previous) = reviewed.get(&key) else { continue };
            lines.extend(edit.old_lines.clone().map(DiffLine::Old));
            if previous.new_fingerprints == edit.reviewed.new_fingerprints {
                lines.extend(edit.new_lines.clone().map(DiffLine::New));
                continue;
            }
            for op in capture_diff_slices(
                Algorithm::Myers,
                &previous.new_fingerprints,
                &edit.reviewed.new_fingerprints,
            ) {
                if op.tag() == DiffTag::Equal {
                    lines.extend(
                        op.new_range()
                            .map(|offset| DiffLine::New(edit.new_lines.start + offset as u32)),
                    );
                }
            }
        }
        lines
    }
}

/// Normalize the diff's change blocks against positions in the old side.
fn diff_edits(rows: &[Row], new_lines: &[&str]) -> Vec<DiffEdit> {
    change_blocks(rows)
        .into_iter()
        .map(|(deletions, insertions)| {
            let old_start = deletions.clone().next().and_then(|i| rows[i].old_no()).map_or_else(
                || rows[..insertions.start].iter().rev().find_map(Row::old_no).unwrap_or(0),
                |line| line - 1,
            );
            let new_start = insertions.clone().find_map(|i| rows[i].new_no()).unwrap_or(0);
            let new_fingerprints = insertions
                .clone()
                .filter_map(|i| rows[i].new_no())
                .map(|line| {
                    let mut fingerprint = DefaultHasher::new();
                    new_lines[line as usize - 1].hash(&mut fingerprint);
                    fingerprint.finish()
                })
                .collect();
            DiffEdit {
                reviewed: ReviewedEdit {
                    old_start,
                    old_len: deletions.len() as u32,
                    new_fingerprints,
                },
                old_lines: old_start + 1..old_start + 1 + deletions.len() as u32,
                new_lines: new_start..new_start + insertions.len() as u32,
            }
        })
        .collect()
}

pub(crate) fn set_row_spans(row: &mut Row, next: Vec<Span>) {
    match row {
        Row::Context { spans, .. } | Row::Deletion { spans, .. } | Row::Insertion { spans, .. } => {
            *spans = next;
        }
        Row::Rendered { .. } | Row::Fold { .. } => {}
    }
}

/// Mark the CR of each edited line whose ending changed: on its exact twin first, else its pair.
fn mark_crs(rows: &mut [Row], old: &[&str], new: &[&str], pairs: &[(usize, usize)]) {
    if !old.iter().chain(new).any(|line| line.contains('\r')) {
        return;
    }
    let ends_cr = |lines: &[&str], no: u32| line_body(lines[no as usize - 1]).1;
    for &(d, i) in pairs {
        let (Some(o), Some(n)) = (rows[d].old_no(), rows[i].new_no()) else { continue };
        let marked = match (ends_cr(old, o), ends_cr(new, n)) {
            (true, false) => d,
            (false, true) => i,
            _ => continue,
        };
        if let Row::Deletion { cr, .. } | Row::Insertion { cr, .. } = &mut rows[marked] {
            *cr = true;
        }
    }
}

/// Word emphasis on each change block's homolog pairs; returns the pairs' row indices.
pub(crate) fn compute_emphasis(rows: &mut [Row]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for (dels, inss) in change_blocks(rows) {
        pair_homologs(rows, dels, inss, &mut pairs);
    }
    pairs
}

/// Each change block's deletion and insertion index ranges, in order.
pub(crate) fn change_blocks<R: std::borrow::Borrow<Row>>(
    rows: &[R],
) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let is = |i: usize, deletion: bool| match rows.get(i).map(std::borrow::Borrow::borrow) {
        Some(Row::Deletion { .. }) => deletion,
        Some(Row::Insertion { .. }) => !deletion,
        _ => false,
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let del_start = i;
        while is(i, true) {
            i += 1;
        }
        let ins_start = i;
        while is(i, false) {
            i += 1;
        }
        if del_start == i {
            // No change block started here; step over the context/fold row.
            i += 1;
        } else {
            out.push((del_start..ins_start, ins_start..i));
        }
    }
    out
}

/// Pair each deletion with its homolog insertion, exact twins first, and set both lines' emphasis.
fn pair_homologs(
    rows: &mut [Row],
    dels: std::ops::Range<usize>,
    inss: std::ops::Range<usize>,
    pairs: &mut Vec<(usize, usize)>,
) {
    let old_texts: Vec<String> = dels.clone().map(|d| rows[d].text()).collect();
    let new_texts: Vec<String> = inss.clone().map(|p| rows[p].text()).collect();
    let mut claimed = vec![false; inss.len()];
    let mut twinned = vec![false; dels.len()];
    // Exact twins first, anywhere in the block: a line whose ending alone changed is that line.
    let mut twins: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (j, text) in new_texts.iter().enumerate() {
        twins.entry(text).or_default().push_back(j);
    }
    for (k, d) in dels.clone().enumerate() {
        if let Some(j) = twins.get_mut(old_texts[k].as_str()).and_then(VecDeque::pop_front) {
            claimed[j] = true;
            twinned[k] = true;
            pairs.push((d, inss.start + j));
        }
    }
    // Then each remaining line's first similar unclaimed successor within a window, in order.
    let mut next_ins = inss.start;
    for (k, d) in dels.enumerate() {
        if twinned[k] {
            continue;
        }
        let candidates = (next_ins..inss.end).filter(|&p| !claimed[p - inss.start]);
        for p in candidates.take(SIMILAR_WINDOW) {
            let (ratio, old_e, new_e) = word_emphasis(&old_texts[k], &new_texts[p - inss.start]);
            if ratio >= MIN_SIMILARITY {
                if let Row::Deletion { emphasis, .. } = &mut rows[d] {
                    *emphasis = old_e;
                }
                if let Row::Insertion { emphasis, .. } = &mut rows[p] {
                    *emphasis = new_e;
                }
                claimed[p - inss.start] = true;
                pairs.push((d, p));
                next_ins = p + 1;
                break;
            }
        }
    }
}

/// How many unclaimed insertions a deletion tries, so a rewrite block costs linear, not square.
const SIMILAR_WINDOW: usize = 64;

/// Below this, two lines are different lines, not one edited. Stricter than git-delta: marginal
/// pairs land near 0.6–0.65, real edits near 0.71–0.78.
const MIN_SIMILARITY: f32 = 0.7;

/// The word similarity of `old` and `new`, and the char ranges only each side has.
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

/// Trim whitespace off each range's edges, dropping all-whitespace ranges.
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

/// Merge ranges separated only by whitespace, so a changed phrase reads as one span.
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

/// Fold each long context run, keeping `FOLD_MARGIN` lines by every change and file end.
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

/// The extension that picks a syntax, e.g. `rs` for `src/app.rs`.
pub(crate) fn language_of(path: &str) -> Option<String> {
    Path::new(path).extension().and_then(|e| e.to_str()).map(str::to_string)
}

/// Built `FileDiff`s by content, so an unchanged refresh rebuilds nothing.
#[derive(Default, Debug)]
pub struct DiffCache {
    entries: HashMap<String, (u64, FileDiff)>,
}

/// The cache clears at this size; only the open file rebuilds.
const CACHE_CAP: usize = 256;

impl DiffCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The diff for `path`, keyed by both sides and `previous_path`, so a rename's header never leaks.
    pub fn get(
        &mut self,
        path: String,
        previous_path: Option<String>,
        old: &str,
        new: &str,
        hl: &Highlighter,
    ) -> FileDiff {
        let key = content_hash(previous_path.as_deref(), old, new);
        self.get_or_build(path.clone(), key, || FileDiff::build(path, previous_path, old, new, hl))
    }

    pub(crate) fn get_identified(
        &mut self,
        path: String,
        previous_path: Option<String>,
        old: &str,
        new: &str,
        identity: FileIdentity,
        hl: &Highlighter,
    ) -> FileDiff {
        // The identity already includes both exact content objects, paths, modes, and endpoints.
        // Avoid scanning both full strings again on every file navigation.
        let key = identity_hash(&identity);
        self.get_or_build(path.clone(), key, || {
            FileDiff::build_identified(path, previous_path, old, new, identity, hl)
        })
    }

    /// The File view for `path`, under a `file:` key so it never evicts the path's diff.
    pub fn get_file(&mut self, path: String, content: &str, hl: &Highlighter) -> FileDiff {
        let key = content_hash(None, content, content);
        self.get_or_build(format!("file:{path}"), key, || FileDiff::build_file(path, content, hl))
    }

    /// The entry under `cache_key` while its hash is `content_key`, else a fresh `build`.
    fn get_or_build(
        &mut self,
        cache_key: String,
        content_key: u64,
        build: impl FnOnce() -> FileDiff,
    ) -> FileDiff {
        if let Some((cached, diff)) = self.entries.get(&cache_key)
            && *cached == content_key
        {
            return diff.clone();
        }
        let diff = build();
        if self.entries.len() >= CACHE_CAP {
            self.entries.clear();
        }
        self.entries.insert(cache_key, (content_key, diff.clone()));
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

fn identity_hash(identity: &FileIdentity) -> u64 {
    let mut h = DefaultHasher::new();
    identity.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::{DiffCache, DiffLine, FileDiff, Notice, Row, SIMILAR_WINDOW, View, language_of};
    use crate::highlight::Highlighter;
    use crate::theme;

    /// The default theme's syntax pairing (bundled Catppuccin Mocha), for highlighter setup.
    fn mocha() -> crate::theme::SyntaxChoice {
        theme::resolve(Some("catppuccin")).syntax
    }

    fn live_identity(content: &[u8]) -> crate::model::FileIdentity {
        use crate::model::{ChangeKind, FileIdentity, FileIdentityInput};
        let fingerprint = String::from_utf8_lossy(content);
        FileIdentity::from_git(FileIdentityInput {
            old_endpoint: "old-tree",
            new_endpoint: "worktree",
            kind: ChangeKind::Modified,
            path: "a.rs",
            previous_path: None,
            old_mode: "100644",
            new_mode: "100644",
            old_content: "old-blob",
            new_content: &fingerprint,
            binary: false,
            live_new_side: true,
        })
    }

    #[test]
    fn file_view_is_all_context_with_no_folds() {
        use std::fmt::Write as _;
        let hl = Highlighter::new(mocha());
        let mut content = String::new();
        for i in 0..40 {
            writeln!(content, "line {i}").unwrap();
        }
        let d = FileDiff::build_file("a.rs".into(), &content, &hl);
        assert_eq!(d.view, View::File);
        assert_eq!(d.notice, None);
        assert_eq!(d.rows.len(), 40);
        assert!(d.rows.iter().all(|r| matches!(r, Row::Context { .. })), "every row is context");
        // Even a long unchanged run never folds in the File view.
        assert!(!d.rows.iter().any(|r| matches!(r, Row::Fold { .. })));
        assert_eq!(d.rows[0].new_no(), Some(1));
        assert_eq!(d.rows[39].new_no(), Some(40));
    }

    #[test]
    fn file_view_degrades_on_binary() {
        let hl = Highlighter::new(mocha());
        let d = FileDiff::build_file("blob.bin".into(), "a\0b", &hl);
        assert_eq!(d.notice, Some(Notice::Binary));
        assert_eq!(d.view, View::File);
        assert!(d.rows.is_empty());
    }

    fn build(old: &str, new: &str) -> FileDiff {
        let hl = Highlighter::new(mocha());
        FileDiff::build("a.rs".into(), None, old, new, &hl)
    }

    #[test]
    fn reviewed_edits_match_unchanged_lines_at_the_same_base_anchor() {
        let base = "base\nmiddle\ntail\n";
        let reviewed = build(base, "base\nseen\nmiddle\ntail\n").reviewed_edits();
        let current = build(base, "base\nseen\nmiddle\nlater\ntail\n");
        let unchanged = current.reviewed_lines(&reviewed);
        assert!(unchanged.contains(&DiffLine::New(2)), "the exact reviewed insertion matches");
        assert!(!unchanged.contains(&DiffLine::New(4)), "a later insertion does not match");

        let rewritten = build(base, "base\nrewritten\nmiddle\ntail\n");
        assert_eq!(
            rewritten.reviewed_lines(&reviewed),
            std::collections::HashSet::new(),
            "a changed payload invalidates the complete reviewed block",
        );

        let moved = build(base, "base\nmiddle\nseen\ntail\n");
        assert!(
            moved.reviewed_lines(&reviewed).is_empty(),
            "the same payload at another base anchor is new work",
        );

        let top_reviewed = build(base, "seen\nbase\nmiddle\ntail\n").reviewed_edits();
        let top_current = build(base, "seen\nbase\nmiddle\nlater\ntail\n");
        assert!(
            top_current.reviewed_lines(&top_reviewed).contains(&DiffLine::New(1)),
            "a top-of-file insertion keeps its base anchor",
        );

        let deletion_base = "head\ngone\nmiddle\ntail\n";
        let deletion = build(deletion_base, "head\nmiddle\ntail\n").reviewed_edits();
        let after_deletion = build(deletion_base, "head\nmiddle\nlater\ntail\n");
        assert!(
            after_deletion.reviewed_lines(&deletion).contains(&DiffLine::Old(2)),
            "a retained deletion matches its old-side line",
        );

        let replacement_base = "head\nold\nmiddle\ntail\n";
        let replacement = build(replacement_base, "head\nnew\nmiddle\ntail\n").reviewed_edits();
        let after_replacement = build(replacement_base, "head\nnew\nmiddle\nlater\ntail\n");
        let replacement_lines = after_replacement.reviewed_lines(&replacement);
        assert!(replacement_lines.contains(&DiffLine::Old(2)));
        assert!(replacement_lines.contains(&DiffLine::New(2)));

        let adjacent_reviewed = build(base, "base\nseen\nmiddle\ntail\n").reviewed_edits();
        let adjacent_current = build(base, "base\nseen\nlater\nmiddle\ntail\n");
        let adjacent_lines = adjacent_current.reviewed_lines(&adjacent_reviewed);
        assert!(
            adjacent_lines.contains(&DiffLine::New(2)),
            "a reviewed line remains reviewed when the insertion block grows",
        );
        assert!(
            !adjacent_lines.contains(&DiffLine::New(3)),
            "the adjacent line added later remains new",
        );

        let whole_file_reviewed = build("", "one\ntwo\nthree\n").reviewed_edits();
        let whole_file_current = build("", "one\ntwo\nnew\nthree\n");
        let whole_file_lines = whole_file_current.reviewed_lines(&whole_file_reviewed);
        assert!(whole_file_lines.contains(&DiffLine::New(1)));
        assert!(whole_file_lines.contains(&DiffLine::New(2)));
        assert!(!whole_file_lines.contains(&DiffLine::New(3)));
        assert!(whole_file_lines.contains(&DiffLine::New(4)));
    }

    #[test]
    fn cache_keys_on_previous_path_so_a_rename_and_a_plain_edit_differ() {
        let hl = Highlighter::new(mocha());
        let mut cache = DiffCache::new();
        // Same path and content, one a rename: the plain edit must not get the rename's build.
        let renamed = cache.get("f.rs".into(), Some("old.rs".into()), "x\n", "y\n", &hl);
        let plain = cache.get("f.rs".into(), None, "x\n", "y\n", &hl);
        assert_eq!(renamed.previous_path.as_deref(), Some("old.rs"));
        assert_eq!(plain.previous_path, None);
    }

    #[test]
    fn rows_carry_sides_numbers_and_markers() {
        let d = build("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(d.notice, None);
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

    /// The changed rows, each with whether it marks a CR.
    fn changes(d: &FileDiff) -> Vec<(String, bool)> {
        d.rows
            .iter()
            .filter(|r| r.marker() != ' ')
            .map(|r| (r.marker_text(), r.cr_marker()))
            .collect()
    }

    #[test]
    fn a_changed_line_ending_shows_its_cr_and_a_shared_one_does_not() {
        // Only the ending changed: the gained CR is the one difference, marked, never text.
        let d = build("alpha\nbeta\n", "alpha\r\nbeta\n");
        assert_eq!(changes(&d), [("-alpha".into(), false), ("+alpha".into(), true)]);
        assert_eq!(d.pairs, [(1, 1)], "the line pairs with its twin");
        // CRLF throughout: an edit shows its text, and no ending changed.
        let d = build("alpha\r\nbeta\r\n", "alpha\r\nBETA\r\n");
        assert_eq!(changes(&d), [("-beta".into(), false), ("+BETA".into(), false)]);
        // One block, two edited lines: only the line whose own ending changed is marked.
        let d = build("alpha\r\nbeta\n", "alpha!\r\nbeta\r\n");
        assert_eq!(
            changes(&d),
            [
                ("-alpha".into(), false),
                ("-beta".into(), false),
                ("+alpha!".into(), false),
                ("+beta".into(), true),
            ]
        );
        // Pairs that each keep their ending mark nothing.
        let d = build("x\r\ny\n", "X\r\ny2\n");
        assert!(changes(&d).iter().all(|&(_, cr)| !cr), "{:?}", changes(&d));
        // An unpaired line has no old ending to compare: an added CRLF line is unmarked.
        let d = build("alpha\n", "alpha\nzzz\r\n");
        assert_eq!(changes(&d), [("+zzz".into(), false)]);
        // A similar line inserted ahead cannot take the edited line's twin.
        let d = build("a = 1;\n", "a = 2;\r\na = 1;\r\n");
        assert_eq!(
            changes(&d),
            [("-a = 1;".into(), false), ("+a = 2;".into(), false), ("+a = 1;".into(), true)]
        );
        // The emphasis pairs the same twin the marker does.
        assert_eq!(d.pairs, [(1, 2)]);
    }

    #[test]
    fn swapped_lines_keep_their_ending_marks() {
        let d = build("alpha one\r\nbeta two\r\n", "beta two\nalpha one\n");
        assert_eq!(
            changes(&d),
            [
                ("-alpha one".into(), true),
                ("-beta two".into(), true),
                ("+beta two".into(), false),
                ("+alpha one".into(), false),
            ]
        );
    }

    #[test]
    fn a_bare_cr_inside_a_line_breaks_no_line() {
        // git splits lines on `\n` alone: the CR is text, and line 2 is `let d`.
        let d = build("let c\r= 3;\nlet d = 4;\n", "let c\r= 3;\nlet d = 5;\n");
        let rows: Vec<(Option<u32>, Option<u32>, String)> =
            d.rows.iter().map(|r| (r.old_no(), r.new_no(), r.marker_text())).collect();
        assert_eq!(
            rows,
            [
                (Some(1), Some(1), " let c\r= 3;".to_string()),
                (Some(2), None, "-let d = 4;".to_string()),
                (None, Some(2), "+let d = 5;".to_string()),
            ]
        );
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
        // The long unchanged head and tail each fold.
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
        // Only `foo`→`bar` and `, b` are emphasized.
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
        // Two changed words split by a space emphasize as one block.
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
        // A line inserted above an edit: the edit pairs with its homolog, not the new line.
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
        // An added trailing comment highlights `// note`, not the space before it.
        let d = build("    let x = 1;\n", "    let x = 1; // note\n");
        let ins = d.rows.iter().find(|r| matches!(r, Row::Insertion { .. })).unwrap();
        assert_eq!(ins.emphasis().len(), 1);
        let (a, b) = ins.emphasis()[0];
        let seg: String = ins.text().chars().skip(a as usize).take((b - a) as usize).collect();
        assert_eq!(seg, "// note", "emphasis hugs the comment, no leading space");
    }

    #[test]
    fn a_reformat_or_unrelated_pair_is_not_emphasized() {
        // A reformat and two `let`s sharing a skeleton stay below the bar, unemphasized.
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
        // Two unrelated lines sharing only `///` and punctuation stay unemphasized.
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
    fn a_similar_line_past_the_window_stays_unpaired() {
        use std::fmt::Write as _;
        let pairs_with = |unlike: usize| {
            let mut new = String::new();
            for i in 0..unlike {
                writeln!(new, "zz{i} qq{i} ww{i}").unwrap();
            }
            new.push_str("let total = sum + 2;\n");
            build("let total = sum + 1;\n", &new).pairs
        };
        let at = |n: usize| u32::try_from(n + 1).unwrap();
        assert_eq!(pairs_with(SIMILAR_WINDOW - 1), [(1, at(SIMILAR_WINDOW - 1))]);
        assert!(pairs_with(SIMILAR_WINDOW).is_empty());
    }

    #[test]
    fn binary_content_is_flagged_not_rowed() {
        let d = build("ok\n", "bin\0ary\n");
        assert_eq!(d.notice, Some(Notice::Binary));
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
        let hl = Highlighter::new(mocha());
        let mut cache = DiffCache::new();
        let d1 = cache.get("a.rs".into(), None, "x\n", "y\n", &hl);
        let d2 = cache.get("a.rs".into(), None, "x\n", "y\n", &hl);
        assert_eq!(d1, d2);
    }

    #[test]
    fn loaded_diff_reproduces_the_side_it_actually_read() {
        let hl = Highlighter::new(mocha());
        let landed = live_identity(b"one\n");
        let unchanged = landed.with_loaded_worktree_fingerprint("100644", "one\n");
        let moved = landed.with_loaded_worktree_fingerprint("100644", "two\n");

        let diff =
            FileDiff::build_identified("a.rs".into(), None, "old\n", "two\n", moved.clone(), &hl);
        assert_eq!(unchanged, landed);
        assert_ne!(moved, landed, "an edit after the world build cannot match the landed row");
        assert_eq!(diff.identity.as_ref(), Some(&moved));
    }
}
