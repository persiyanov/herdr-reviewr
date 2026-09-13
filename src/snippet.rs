//! A stored PR finding hunk as Diff-view rows.
//!
//! This is a quotation, not a [`crate::diff::FileDiff`]. The live viewer owns folds,
//! cursor, and comment targets. This module parses a unified-diff snippet, windows it
//! to the finding's side and range, and highlights the kept rows.

use crate::diff::{Row, Span, compute_emphasis, language_of, set_row_spans};
use crate::highlight::Highlighter;
use crate::model::Side;

/// Lines of stored hunk to keep above and below the comment range.
pub(crate) const SNIPPET_CONTEXT: u32 = 3;

/// Content rows from a stored PR finding hunk.
pub fn rows_from_snippet(
    hunk: &str,
    path: &str,
    start: u32,
    end: u32,
    side: Side,
    hl: &Highlighter,
) -> Vec<Row> {
    let (start, end) = ordered(start, end);
    let (mut rows, numbered) = parse_hunk(hunk);
    if numbered {
        if !rows.iter().any(|row| snippet_row_is_comment(row, start, end, side)) {
            return Vec::new();
        }
        let pad_start = start.saturating_sub(SNIPPET_CONTEXT);
        let pad_end = end.saturating_add(SNIPPET_CONTEXT);
        let mut keep = vec![false; rows.len()];
        for (i, row) in rows.iter().enumerate() {
            if snippet_row_is_comment(row, pad_start, pad_end, side) {
                keep[i] = true;
            }
        }
        for block in change_blocks(&rows) {
            if block.clone().any(|i| snippet_row_is_comment(&rows[i], start, end, side)) {
                for i in block {
                    if opposite_change(&rows[i], side) {
                        keep[i] = true;
                    }
                }
            }
        }
        let mut i = 0;
        rows.retain(|_| {
            let on = keep[i];
            i += 1;
            on
        });
        // `render_row` paints `new_no` first; on an old-side range copy `old_no` so the
        // gutter matches `path:start-end`.
        if side == Side::Old {
            for row in &mut rows {
                if let Row::Context { old_no, new_no, .. } = row {
                    *new_no = *old_no;
                }
            }
        }
    }

    let texts: Vec<String> = rows.iter().map(Row::text).collect();
    let highlighted = hl.highlight(&texts.join("\n"), language_of(path).as_deref());
    for (row, spans) in rows.iter_mut().zip(highlighted) {
        set_row_spans(row, spans);
    }
    compute_emphasis(&mut rows);
    rows
}

/// One-slot memo for [`rows_from_snippet`]. The PR pane shows one finding at a time,
/// so one key absorbs the per-frame highlight (`policies/ux-responsiveness.md`).
/// Cleared on a theme switch — the key does not include the highlighter.
#[derive(Debug, Default)]
pub struct SnippetRowCache {
    key: Option<(String, String, u32, u32, Side)>,
    rows: Vec<Row>,
}

impl SnippetRowCache {
    pub fn get(
        &mut self,
        hunk: &str,
        path: &str,
        start: u32,
        end: u32,
        side: Side,
        hl: &Highlighter,
    ) -> Vec<Row> {
        if self.key.as_ref().is_some_and(|(h, p, s, e, sd)| {
            h == hunk && p == path && *s == start && *e == end && *sd == side
        }) {
            return self.rows.clone();
        }
        self.rows = rows_from_snippet(hunk, path, start, end, side, hl);
        self.key = Some((hunk.to_string(), path.to_string(), start, end, side));
        self.rows.clone()
    }

    pub fn clear(&mut self) {
        self.key = None;
        self.rows.clear();
    }
}

/// `+` when the resolved side is new and the range has insertions, `-` when the
/// side is old, otherwise no sign.
pub(crate) fn snippet_caption_sign(rows: &[Row], start: u32, end: u32, side: Side) -> Option<char> {
    match side {
        Side::Old => Some('-'),
        Side::New => {
            let (start, end) = ordered(start, end);
            rows.iter()
                .any(|row| matches!(row, Row::Insertion { new_no, .. } if in_span(*new_no, start, end)))
                .then_some('+')
        }
    }
}

