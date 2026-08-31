//! Mouse text selection: the drag state and the copied text.
//!
//! The gesture lives here as pure data plus the extraction
//! that turns spanned rows into clipboard text; hit-testing and painting live in `ui.rs`,
//! routing in `lib.rs`.

use crate::diff::Row;
use crate::file_list::{self, RowKind};

/// Where a text drag lives, locked at mouse-down (`TS-ONE-SURFACE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// The read pane's `FileDiff` rows (Diff or File view): character-precise source text.
    Read,
    /// The read pane's painted lines (markdown preview, `PR` read pane): character-precise
    /// painted text.
    Painted,
    /// A spliced comment card in the read pane: character-precise card text, confined to the
    /// card it started on (`TS-ONE-SURFACE`).
    Card { comment: usize },
    /// The file navigator: row-granular, a row copies its repo-relative path.
    Files,
    /// The `PR` tab's checks-and-comments navigator: row-granular displayed text.
    PrNav,
}

/// One endpoint: a logical row in its surface plus a character offset into that row's text.
/// Row-granular surfaces keep `chr` at 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub row: usize,
    pub chr: usize,
}

/// An active text drag: born at mouse-down, ended on release or an interrupting event.
#[derive(Clone, Copy, Debug)]
pub struct TextDrag {
    pub surface: Surface,
    pub anchor: Point,
    pub extent: Point,
}

/// The one live mouse gesture. A text gesture carries its drag; a release whose point never
/// left the anchor's resolves into the click or the double's action, so a head cannot exist
/// without its origin.
#[derive(Clone, Copy, Debug, Default)]
pub enum Gesture {
    /// No live gesture.
    #[default]
    None,
    /// A text gesture over a selectable surface.
    Text {
        drag: TextDrag,
        /// The mouse-down's multi-click count: 1 = single, 2 = double, 3 = triple, and
        /// further clicks within the window stay 3.
        count: u8,
    },
    /// A gutter comment gesture: line selection by drag, the composer on release
    Gutter,
}

impl TextDrag {
    /// The selection's endpoints in document order. Both ends are inclusive: the character
    /// under the pointer is part of the selection.
    #[must_use]
    pub fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.extent {
            (self.anchor, self.extent)
        } else {
            (self.extent, self.anchor)
        }
    }
}

/// The clipboard text for a `Read`-surface selection over `rows`: each spanned content row
/// contributes its source line once (a wrapped line is one row), the first and last rows cut
/// at the endpoints, folds contribute nothing.
#[must_use]
pub fn read_text(rows: &[Row], a: Point, b: Point) -> String {
    let hi_row = b.row.min(rows.len().saturating_sub(1));
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate().take(hi_row + 1).skip(a.row) {
        if !row.is_content() {
            continue;
        }
        let text = row.text();
        let from = if i == a.row { a.chr } else { 0 };
        let to = if i == hi_row { Some(b.chr) } else { None };
        out.push(slice_chars(&text, from, to));
    }
    out.join("\n")
}

/// The clipboard text for a selection over prebuilt line texts (painted surfaces and cards):
/// whole lines between the endpoints, the first and last cut at them.
#[must_use]
pub fn lines_text(lines: &[String], a: Point, b: Point) -> String {
    let hi_row = b.row.min(lines.len().saturating_sub(1));
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate().take(hi_row + 1).skip(a.row) {
        let from = if i == a.row { a.chr } else { 0 };
        let to = if i == hi_row { Some(b.chr) } else { None };
        out.push(slice_chars(line, from, to));
    }
    out.join("\n")
}