/// Whether this row is the comment subject (orange).
pub(crate) fn snippet_row_is_comment(row: &Row, start: u32, end: u32, side: Side) -> bool {
    let (start, end) = ordered(start, end);
    match row {
        Row::Insertion { new_no, .. } => side == Side::New && in_span(*new_no, start, end),
        Row::Deletion { old_no, .. } => side == Side::Old && in_span(*old_no, start, end),
        Row::Context { old_no, new_no, .. } => match side {
            Side::New => in_span(*new_no, start, end),
            Side::Old => in_span(*old_no, start, end),
        },
        Row::Fold { .. } => false,
    }
}

fn opposite_change(row: &Row, side: Side) -> bool {
    matches!((row, side), (Row::Deletion { .. }, Side::New) | (Row::Insertion { .. }, Side::Old))
}

fn parse_hunk(hunk: &str) -> (Vec<Row>, bool) {
    let mut rows = Vec::new();
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let mut numbered = false;
    for raw in hunk.lines() {
        if raw.starts_with("@@ ") {
            if let Some((o, n)) = parse_hunk_header(raw) {
                old_no = o;
                new_no = n;
                numbered = true;
            }
            continue;
        }
        if raw.starts_with('\\') {
            continue;
        }
        let marker = raw.chars().next().unwrap_or(' ');
        let (marker, text, this_old, this_new) = match marker {
            '+' => {
                let n = new_no;
                if numbered {
                    new_no = new_no.saturating_add(1);
                }
                ('+', raw.get(1..).unwrap_or(""), 0, n)
            }
            '-' => {
                let o = old_no;
                if numbered {
                    old_no = old_no.saturating_add(1);
                }
                ('-', raw.get(1..).unwrap_or(""), o, 0)
            }
            ' ' => {
                let (o, n) = (old_no, new_no);
                if numbered {
                    old_no = old_no.saturating_add(1);
                    new_no = new_no.saturating_add(1);
                }
                (' ', raw.get(1..).unwrap_or(""), o, n)
            }
            _ => (' ', raw, 0, 0),
        };
        let spans =
            vec![Span { text: text.to_string(), color: ratatui::style::Color::Rgb(0, 0, 0) }];
        rows.push(match marker {
            '+' => Row::Insertion { new_no: this_new, spans, emphasis: Vec::new() },
            '-' => Row::Deletion { old_no: this_old, spans, emphasis: Vec::new() },
            _ => Row::Context { old_no: this_old, new_no: this_new, spans },
        });
    }
    (rows, numbered)
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@ ")?;
    let mut parts = rest.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    if !old.starts_with('-') || !new.starts_with('+') {
        return None;
    }
    let old_no = old[1..].split(',').next()?.parse().ok()?;
    let new_no = new[1..].split(',').next()?.parse().ok()?;
    Some((old_no, new_no))
}

fn change_blocks(rows: &[Row]) -> Vec<std::ops::Range<usize>> {
    let mut i = 0;
    let mut out = Vec::new();
    while i < rows.len() {
        if matches!(rows[i], Row::Deletion { .. }) {
            let start = i;
            while i < rows.len() && matches!(rows[i], Row::Deletion { .. }) {
                i += 1;
            }
            let ins = i;
            while i < rows.len() && matches!(rows[i], Row::Insertion { .. }) {
                i += 1;
            }
            if i > ins {
                out.push(start..i);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn ordered(start: u32, end: u32) -> (u32, u32) {
    if start <= end { (start, end) } else { (end, start) }
}

fn in_span(n: u32, start: u32, end: u32) -> bool {
    n >= start && n <= end && n > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mocha() -> crate::theme::SyntaxChoice {
        crate::theme::resolve(Some("catppuccin")).syntax
    }

    fn snippet(hunk: &str, start: u32, end: u32) -> Vec<Row> {
        snippet_side(hunk, start, end, Side::New)
    }

    fn snippet_side(hunk: &str, start: u32, end: u32, side: Side) -> Vec<Row> {
        let hl = Highlighter::new(mocha());
        rows_from_snippet(hunk, "a.rs", start, end, side, &hl)
    }

    #[test]
    fn rows_from_snippet_keeps_the_range_and_drops_headers() {
        let hunk = concat!(
            "@@ -105,11 +105,11 @@\n",
            " far_above\n",
            " a\n",
            " b\n",
            " c\n",
            " mid\n",
            " ctx\n",
            "-    let x = foo(a);\n",
            "+    let x = bar(a);\n",
            " tail\n",
            " d\n",
            " e\n",
            " far_below\n",
        );
        let rows = snippet(hunk, 111, 111);
        let has = |s: &str| rows.iter().any(|r| r.text() == s);
        assert!(has("c") && has("ctx") && has("tail"));
        assert!(has("    let x = foo(a);") && has("    let x = bar(a);"));
        assert!(rows.iter().all(|r| !r.text().starts_with("@@")));
        assert!(!has("far_above") && !has("a") && !has("b"));
        assert!(!has("far_below"));
        let del = rows.iter().find(|r| matches!(r, Row::Deletion { .. })).unwrap();
        let ins = rows.iter().find(|r| matches!(r, Row::Insertion { .. })).unwrap();
        assert_eq!(del.old_no(), Some(111));
        assert_eq!(ins.new_no(), Some(111));
        assert!(!del.emphasis().is_empty() && !ins.emphasis().is_empty());
        assert!(!snippet_row_is_comment(del, 111, 111, Side::New));
        assert!(snippet_row_is_comment(ins, 111, 111, Side::New));
        assert!(!snippet_row_is_comment(
            rows.iter().find(|r| r.text() == "ctx").unwrap(),
            111,
            111,
            Side::New,
        ));
        assert_eq!(snippet_caption_sign(&rows, 111, 111, Side::New), Some('+'));
        let wider = snippet(hunk, 110, 111);
        let wider_has = |s: &str| wider.iter().any(|r| r.text() == s);
        assert!(wider_has("b") && wider_has("ctx"));
        assert!(!wider_has("far_above") && !wider_has("a"));
    }

    #[test]
    fn rows_from_snippet_paints_old_side_numbers_on_kept_context() {
        let hunk = concat!(
            "@@ -20,5 +20,6 @@\n",
            " keep\n",
            "+    added\n",
            " keep\n",
            " ctx\n",
            "-    gone\n",
        );
        let rows = snippet_side(hunk, 22, 23, Side::Old);
        let ctx = rows.iter().find(|r| r.text() == "ctx").unwrap();
        assert_eq!(ctx.old_no(), Some(22));
        assert_eq!(ctx.new_no(), Some(22));
        let del = rows.iter().find(|r| matches!(r, Row::Deletion { .. })).unwrap();
        assert_eq!(del.old_no(), Some(23));
        assert_eq!(del.text(), "    gone");
        let new_side = snippet(hunk, 22, 22);
        let keep =
            new_side.iter().find(|r| r.old_no() == Some(21) && r.new_no() == Some(22)).unwrap();
        assert_eq!(keep.text(), "keep");
        assert_eq!(snippet_caption_sign(&rows, 22, 23, Side::Old), Some('-'));
        assert!(snippet_row_is_comment(del, 22, 23, Side::Old));
        assert_eq!(snippet_caption_sign(&new_side, 22, 22, Side::New), None);
    }

    #[test]
    fn rows_from_snippet_old_side_does_not_keep_an_unrelated_insert() {
        let hunk =
            concat!("@@ -20,4 +20,5 @@\n", " keep\n", "+    added\n", " keep\n", "-    gone\n",);
        let rows = snippet_side(hunk, 22, 22, Side::Old);
        let has = |s: &str| rows.iter().any(|r| r.text() == s);
        assert!(has("    gone"));
        assert!(!has("    added"));
        assert_eq!(snippet_caption_sign(&rows, 22, 22, Side::Old), Some('-'));
    }

    #[test]
    fn rows_from_snippet_keeps_the_pair_when_only_one_number_hits() {
        let hunk = "@@ -10,3 +10,3 @@\n keep\n-    old\n+    new\n";
        let rows = snippet_side(hunk, 11, 11, Side::New);
        let has = |s: &str| rows.iter().any(|r| r.text() == s);
        assert!(has("    new") && has("    old"));
    }

    #[test]
    fn rows_from_snippet_without_a_header_has_no_line_numbers() {
        let rows = snippet("-    old\n+    new\n", 1, 1);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].old_no(), Some(0));
        assert_eq!(rows[1].new_no(), Some(0));
        assert!(matches!(rows[0], Row::Deletion { .. }));
        assert!(matches!(rows[1], Row::Insertion { .. }));
    }

    #[test]
    fn rows_from_snippet_does_not_keep_following_context_on_an_insert() {
        let hunk = concat!(
            "@@ -20,8 +20,9 @@\n",
            " keep\n",
            " keep\n",
            " keep\n",
            "+    added\n",
            " after\n",
            " near\n",
            " near2\n",
            " far\n",
            " farther\n",
        );
        let rows = snippet(hunk, 23, 23);
        let has = |s: &str| rows.iter().any(|r| r.text() == s);
        assert!(has("    added") && has("after") && has("near2"));
        assert!(!has("far") && !has("farther"));
        assert_eq!(snippet_caption_sign(&rows, 23, 23, Side::New), Some('+'));
        let deleted = concat!(
            "@@ -20,9 +20,8 @@\n",
            " keep\n",
            " keep\n",
            " keep\n",
            "-    gone\n",
            " after\n",
            " near\n",
            " near2\n",
            " far\n",
            " farther\n",
        );
        let rows = snippet_side(deleted, 23, 23, Side::Old);
        let has = |s: &str| rows.iter().any(|r| r.text() == s);
        assert!(has("    gone") && has("near2"));
        assert!(!has("far") && !has("farther"));
        assert_eq!(snippet_caption_sign(&rows, 23, 23, Side::Old), Some('-'));
    }

    #[test]
    fn rows_from_snippet_does_not_keep_same_side_lines_past_the_margin() {
        let mut hunk = String::from("@@ -10,11 +10,11 @@\n keep\n");
        for i in 1..=10 {
            hunk.push_str("-    old");
            hunk.push_str(&i.to_string());
            hunk.push('\n');
        }
        for i in 1..=10 {
            hunk.push_str("+    new");
            hunk.push_str(&i.to_string());
            hunk.push('\n');
        }
        let rows = snippet_side(&hunk, 11, 11, Side::New);
        let has = |s: &str| rows.iter().any(|r| r.text() == s);
        assert!(has("    new1") && has("    old1"));
        assert!(has("    new4"), "pad includes new 14");
        assert!(!has("    new10"), "same-side past ±3 is dropped");
        assert!(has("    old10"), "the other half of the replace stays");
    }

    #[test]
    fn rows_from_snippet_is_empty_when_the_range_misses() {
        let hunk = "@@ -10,2 +10,2 @@\n-    old\n+    new\n";
        assert!(snippet(hunk, 99, 99).is_empty());
    }

    #[test]
    fn rows_from_snippet_skips_no_newline_and_keeps_other_lines() {
        let rows = snippet("-    old\n\\ No newline at end of file\nnot-a-marker\n", 1, 1);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].text(), "    old");
        assert_eq!(rows[1].text(), "not-a-marker");
        assert!(matches!(rows[1], Row::Context { .. }));
        assert_eq!(rows[1].new_no(), Some(0));
    }
}