/// The clipboard text for a `Files`-surface selection: each spanned row contributes its
/// full repo-relative path, directories included, as the tree nests it (a directory row its
/// directory path), one per line, without the tree glyphs and annotations
#[must_use]
pub fn files_text(
    rows: &[file_list::Row],
    entries: &[file_list::Entry],
    a_row: usize,
    b_row: usize,
) -> String {
    rows.iter()
        .take(b_row.min(rows.len().saturating_sub(1)) + 1)
        .skip(a_row)
        .filter_map(|r| match &r.kind {
            RowKind::Dir { path, .. } => Some(path.clone()),
            RowKind::File { index, .. } => entries.get(*index).map(|e| e.path.clone()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The chars of `text` from `from` up to and including `to` (to the end when `to` is `None`),
/// clamped to the text.
fn slice_chars(text: &str, from: usize, to: Option<usize>) -> String {
    let iter = text.chars().skip(from);
    match to {
        Some(t) => iter.take(t.saturating_add(1).saturating_sub(from)).collect(),
        None => iter.collect(),
    }
}

/// The word at char `chr` of `text`: the inclusive char range of the unbroken run of
/// letters, digits, and underscores covering it. Whitespace, punctuation, and offsets past
/// the text yield `None`, so the double falls back to the click.
#[must_use]
pub fn token_at(text: &str, chr: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    if chr >= chars.len() || !is_word(chars[chr]) {
        return None;
    }
    let mut s = chr;
    while s > 0 && is_word(chars[s - 1]) {
        s -= 1;
    }
    let mut e = chr;
    while e + 1 < chars.len() && is_word(chars[e + 1]) {
        e += 1;
    }
    Some((s, e))
}

/// The footer status after a copy of `text`.
#[must_use]
pub fn copied_status(text: &str) -> String {
    let n = text.chars().count();
    let noun = if n == 1 { "char" } else { "chars" };
    format!("copied {n} {noun}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Span;

    fn spans(text: &str) -> Vec<Span> {
        vec![Span { text: text.into(), color: ratatui::style::Color::Rgb(0, 0, 0) }]
    }

    fn ctx(text: &str) -> Row {
        Row::Context { old_no: 1, new_no: 1, spans: spans(text) }
    }

    fn del(text: &str) -> Row {
        Row::Deletion { old_no: 1, spans: spans(text), emphasis: vec![] }
    }

    fn ins(text: &str) -> Row {
        Row::Insertion { new_no: 1, spans: spans(text), emphasis: vec![] }
    }

    fn fold(hidden: usize) -> Row {
        Row::Fold { lines: (0..hidden).map(|_| ctx("hidden")).collect() }
    }

    fn point(row: usize, chr: usize) -> Point {
        Point { row, chr }
    }

    #[test]
    fn endpoints_order_by_row_then_char_and_include_both_ends() {
        let drag = TextDrag { surface: Surface::Read, anchor: point(3, 7), extent: point(1, 2) };
        assert_eq!(drag.ordered(), (point(1, 2), point(3, 7)));
        let same_row =
            TextDrag { surface: Surface::Read, anchor: point(2, 9), extent: point(2, 4) };
        assert_eq!(same_row.ordered(), (point(2, 4), point(2, 9)));
    }

    #[test]
    fn read_text_slices_the_end_rows_and_takes_middles_whole() {
        let rows = vec![ctx("let alpha = 1;"), ctx("let beta = 2;"), ctx("let gamma = 3;")];
        let got = read_text(&rows, point(0, 4), point(2, 8));
        assert_eq!(got, "alpha = 1;\nlet beta = 2;\nlet gamma");
    }

    #[test]
    fn read_text_on_one_row_takes_the_inclusive_span() {
        let rows = vec![ctx("let alpha = 1;")];
        assert_eq!(read_text(&rows, point(0, 4), point(0, 8)), "alpha");
    }

    #[test]
    fn read_text_skips_folds_and_takes_change_rows() {
        let rows = vec![ctx("a"), fold(2), del("removed"), ins("added")];
        assert_eq!(read_text(&rows, point(0, 0), point(3, 4)), "a\nremoved\nadded");
    }

    #[test]
    fn read_text_clamps_past_the_end() {
        let rows = vec![ctx("ab")];
        assert_eq!(read_text(&rows, point(0, 0), point(9, 99)), "ab");
        assert_eq!(read_text(&rows, point(0, 1), point(0, 99)), "b");
    }

    #[test]
    fn copied_status_counts_chars_and_pluralizes() {
        assert_eq!(copied_status("x"), "copied 1 char");
        assert_eq!(copied_status("a\nb\nc"), "copied 5 chars");
        // Characters, not bytes or display columns: a wide glyph counts once.
        assert_eq!(copied_status("日本"), "copied 2 chars");
    }

    #[test]
    fn token_at_expands_words_and_rejects_everything_else() {
        let line = "let foo_bar2 = 日本(x);";
        assert_eq!(token_at(line, 0), Some((0, 2)), "start of `let`");
        assert_eq!(token_at(line, 6), Some((4, 11)), "underscores and digits join `foo_bar2`");
        assert_eq!(token_at(line, 3), None, "whitespace acts as the click");
        assert_eq!(token_at(line, 13), None, "punctuation acts as the click");
        assert_eq!(token_at(line, 15), Some((15, 16)), "non-ASCII letters are word chars");
        assert_eq!(token_at(line, 99), None, "past the text acts as the click");
        assert_eq!(token_at("", 0), None, "an empty line has no word");
    }

    #[test]
    fn files_text_yields_repo_relative_paths_for_dir_and_file_rows() {
        use crate::file_list::{Entry, Row as FileRow, RowKind};
        let entries = vec![Entry {
            path: "sub/two.rs".into(),
            previous_path: None,
            annotation: None,
            ignored: false,
            is_dir: false,
        }];
        let rows = vec![
            FileRow {
                depth: 0,
                name: "sub".into(),
                kind: RowKind::Dir { path: "sub".into(), expanded: true },
                ignored: false,
            },
            FileRow {
                depth: 1,
                name: "two.rs".into(),
                kind: RowKind::File { index: 0, annotation: None },
                ignored: false,
            },
        ];
        // A directory row contributes its own path; a file row its entry's full
        // repo-relative path, never the displayed basename.
        assert_eq!(files_text(&rows, &entries, 0, 1), "sub\nsub/two.rs");
        assert_eq!(files_text(&rows, &entries, 1, 1), "sub/two.rs");
    }
}
