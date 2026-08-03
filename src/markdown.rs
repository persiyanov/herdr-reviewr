//! Markdown rendering for PR bodies and Markdown file previews.
//!
//! See `specs/markdown.md`. PR bodies always use the built-in `pulldown-cmark` renderer.
//! File previews may delegate to one configured argv command, with the built-in renderer as
//! the default and failure fallback.

use std::io::{Read, Write};
use std::ops::Range;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::highlight::Highlighter;
use crate::theme::Palette;

/// Block indents (quote bars, list levels) deeper than this render at the cap, so
/// pathological nesting can never squeeze the content column to nothing.
const MAX_NEST: usize = 8;

/// The indent code-block lines carry inside their block.
const CODE_INDENT: &str = "  ";

/// An external file-preview render gets one bounded wall-clock window. The timeout is not
/// configurable, so untrusted Markdown cannot extend it (`specs/markdown.md`).
const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound captured renderer output independently from the already-bounded file input.
const MAX_EXTERNAL_OUTPUT: u64 = 16 * 1024 * 1024;

/// Bounds on parsed external output complexity, before bytes become heap-allocated spans.
const MAX_EXTERNAL_LINES: usize = 100_000;
const MAX_EXTERNAL_STYLE_RUNS: usize = 250_000;

/// A shrunk table column never drops below this width (or its natural width, when
/// smaller), so wrapped cells stay readable (`specs/markdown.md`).
const COL_FLOOR: usize = 8;

/// Rendered markdown: the styled lines, one metadata entry per line in lockstep, and
/// the document's heading anchors — the position mapping and link hit-testing the
/// surfaces consume (`specs/diff-view.md`, `specs/markdown.md`).
#[derive(Clone, Debug, Default)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub meta: Vec<LineMeta>,
    /// Each heading's GitHub slug and the rendered line it starts on.
    pub anchors: Vec<(String, usize)>,
}

/// One rendered line's metadata: the 1-based source line it maps to (its block's first
/// line, or the exact line inside a code block) and the link spans it carries.
#[derive(Clone, Debug)]
pub struct LineMeta {
    pub source_line: usize,
    pub links: Vec<LinkSpan>,
}

/// A clickable span on one rendered line: `start..end` display columns and where it
/// goes. The destination is shared, so cloning a span (or a whole render) never copies
/// the url bytes.
#[derive(Clone, Debug)]
pub struct LinkSpan {
    pub start: usize,
    pub end: usize,
    pub url: std::sync::Arc<str>,
}

/// Render `text` as styled lines wrapped to `width` columns.
pub fn render(text: &str, width: usize, hl: &Highlighter, p: &Palette) -> Rendered {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut line_starts = vec![0usize];
    line_starts.extend(text.char_indices().filter(|(_, c)| *c == '\n').map(|(i, _)| i + 1));
    let mut r = Renderer {
        hl,
        p,
        width: width.max(1),
        source: text,
        line_starts,
        block_src: 1,
        out: Rendered::default(),
        inline: Vec::new(),
        styles: Vec::new(),
        quote: 0,
        lists: Vec::new(),
        marker: None,
        links: Vec::new(),
        urls: Vec::new(),
        heading_text: None,
        images: Vec::new(),
        code: None,
        table: None,
        needs_blank: false,
        pending_anchor: None,
        slug_counts: std::collections::HashMap::new(),
    };
    for (event, range) in Parser::new_ext(text, opts).into_offset_iter() {
        r.event(event, range);
    }
    r.flush_block(false);
    r.out
}

/// A single-slot render memo: the last `(text, width)` and its lines. One input is on
/// screen at a time per surface, so one slot absorbs the per-frame recompute
/// (`specs/markdown.md`). Cleared on a theme switch, which changes every color.
#[derive(Debug, Default)]
pub struct RenderCache {
    key: Option<(String, usize)>,
    rendered: Rendered,
}

/// The file-preview cache includes the configured argv in its key. A live config change therefore
/// replaces an on-screen render without affecting the PR cache (`specs/config.md`).
#[derive(Debug, Default)]
pub struct FileRenderCache {
    key: Option<(String, usize, Option<Vec<String>>, crate::theme::Appearance)>,
    rendered: Rendered,
}

impl RenderCache {
    pub fn get(&mut self, text: &str, width: usize, hl: &Highlighter, p: &Palette) -> Rendered {
        if !self.key.as_ref().is_some_and(|(t, w)| t == text && *w == width) {
            self.rendered = render(text, width, hl, p);
            self.key = Some((text.to_string(), width));
        }
        self.rendered.clone()
    }

    pub fn clear(&mut self) {
        self.key = None;
        self.rendered = Rendered::default();
    }
}

impl FileRenderCache {
    pub fn get(
        &mut self,
        text: &str,
        width: usize,
        command: Option<&[String]>,
        appearance: crate::theme::Appearance,
        hl: &Highlighter,
        p: &Palette,
    ) -> Rendered {
        let command = command.map(<[String]>::to_vec);
        if !self.key.as_ref().is_some_and(|(t, w, c, a)| {
            t == text && *w == width && *c == command && *a == appearance
        }) {
            self.rendered = command.as_deref().map_or_else(
                || render(text, width, hl, p),
                |argv| {
                    render_external(argv, text, width, appearance, EXTERNAL_TIMEOUT)
                        .unwrap_or_else(|| render(text, width, hl, p))
                },
            );
            self.key = Some((text.to_string(), width, command, appearance));
        }
        self.rendered.clone()
    }

    pub fn clear(&mut self) {
        self.key = None;
        self.rendered = Rendered::default();
    }
}

/// One styled run of inline text, not yet wrapped. `"\n"` is a forced line break.
/// `link` indexes the renderer's url list when the run belongs to a link's click target.
#[derive(Clone, Debug)]
struct Chunk {
    text: String,
    style: Style,
    link: Option<usize>,
}

/// An in-progress code block: the fence's language tag, its content, and whether a
/// fence line precedes the content in the source.
struct CodeBlock {
    lang: Option<String>,
    content: String,
    fenced: bool,
}

/// An in-progress table: its source range (the wide-table fallback), rows of cells of
/// chunks, and how many leading rows are the header.
struct Table {
    range: Range<usize>,
    rows: Vec<Vec<Vec<Chunk>>>,
    head_rows: usize,
}

struct Renderer<'a> {
    hl: &'a Highlighter,
    p: &'a Palette,
    width: usize,
    source: &'a str,
    /// Byte offset of each 1-based source line's start, for offset → line mapping.
    line_starts: Vec<usize>,
    /// The 1-based source line the block being emitted starts on.
    block_src: usize,
    out: Rendered,
    inline: Vec<Chunk>,
    /// The emphasis/link/heading style stack; the current style folds base + every entry.
    styles: Vec<Style>,
    quote: usize,
    /// One entry per open list: the next ordinal for an ordered list, `None` for bullets.
    lists: Vec<Option<u64>>,
    /// The pending item marker, consumed by the item's first flushed line.
    marker: Option<String>,
    /// Open links: the url-list index and the chunk index where the link's text starts.
    links: Vec<(usize, usize)>,
    /// Every link destination seen, indexed by the chunks that belong to it.
    urls: Vec<std::sync::Arc<str>>,
    /// The heading text being collected for its slug — prose only, never a link's
    /// appended destination, so `## See [docs](url)` slugs as GitHub does.
    heading_text: Option<String>,
    /// Open images: the chunk index where the alt text starts, and the heading-text
    /// length to roll back to — alt text stays out of a heading's slug, as on GitHub.
    images: Vec<(usize, usize)>,
    code: Option<CodeBlock>,
    table: Option<Table>,
    needs_blank: bool,
    /// The slug the next flushed line carries as its heading anchor.
    pending_anchor: Option<String>,
    /// Slugs seen so far, so duplicate headings number like GitHub's.
    slug_counts: std::collections::HashMap<String, usize>,
}

impl Renderer<'_> {
    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some(code) = &mut self.code {
                    code.content.push_str(&t);
                } else {
                    self.push_text(&t, self.current_style());
                }
            }
            Event::Code(t) => {
                let style = self.current_style().fg(self.p.peach);
                self.push_text(&t, style);
            }
            Event::SoftBreak => self.push_text(" ", self.current_style()),
            Event::HardBreak => {
                let style = self.current_style();
                let link = self.current_link();
                self.push_chunk("\n".into(), style, link);
            }
            Event::Rule => {
                self.flush_block(true);
                self.block_src = self.src_line(range.start);
                self.blank_before_block();
                let budget = self.budget(self.prefix(None).0.width());
                let line = Line::from(vec![
                    self.prefix(None).0,
                    Span::styled("─".repeat(budget), Style::default().fg(self.p.overlay0)),
                ]);
                self.push_plain_line(line);
                self.needs_blank = true;
            }
            // Raw HTML shows as its dim source text: the parser's own classification
            // decides — an inline tag stays inline (`## <a name="x"></a>Title` is one
            // heading), a block becomes its own dim lines (`specs/markdown.md`).
            Event::InlineHtml(t) => {
                let style = Style::default().fg(self.p.overlay0);
                let link = self.current_link();
                // Straight to the chunks: tag text never enters a heading's slug.
                self.push_chunk(sanitize(&t), style, link);
            }
            Event::Html(t) => {
                let style = Style::default().fg(self.p.overlay0);
                if self.table.is_some() {
                    self.push_chunk(sanitize(&t), style, None);
                } else {
                    // A tight list item's text can still be pending: it emits first,
                    // with its marker, so the HTML block never jumps ahead of it.
                    self.flush_block(true);
                    self.block_src = self.src_line(range.start);
                    self.blank_before_block();
                    for html_line in t.trim_end_matches('\n').split('\n') {
                        self.emit_fragments(vec![(sanitize(html_line), style)], "");
                    }
                    self.needs_blank = true;
                }
            }
            Event::TaskListMarker(done) => {
                self.marker = Some(if done { "☑ ".into() } else { "☐ ".into() });
            }
            // Footnotes and math are not enabled; their syntax arrives as literal text.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        // A block-level tag stamps the source line every rendered line of the block maps
        // back to (`specs/diff-view.md` position mapping).
        if matches!(
            tag,
            Tag::Paragraph | Tag::Heading { .. } | Tag::Item | Tag::CodeBlock(_) | Tag::Table(_)
        ) {
            self.block_src = self.src_line(range.start);
        }
        match tag {
            Tag::Heading { level, .. } => {
                self.heading_text = Some(String::new());
                self.styles.push(self.heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_block(true);
                self.quote += 1;
            }
            Tag::List(start) => {
                // "- a" followed by a nested list flushes "a" before the depth changes.
                self.flush_block(false);
                self.lists.push(start);
            }
            Tag::Item => {
                self.marker = Some(match self.lists.last().copied().flatten() {
                    Some(n) => {
                        if let Some(slot) = self.lists.last_mut() {
                            *slot = Some(n + 1);
                        }
                        format!("{n}. ")
                    }
                    None => "• ".into(),
                });
            }
            Tag::CodeBlock(kind) => {
                self.flush_block(true);
                let (lang, fenced) = match kind {
                    CodeBlockKind::Fenced(info) => {
                        (info.split_whitespace().next().map(str::to_string), true)
                    }
                    CodeBlockKind::Indented => (None, false),
                };
                self.code = Some(CodeBlock { lang, content: String::new(), fenced });
            }
            Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                let at = self.chunk_len();
                self.urls.push(std::sync::Arc::from(dest_url.as_ref()));
                self.links.push((self.urls.len() - 1, at));
                let lavender = self.p.lavender;
                self.push_style(|s| s.fg(lavender).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { .. } => {
                let at = self.chunk_len();
                let heading_len = self.heading_text.as_ref().map_or(0, String::len);
                self.images.push((at, heading_len));
            }
            Tag::Table(_) => {
                self.flush_block(true);
                self.table = Some(Table { range, rows: Vec::new(), head_rows: 0 });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(Vec::new());
                }
            }
            // Paragraph / FootnoteDefinition / HtmlBlock carry no styling of their own.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_block(true),
            TagEnd::Heading(_) => {
                let text = self.heading_text.take().unwrap_or_default();
                let slug = self.slugify(&text);
                if !slug.is_empty() {
                    self.pending_anchor = Some(slug);
                }
                self.flush_block(true);
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_block(true);
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.flush_block(false);
                self.lists.pop();
                self.needs_blank = true;
            }
            TagEnd::Item => {
                self.flush_block(false);
                self.marker = None;
            }
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => self.end_link(),
            TagEnd::Image => self.end_image(),
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head_rows = t.rows.len();
                }
            }
            TagEnd::Table => self.end_table(),
            _ => {}
        }
    }

    // ---- inline accumulation -----------------------------------------------------------

    /// Where new chunks land: the open table cell, or the block's inline run.
    fn chunks_mut(&mut self) -> &mut Vec<Chunk> {
        match self.table.as_mut().and_then(|t| t.rows.last_mut()).and_then(|r| r.last_mut()) {
            Some(cell) => cell,
            None => &mut self.inline,
        }
    }

    fn chunk_len(&mut self) -> usize {
        self.chunks_mut().len()
    }

    fn push_text(&mut self, text: &str, style: Style) {
        let clean = sanitize(text);
        if let Some(h) = &mut self.heading_text {
            h.push_str(&clean);
        }
        let link = self.current_link();
        self.push_chunk(clean, style, link);
    }

    fn push_chunk(&mut self, text: String, style: Style, link: Option<usize>) {
        self.chunks_mut().push(Chunk { text, style, link });
    }

    /// The innermost open link's url index, stamped onto every chunk inside it.
    fn current_link(&self) -> Option<usize> {
        self.links.last().map(|(id, _)| *id)
    }

    fn push_style(&mut self, patch: impl FnOnce(Style) -> Style) {
        self.styles.push(patch(self.current_style()));
    }

    fn current_style(&self) -> Style {
        self.styles.last().copied().unwrap_or_else(|| Style::default().fg(self.p.text))
    }

    /// Heading style: bold in an accent, deeper levels dimmer (`specs/markdown.md`).
    fn heading_style(&self, level: HeadingLevel) -> Style {
        let fg = match level {
            HeadingLevel::H1 | HeadingLevel::H2 => self.p.mauve,
            HeadingLevel::H3 => self.p.lavender,
            _ => self.p.subtext0,
        };
        Style::default().fg(fg).add_modifier(Modifier::BOLD)
    }

    /// Close a link: when its visible text differs from the destination, append the
    /// destination dim (`specs/markdown.md`).
    fn end_link(&mut self) {
        self.styles.pop();
        let Some((id, start)) = self.links.pop() else {
            return;
        };
        let dest = self.urls[id].clone();
        let text: String = self.chunks_mut()[start..].iter().map(|c| c.text.as_str()).collect();
        if !dest.is_empty() && text != *dest {
            let style = Style::default().fg(self.p.overlay0);
            // The dim destination shares the click target with the text (`specs/markdown.md`).
            self.push_chunk(format!(" ({})", sanitize(&dest)), style, Some(id));
        }
    }

    /// Collapse an image to its dim `⧉ alt-text` placeholder.
    fn end_image(&mut self) {
        let Some((start, heading_len)) = self.images.pop() else {
            return;
        };
        if let Some(h) = &mut self.heading_text {
            h.truncate(heading_len);
        }
        let style = Style::default().fg(self.p.overlay0);
        let chunks = self.chunks_mut();
        let alt: String = chunks[start..].iter().map(|c| c.text.as_str()).collect();
        chunks.truncate(start);
        let alt = alt.trim();
        let text = if alt.is_empty() { "⧉ image".to_string() } else { format!("⧉ {alt}") };
        // An image inside a link stays part of that link's click target.
        let link = self.current_link();
        self.push_chunk(text, style, link);
    }

    // ---- block emission ------------------------------------------------------------------

    /// The current block prefixes: quote bars plus list indent, with the pending item
    /// marker on the first line. Returns `(first, continuation)` spans.
    fn prefix(&self, marker: Option<&str>) -> (Span<'static>, Span<'static>) {
        let bars = "▎".repeat(self.quote.min(MAX_NEST));
        let gap = if bars.is_empty() { "" } else { " " };
        let indent = "  ".repeat(self.lists.len().min(MAX_NEST).saturating_sub(1));
        let marker = marker.unwrap_or("");
        let first = format!("{bars}{gap}{indent}{marker}");
        let cont = format!("{bars}{gap}{indent}{}", " ".repeat(marker.width()));
        let style = Style::default().fg(self.p.overlay0);
        (Span::styled(first, style), Span::styled(cont, style))
    }

    /// Content columns left of a `prefix_width`-wide prefix.
    fn budget(&self, prefix_width: usize) -> usize {
        self.width.saturating_sub(prefix_width).max(1)
    }

    /// The 1-based source line holding byte offset `at`.
    fn src_line(&self, at: usize) -> usize {
        self.line_starts.partition_point(|&start| start <= at)
    }

    /// Emit one rendered line with its metadata; lines and meta stay in lockstep. The
    /// pending heading anchor lands in the anchor list at the block's first line.
    fn push_line(&mut self, line: Line<'static>, links: Vec<LinkSpan>) {
        if let Some(slug) = self.pending_anchor.take() {
            self.out.anchors.push((slug, self.out.lines.len()));
        }
        self.out.meta.push(LineMeta { source_line: self.block_src, links });
        self.out.lines.push(line);
    }

    /// Emit one rendered line with no links, mapped to the current block.
    fn push_plain_line(&mut self, line: Line<'static>) {
        self.push_line(line, Vec::new());
    }

    /// One blank separator line before a new block, when a block already ended above.
    /// It bypasses [`Self::push_line`], so it can never consume a pending heading anchor.
    fn blank_before_block(&mut self) {
        if self.needs_blank && !self.out.lines.is_empty() {
            let bars = "▎".repeat(self.quote.min(MAX_NEST));
            self.out.meta.push(LineMeta { source_line: self.block_src, links: Vec::new() });
            self.out.lines.push(if bars.is_empty() {
                Line::default()
            } else {
                Line::from(Span::styled(bars, Style::default().fg(self.p.overlay0)))
            });
        }
        self.needs_blank = false;
    }

    /// Wrap and emit the pending inline run as one block. `set_blank` marks a block
    /// boundary, so the next block opens after a separator line.
    fn flush_block(&mut self, set_blank: bool) {
        if self.inline.iter().all(|c| c.text.trim().is_empty()) {
            self.inline.clear();
            if set_blank {
                self.needs_blank = true;
            }
            return;
        }
        self.blank_before_block();
        let marker = self.marker.take();
        let (first, cont) = self.prefix(marker.as_deref());
        let chunks = std::mem::take(&mut self.inline);
        let fragments: Vec<Fragment> =
            chunks.into_iter().map(|c| (c.text, c.style, c.link)).collect();
        let wrapped = wrap_fragments(&fragments, self.budget(first.width()), true);
        for (i, (spans, links)) in wrapped.into_iter().enumerate() {
            let prefix = if i == 0 { first.clone() } else { cont.clone() };
            // Link columns come back content-relative; the prefix shifts them on screen.
            let off = prefix.width();
            let link_spans = links
                .into_iter()
                .map(|(start, end, id)| LinkSpan {
                    start: start + off,
                    end: end + off,
                    url: self.urls[id].clone(),
                })
                .collect();
            let mut line = vec![prefix];
            line.extend(spans);
            self.push_line(Line::from(line), link_spans);
        }
        if set_blank {
            self.needs_blank = true;
        }
    }

    /// Emit one already-styled fragment run as block lines, char-wrapped, under the
    /// current block prefix plus `extra_indent`. A pending item marker lands on the
    /// first line, so a list item whose first block is a code block (or table, or
    /// HTML) still shows its bullet.
    fn emit_fragments(&mut self, fragments: Vec<(String, Style)>, extra_indent: &str) {
        let marker = self.marker.take();
        let (first, cont) = self.prefix(marker.as_deref());
        let style = Style::default().fg(self.p.overlay0);
        let first = Span::styled(format!("{}{extra_indent}", first.content), style);
        let cont = Span::styled(format!("{}{extra_indent}", cont.content), style);
        let budget = self.budget(cont.width());
        let linkless: Vec<Fragment> = fragments.into_iter().map(|(t, s)| (t, s, None)).collect();
        let wrapped = wrap_fragments(&linkless, budget, false);
        if wrapped.is_empty() {
            self.push_plain_line(Line::from(first));
            return;
        }
        for (i, (spans, _)) in wrapped.into_iter().enumerate() {
            let mut line = vec![if i == 0 { first.clone() } else { cont.clone() }];
            line.extend(spans);
            self.push_plain_line(Line::from(line));
        }
    }

    /// Close a code block: highlight it whole through the shared highlighter, then emit
    /// each line indented, char-wrapped to the pane.
    fn end_code(&mut self) {
        let Some(CodeBlock { lang, content, fenced }) = self.code.take() else {
            return;
        };
        self.blank_before_block();
        let content = content.replace('\t', "    ");
        let highlighted = self.hl.highlight(&content, lang.as_deref());
        // Code maps line-accurately: a fence line precedes fenced content in the source.
        let block_start = self.block_src + usize::from(fenced);
        for (i, line) in highlighted.into_iter().enumerate() {
            self.block_src = block_start + i;
            let fragments: Vec<(String, Style)> = line
                .into_iter()
                .map(|s| (sanitize(&s.text), Style::default().fg(crate::ui::rgb(s.color))))
                .collect();
            self.emit_fragments(fragments, CODE_INDENT);
        }
        self.needs_blank = true;
    }

    /// Close a table: aligned columns with a bold header, an over-wide table shrinking
    /// its widest column and wrapping that column's cells, and dim source text only when
    /// the column floors still overflow the pane (`specs/markdown.md`).
    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        self.blank_before_block();
        // The pending item marker lands on the first row, like every block emitter —
        // `first` and `cont` share a width, so the column accounting is unchanged.
        let marker = self.marker.take();
        let (first, cont) = self.prefix(marker.as_deref());
        let budget = self.budget(cont.width());

        let cell_text =
            |cell: &[Chunk]| -> String { cell.iter().map(|c| c.text.as_str()).collect() };
        let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0usize; cols];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell_text(cell).width());
            }
        }
        // An over-wide table shrinks its widest columns, never below their floors: each
        // pass levels every widest column down toward the next-highest width or its
        // floor, whichever is larger, so cost is bounded by the column count, not by
        // how over-wide a cell's content is. Tied widest columns shrink together, as
        // the reference renderers do (`specs/markdown.md`).
        let floors: Vec<usize> = widths.iter().map(|w| (*w).min(COL_FLOOR)).collect();
        let sep_total = 3 * cols.saturating_sub(1);
        let mut deficit = (widths.iter().sum::<usize>() + sep_total).saturating_sub(budget);
        while deficit > 0 {
            let Some(hi) = (0..cols).filter(|&i| widths[i] > floors[i]).map(|i| widths[i]).max()
            else {
                break;
            };
            let ties: Vec<usize> =
                (0..cols).filter(|&i| widths[i] == hi && hi > floors[i]).collect();
            let step = (0..cols)
                .map(|j| widths[j])
                .filter(|&w| w < hi)
                .chain(ties.iter().map(|&i| floors[i]))
                .max()
                .expect("ties are non-empty");
            let room = (hi - step) * ties.len();
            if room >= deficit {
                let per = deficit / ties.len();
                let extra = deficit % ties.len();
                // The rightmost ties give up the remainder cell, so the split is exact.
                for (k, &i) in ties.iter().rev().enumerate() {
                    widths[i] = hi - per - usize::from(k < extra);
                }
                deficit = 0;
            } else {
                for &i in &ties {
                    widths[i] = step;
                }
                deficit -= room;
            }
        }
        let total: usize = widths.iter().sum::<usize>() + sep_total;

        if deficit > 0 {
            // Over-wide at every floor: the table renders as its source text instead.
            let style = Style::default().fg(self.p.overlay0);
            let src = self.source.get(table.range.clone()).unwrap_or("");
            for src_line in src.trim_end_matches('\n').split('\n') {
                self.emit_fragments(vec![(sanitize(src_line), style)], "");
            }
            self.needs_blank = true;
            return;
        }

        let dim = Style::default().fg(self.p.overlay0);
        for (r, row) in table.rows.iter().enumerate() {
            let head = r < table.head_rows;
            let style_of =
                |c: &Chunk| if head { c.style.add_modifier(Modifier::BOLD) } else { c.style };
            // A cell that fits its column emits verbatim, spaces and all; only a cell
            // wider than its shrunk column wraps inside it (`specs/markdown.md`). The
            // row is as tall as its tallest cell.
            let cells: Vec<Vec<WrappedLine>> = (0..cols)
                .map(|i| {
                    let cell = row.get(i).map_or(&[][..], Vec::as_slice);
                    if cell_text(cell).width() <= widths[i] {
                        let mut spans: Vec<Span<'static>> = Vec::new();
                        let mut links: Vec<(usize, usize, usize)> = Vec::new();
                        let mut used = 0;
                        for c in cell {
                            let w = c.text.width();
                            if let Some(id) = c.link {
                                match links.last_mut() {
                                    Some((_, end, last)) if *last == id && *end == used => {
                                        *end += w;
                                    }
                                    _ => links.push((used, used + w, id)),
                                }
                            }
                            used += w;
                            spans.push(Span::styled(c.text.clone(), style_of(c)));
                        }
                        if spans.is_empty() { Vec::new() } else { vec![(spans, links)] }
                    } else {
                        let fragments: Vec<Fragment> =
                            cell.iter().map(|c| (c.text.clone(), style_of(c), c.link)).collect();
                        wrap_fragments(&fragments, widths[i], true)
                    }
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(0).max(1);
            let mut cells: Vec<std::vec::IntoIter<WrappedLine>> =
                cells.into_iter().map(Vec::into_iter).collect();
            for l in 0..height {
                let lead = if r == 0 && l == 0 { first.clone() } else { cont.clone() };
                let mut spans: Vec<Span<'static>> = vec![lead];
                let mut links: Vec<LinkSpan> = Vec::new();
                let mut col = cont.width();
                for (i, width) in widths.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::styled(" │ ", dim));
                        col += 3;
                    }
                    let mut used = 0;
                    if let Some((cell_spans, cell_links)) = cells[i].next() {
                        for (start, end, id) in cell_links {
                            links.push(LinkSpan {
                                start: col + start,
                                end: col + end,
                                url: self.urls[id].clone(),
                            });
                        }
                        used = cell_spans.iter().map(Span::width).sum();
                        spans.extend(cell_spans);
                    }
                    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
                    col += width;
                }
                self.push_line(Line::from(spans), links);
            }
            if head && r + 1 == table.head_rows {
                self.push_plain_line(Line::from(vec![
                    cont.clone(),
                    Span::styled("─".repeat(total), dim),
                ]));
            }
        }
        self.needs_blank = true;
    }
}

impl Renderer<'_> {
    /// GitHub's heading slug with duplicate numbering (`-1`, `-2`, …) over
    /// [`slug_text`]'s normalization.
    fn slugify(&mut self, text: &str) -> String {
        let slug = slug_text(text);
        let n = self.slug_counts.entry(slug.clone()).or_insert(0);
        let out = if *n == 0 { slug.clone() } else { format!("{slug}-{n}") };
        *n += 1;
        out
    }
}

/// GitHub's slug normalization: lowercase, spaces to hyphens, everything but letters,
/// digits, hyphens, and underscores dropped. The click side runs a fragment through the
/// same transform, so `#Set-Up!` finds the `set-up` heading (`specs/markdown.md`).
pub(crate) fn slug_text(text: &str) -> String {
    let mut slug = String::new();
    for c in text.trim().to_lowercase().chars() {
        match c {
            ' ' => slug.push('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => slug.push(c),
            _ => {}
        }
    }
    slug
}

/// Run one configured Markdown file-preview command directly. The repository text is stdin-only;
/// `{width}` and `{style}` substitution applies to already-tokenized trusted arguments. Any spawn,
/// I/O, exit, timeout, size, or ANSI-conversion failure returns `None` for the built-in fallback.
fn render_external(
    command: &[String],
    input: &str,
    width: usize,
    appearance: crate::theme::Appearance,
    timeout: Duration,
) -> Option<Rendered> {
    let width = width.max(1).to_string();
    let argv: Vec<String> = command
        .iter()
        .map(|arg| arg.replace("{width}", &width).replace("{style}", appearance.as_str()))
        .collect();
    let (program, command_args) = argv.split_first()?;
    let mut child = Command::new(program)
        .args(command_args)
        // Glow otherwise selects its no-color output profile when stdout is a pipe.
        .env("CLICOLOR_FORCE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;

    let mut stdin = child.stdin.take()?;
    let owned = input.to_owned();
    let (stdin_tx, stdin_rx) = mpsc::channel();
    let stdin_thread = std::thread::spawn(move || {
        let ok = stdin.write_all(owned.as_bytes()).is_ok();
        let _ = stdin_tx.send(ok);
    });

    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    let stdout_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(MAX_EXTERNAL_OUTPUT + 1).read_to_end(&mut bytes).map(|_| bytes);
        let _ = tx.send(result);
    });

    let deadline = Instant::now() + timeout;
    let bytes = match rx.recv_timeout(timeout) {
        Ok(Ok(bytes)) if bytes.len() as u64 <= MAX_EXTERNAL_OUTPUT => bytes,
        _ => {
            crate::proc::terminate_process_group(&mut child);
            let _ = stdin_thread.join();
            let _ = stdout_thread.join();
            return None;
        }
    };
    let stdin_ok =
        stdin_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or(false);
    if !stdin_ok {
        crate::proc::terminate_process_group(&mut child);
        let _ = stdin_thread.join();
        let _ = stdout_thread.join();
        return None;
    }
    let status =
        crate::proc::wait_bounded(&mut child, deadline.saturating_duration_since(Instant::now()));
    let _ = stdin_thread.join();
    let _ = stdout_thread.join();
    let status = status?;
    if !status.success() {
        return None;
    }

    external_output(&String::from_utf8_lossy(&bytes))
}

/// Keep only Select Graphic Rendition (SGR) ANSI styling and convert it into ratatui spans.
/// Cursor, screen, hyperlink, bell, carriage-return, and other terminal controls never reach
/// the terminal (`specs/markdown.md`).
fn external_output(raw: &str) -> Option<Rendered> {
    let clean = strip_terminal_control(raw);
    if clean.lines().count() > MAX_EXTERNAL_LINES
        || clean.matches('\u{1b}').take(MAX_EXTERNAL_STYLE_RUNS + 1).count()
            > MAX_EXTERNAL_STYLE_RUNS
    {
        return None;
    }
    let text = clean.into_text().ok()?;
    let lines = text.lines;
    // External renderers do not expose source blocks, links, or heading identities. A constant
    // source identity makes source/preview position transfer conservatively return to the top.
    let meta = lines.iter().map(|_| LineMeta { source_line: 1, links: Vec::new() }).collect();
    Some(Rendered { lines, meta, anchors: Vec::new() })
}

/// Strip every terminal control sequence except SGR (`CSI ... m`). This byte-level pass follows
/// the external renderer boundary used by herdr-file-viewer, then neutralizes Unicode controls.
fn strip_terminal_control(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            match bytes.get(i + 1) {
                Some(b'[') => {
                    let start = i;
                    let mut end = i + 2;
                    while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                        end += 1;
                    }
                    if end < bytes.len() && bytes[end] == b'm' {
                        out.extend_from_slice(&bytes[start..=end]);
                    }
                    i = end.saturating_add(1);
                }
                Some(b']') => i = control_string_end(bytes, i + 2, true),
                Some(b'P' | b'X' | b'^' | b'_') => {
                    i = control_string_end(bytes, i + 2, false);
                }
                Some(_) => i += 2,
                None => i += 1,
            }
        } else if bytes[i] == 0xc2 {
            match bytes.get(i + 1) {
                Some(0x9b) => {
                    let mut end = i + 2;
                    while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                        end += 1;
                    }
                    if end < bytes.len() && bytes[end] == b'm' {
                        out.extend_from_slice(b"\x1b[");
                        out.extend_from_slice(&bytes[i + 2..=end]);
                    }
                    i = end.saturating_add(1);
                }
                Some(0x9d) => {
                    i = control_string_end(bytes, i + 2, true);
                }
                Some(0x90 | 0x98 | 0x9e | 0x9f) => {
                    i = control_string_end(bytes, i + 2, false);
                }
                Some(0x80..=0x9f) => i += 2,
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        } else {
            let byte = bytes[i];
            if (byte >= 0x20 || matches!(byte, b'\n' | b'\t')) && byte != 0x7f {
                out.push(byte);
            }
            i += 1;
        }
    }
    String::from_utf8_lossy(&out)
        .chars()
        .map(|ch| {
            if matches!(
                ch,
                '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}'
            ) {
                '�'
            } else {
                ch
            }
        })
        .collect()
}

/// Consume an OSC/DCS/SOS/PM/APC control string through ST (`ESC \\`) or C1 ST. OSC also accepts
/// BEL as its terminator; the other control strings treat BEL as payload.
fn control_string_end(bytes: &[u8], mut at: usize, bel_terminates: bool) -> usize {
    while at < bytes.len() {
        if bel_terminates && bytes[at] == 0x07 {
            return at + 1;
        }
        if bytes[at] == 0x1b && bytes.get(at + 1) == Some(&b'\\') {
            return at + 2;
        }
        if bytes[at] == 0xc2 && bytes.get(at + 1) == Some(&0x9c) {
            return at + 2;
        }
        at += 1;
    }
    at
}

/// A character the terminal must never receive raw: a control character or an explicit
/// bidirectional override. One predicate serves the display ([`sanitize`]) and the link
/// opener (`browser::openable_url`), so what the display would neutralize can never
/// open as different bytes (`specs/markdown.md`).
pub(crate) fn hostile_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
}

/// Neutralize text the terminal must never interpret: hostile characters render as a
/// visible placeholder; tabs widen to spaces (`specs/markdown.md`).
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if hostile_char(c) => out.push('�'),
            c => out.push(c),
        }
    }
    out
}

/// One styled run entering the wrapper: text, style, and its link id when clickable.
type Fragment = (String, Style, Option<usize>);

/// One wrapped output line: its spans, plus content-relative link column runs as
/// `(start, end, url index)`.
type WrappedLine = (Vec<Span<'static>>, Vec<(usize, usize, usize)>);

/// Wrap styled fragments into lines of at most `budget` display cells. `by_words` breaks
/// at word boundaries (hard-breaking an over-wide word); otherwise anywhere. A `"\n"`
/// inside a fragment forces a break.
fn wrap_fragments(fragments: &[Fragment], budget: usize, by_words: bool) -> Vec<WrappedLine> {
    let mut w = Wrapper {
        budget: budget.max(1),
        lines: Vec::new(),
        line: Vec::new(),
        line_links: Vec::new(),
        line_w: 0,
        word: Vec::new(),
        word_w: 0,
    };
    for (text, style, link) in fragments {
        for ch in text.chars() {
            match ch {
                '\n' => {
                    w.place_word();
                    w.flush_line();
                }
                ' ' if by_words => w.place_word(),
                c if by_words => w.push_word_char(c, *style, *link),
                c => w.place_char(c, *style, *link),
            }
        }
    }
    w.place_word();
    if !w.line.is_empty() {
        w.lines.push((w.line, w.line_links));
    }
    w.lines
}

/// The wrap state: finished lines, the line being filled (spans plus link column runs),
/// and the word being assembled (a word can span styled fragments).
struct Wrapper {
    budget: usize,
    lines: Vec<WrappedLine>,
    line: Vec<Span<'static>>,
    line_links: Vec<(usize, usize, usize)>,
    line_w: usize,
    word: Vec<(String, Style, Option<usize>)>,
    word_w: usize,
}

impl Wrapper {
    fn flush_line(&mut self) {
        self.lines.push((std::mem::take(&mut self.line), std::mem::take(&mut self.line_links)));
        self.line_w = 0;
    }

    /// Append `text` to the line, merging into the last span when the style matches and
    /// extending the line's last link run when `link` continues it.
    fn place(&mut self, text: &str, style: Style, link: Option<usize>, w: usize) {
        if let Some(id) = link {
            match self.line_links.last_mut() {
                Some((_, end, last)) if *last == id && *end == self.line_w => *end += w,
                _ => self.line_links.push((self.line_w, self.line_w + w, id)),
            }
        }
        if let Some(last) = self.line.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(text);
        } else {
            self.line.push(Span::styled(text.to_string(), style));
        }
        self.line_w += w;
    }

    /// Append one character, breaking the line first when it would overflow.
    fn place_char(&mut self, c: char, style: Style, link: Option<usize>) {
        let w = char_width(c);
        if self.line_w + w > self.budget && self.line_w > 0 {
            self.flush_line();
        }
        self.place(c.encode_utf8(&mut [0u8; 4]), style, link, w);
    }

    /// Grow the pending word by one character.
    fn push_word_char(&mut self, c: char, style: Style, link: Option<usize>) {
        if let Some((t, s, l)) = self.word.last_mut()
            && *s == style
            && *l == link
        {
            t.push(c);
        } else {
            self.word.push((c.to_string(), style, link));
        }
        self.word_w += char_width(c);
    }

    /// Place the assembled word: break the line first when the word no longer fits, and
    /// hard-break the word itself when it is wider than a whole line.
    fn place_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let needs_sep = self.line_w > 0;
        if needs_sep && self.line_w + 1 + self.word_w > self.budget && self.word_w <= self.budget {
            self.flush_line();
        } else if needs_sep {
            // A separator keeps a style only when both sides share it (a space inside one
            // styled run merges into it); between differently styled runs it goes plain,
            // so an underline never bleeds into the gap on either side of a link.
            let next = self.word[0].1;
            let style = match self.line.last() {
                Some(prev) if prev.style == next => next,
                _ => Style::default(),
            };
            // The link id follows its own rule: both sides in one link keep the gap
            // clickable — the plain-styled space between link text and its dim
            // destination is still part of the click target (specs/markdown.md).
            let next_link = self.word[0].2;
            let prev_link = self
                .line_links
                .last()
                .filter(|(_, end, _)| *end == self.line_w)
                .map(|(.., id)| *id);
            let link = if prev_link == next_link { next_link } else { None };
            self.place(" ", style, link, 1);
        }
        for (text, style, link) in std::mem::take(&mut self.word) {
            for ch in text.chars() {
                self.place_char(ch, style, link);
            }
        }
        self.word_w = 0;
    }
}

fn char_width(c: char) -> usize {
    UnicodeWidthStr::width(c.encode_utf8(&mut [0u8; 4]) as &str)
}

#[cfg(test)]
mod tests {
    use super::{LinkSpan, RenderCache, Rendered, render};
    use crate::highlight::Highlighter;
    use crate::theme::{self, Palette};
    use ratatui::style::Modifier;
    use ratatui::text::Line;
    use std::time::{Duration, Instant};

    fn setup() -> (Highlighter, Palette) {
        let t = theme::resolve(Some("catppuccin"));
        (Highlighter::new(t.syntax), t.palette)
    }

    /// The rendered lines alone, for the element-presentation tests.
    fn render_lines(md: &str, width: usize, hl: &Highlighter, p: &Palette) -> Vec<Line<'static>> {
        let Rendered { lines, meta, .. } = render(md, width, hl, p);
        assert_eq!(lines.len(), meta.len(), "lines and meta stay in lockstep");
        lines
    }

    fn text_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    #[test]
    fn heading_is_bold_accent_without_markers() {
        let (hl, p) = setup();
        let lines = render_lines("## Install", 80, &hl, &p);
        assert_eq!(texts(&lines), vec!["Install"]);
        let span = &lines[0].spans[1]; // [0] is the (empty) prefix
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(span.style.fg, Some(p.mauve));
    }

    #[test]
    fn deeper_headings_dim() {
        let (hl, p) = setup();
        let h4 = render_lines("#### Notes", 80, &hl, &p);
        assert_eq!(h4[0].spans[1].style.fg, Some(p.subtext0));
    }

    #[test]
    fn emphasis_maps_to_terminal_attributes() {
        let (hl, p) = setup();
        let lines = render_lines("a **b** *c* ~~d~~", 80, &hl, &p);
        let spans = &lines[0].spans;
        let find = |t: &str| spans.iter().find(|s| s.content.contains(t)).unwrap();
        assert!(find("b").style.add_modifier.contains(Modifier::BOLD));
        assert!(find("c").style.add_modifier.contains(Modifier::ITALIC));
        assert!(find("d").style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn inline_code_gets_the_code_tint() {
        let (hl, p) = setup();
        let lines = render_lines("run `cargo test` now", 80, &hl, &p);
        let code = lines[0].spans.iter().find(|s| s.content.contains("cargo test")).unwrap();
        assert_eq!(code.style.fg, Some(p.peach));
    }

    #[test]
    fn fenced_rust_highlights_like_the_diff_pane() {
        let (hl, p) = setup();
        let lines = render_lines("```rust\nlet x = 1;\n```", 80, &hl, &p);
        assert_eq!(texts(&lines), vec!["  let x = 1;"]);
        // `let` keyword takes a syntax color different from the plain text color.
        let colors: Vec<_> = lines[0].spans.iter().filter_map(|s| s.style.fg).collect();
        assert!(colors.len() > 2, "rust tokenizes into several colored spans: {colors:?}");
        assert!(colors.iter().any(|c| *c != p.text));
    }

    #[test]
    fn fence_language_names_resolve_like_extensions() {
        let (hl, p) = setup();
        // "rust" is a token name, not an extension; both must highlight.
        for fence in ["rust", "rs"] {
            let lines = render_lines(&format!("```{fence}\nlet x = 1;\n```"), 80, &hl, &p);
            assert!(lines[0].spans.len() > 2, "```{fence} should highlight");
        }
    }

    #[test]
    fn link_text_carries_a_dim_destination_when_it_differs() {
        let (hl, p) = setup();
        let lines = render_lines("see [the run](https://ci.example/1)", 80, &hl, &p);
        let text = text_of(&lines[0]);
        assert_eq!(text, "see the run (https://ci.example/1)");
        let dest = lines[0].spans.iter().find(|s| s.content.contains("ci.example")).unwrap();
        assert_eq!(dest.style.fg, Some(p.overlay0));
        let label = lines[0].spans.iter().find(|s| s.content.contains("the run")).unwrap();
        assert!(label.style.add_modifier.contains(Modifier::UNDERLINED));

        let auto = render_lines("<https://ci.example/1>", 80, &hl, &p);
        assert_eq!(text_of(&auto[0]), "https://ci.example/1");
    }

    #[test]
    fn the_underline_never_bleeds_into_the_gaps_around_a_link() {
        let (hl, p) = setup();
        // One-word link followed by its dim destination, and a two-word link whose
        // interior space stays part of the underlined run.
        for md in ["built for [herdr](https://herdr.dev).", "see [the run](https://ci.example/1)"] {
            let lines = render_lines(md, 80, &hl, &p);
            for span in &lines[0].spans {
                if span.style.add_modifier.contains(Modifier::UNDERLINED) {
                    assert!(
                        !span.content.starts_with(' ') && !span.content.ends_with(' '),
                        "an underlined span must not reach into a separator gap: {:?}",
                        span.content
                    );
                }
            }
        }
        // The link text itself stays underlined, interior space included.
        let lines = render_lines("see [the run](https://ci.example/1)", 80, &hl, &p);
        let link = lines[0].spans.iter().find(|s| s.content.contains("the run")).unwrap();
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn image_collapses_to_an_alt_placeholder() {
        let (hl, p) = setup();
        let lines = render_lines("![build badge](https://img.example/b.svg)", 80, &hl, &p);
        assert_eq!(text_of(&lines[0]), "⧉ build badge");
    }

    #[test]
    fn list_items_mark_and_hang_their_wraps() {
        let (hl, p) = setup();
        let lines = render_lines("- alpha beta gamma delta", 14, &hl, &p);
        let t = texts(&lines);
        assert_eq!(t[0], "• alpha beta");
        assert_eq!(t[1], "  gamma delta", "continuation hangs under the text");
    }

    #[test]
    fn ordered_and_task_lists_mark_items() {
        let (hl, p) = setup();
        let t = texts(&render_lines("1. one\n2. two", 80, &hl, &p));
        assert_eq!(t, vec!["1. one", "2. two"]);
        let t = texts(&render_lines("- [x] done\n- [ ] open", 80, &hl, &p));
        assert_eq!(t, vec!["☑ done", "☐ open"]);
    }

    #[test]
    fn block_quotes_carry_bars_per_level() {
        let (hl, p) = setup();
        let t = texts(&render_lines("> outer\n>> inner", 80, &hl, &p));
        assert!(t.iter().any(|l| l.starts_with("▎ outer")), "{t:?}");
        assert!(t.iter().any(|l| l.starts_with("▎▎ inner")), "{t:?}");
    }

    #[test]
    fn nesting_caps_at_eight_bars() {
        let (hl, p) = setup();
        let deep = "> ".repeat(12) + "core";
        let t = texts(&render_lines(&deep, 80, &hl, &p));
        let bars = t.last().unwrap().chars().take_while(|c| *c == '▎').count();
        assert_eq!(bars, 8);
    }

    #[test]
    fn narrow_table_aligns_and_bolds_the_header() {
        let (hl, p) = setup();
        let md = "| a | long |\n|---|---|\n| x1 | y |";
        let t = texts(&render_lines(md, 40, &hl, &p));
        assert_eq!(t[0], "a  │ long");
        assert_eq!(t[2], "x1 │ y   ");
        assert!(t[1].starts_with('─'), "a dim rule under the header: {t:?}");
    }

    #[test]
    fn wide_table_falls_back_to_source_text() {
        let (hl, p) = setup();
        let md = "| alpha | beta |\n|---|---|\n| 123456789 | 987654321 |";
        let t = texts(&render_lines(md, 12, &hl, &p));
        assert!(t[0].starts_with("| alpha"), "source text shows, wrapped: {t:?}");
        let joined = t.concat();
        assert!(joined.contains("123456789") && joined.contains("987654321"), "{t:?}");
    }

    #[test]
    fn an_over_wide_table_shrinks_its_widest_column_and_wraps() {
        let (hl, p) = setup();
        // Natural widths: id 2, note 26; at pane 24 the note column shrinks to 19 and
        // wraps, the id column keeps its natural width, one dim rule under the header.
        let md = "| id | note |\n|---|---|\n| a | wraps beyond the pane edge |\n| b | short |";
        let t = texts(&render_lines(md, 24, &hl, &p));
        assert_eq!(t[0].trim_end(), "id │ note");
        assert!(t[1].chars().all(|c| c == '─'), "one dim rule under the header: {t:?}");
        assert_eq!(t[2].trim_end(), "a  │ wraps beyond the");
        assert_eq!(t[3].trim_end(), "   │ pane edge", "the wrap continues the separator");
        assert_eq!(t[4].trim_end(), "b  │ short", "the next body row follows directly");
        assert_eq!(t.len(), 5, "no separator line between body rows: {t:?}");
        assert!(t.iter().all(|l| l.chars().count() <= 24), "grid fits the pane: {t:?}");
    }

    #[test]
    fn a_word_wider_than_its_shrunk_column_hard_breaks_inside_it() {
        let (hl, p) = setup();
        let md = "| k | url |\n|---|---|\n| x | https://example.com/very/long/path |";
        let t = texts(&render_lines(md, 16, &hl, &p));
        let rows: Vec<&String> = t.iter().filter(|l| l.contains('│')).collect();
        assert!(rows.len() > 2, "the URL hard-breaks across several rows: {t:?}");
        for line in &rows {
            assert_eq!(line.chars().position(|c| c == '│'), Some(2), "aligned: {line:?}");
            assert!(line.chars().count() <= 16, "every row stays in the pane: {line:?}");
        }
        let joined: String = t.iter().filter_map(|l| l.split('│').nth(1)).map(str::trim).collect();
        assert!(joined.contains("https://example.com/very/long/path"), "{t:?}");
    }

    #[test]
    fn a_fitting_cell_preserves_verbatim_spacing() {
        let (hl, p) = setup();
        // A code span keeps its interior run of spaces when the cell never wraps.
        let md = "| k |\n|---|\n| `a    b` |";
        let t = texts(&render_lines(md, 40, &hl, &p));
        assert!(t.iter().any(|l| l.contains("a    b")), "code spacing survives: {t:?}");
    }

    #[test]
    fn a_table_over_wide_at_every_floor_falls_back_to_source() {
        let (hl, p) = setup();
        // Three columns floor at 8 cells each: 30 total, over a 20-cell pane.
        let md = "| aaaaaaaaaa | bbbbbbbbbb | cccccccccc |\n|---|---|---|\n| 1 | 2 | 3 |";
        let t = texts(&render_lines(md, 20, &hl, &p));
        assert!(t[0].starts_with("| aaaaaaaaaa"), "source text shows: {t:?}");
        let joined = t.concat();
        for cell in ["bbbbbbbbbb", "cccccccccc", "| 1 |", "2", "3"] {
            assert!(joined.contains(cell), "the whole source survives: {t:?}");
        }
    }

    #[test]
    fn tied_widest_columns_shrink_together() {
        let (hl, p) = setup();
        // Two 12-cell columns at a 21-cell pane split the deficit evenly: 9 cells each.
        let md = "| aaaaaaaaaaaa | bbbbbbbbbbbb |\n|---|---|\n| x | y |";
        let t = texts(&render_lines(md, 21, &hl, &p));
        assert_eq!(t[0], "aaaaaaaaa │ bbbbbbbbb");
        for line in t.iter().filter(|l| l.contains('│')) {
            assert_eq!(line.chars().position(|c| c == '│'), Some(10), "even split: {line:?}");
        }
    }

    #[test]
    fn a_column_shrinks_to_its_floor_and_still_renders() {
        let (hl, p) = setup();
        // Natural widths: elem 4, prose 24; a 15-cell pane leaves the prose column
        // exactly its 8-cell floor, and the table still renders as a grid.
        let md = "| elem | wraps at the floor width |\n|---|---|\n| a | b |";
        let t = texts(&render_lines(md, 15, &hl, &p));
        assert!(!t[0].starts_with('|'), "a grid, not source fallback: {t:?}");
        assert_eq!(t[0].trim_end(), "elem │ wraps at", "the prose column sits at 8 cells");
        for line in t.iter().filter(|l| l.contains('│')) {
            assert_eq!(line.chars().position(|c| c == '│'), Some(5), "aligned: {line:?}");
            assert!(line.chars().count() <= 15, "every row fits the pane: {line:?}");
        }
    }

    #[test]
    fn a_link_in_a_wrapped_cell_is_clickable_on_every_row() {
        let (hl, p) = setup();
        let md = "| a |\n|---|\n| [alpha beta gamma delta](https://x.dev/l) |";
        let r = render(md, 14, &hl, &p);
        let with_links: Vec<usize> =
            (0..r.meta.len()).filter(|&i| !r.meta[i].links.is_empty()).collect();
        assert!(
            with_links.len() >= 2,
            "the cell wraps, every row clickable: {:?}",
            texts(&r.lines)
        );
        for i in with_links {
            let line = text_of(&r.lines[i]);
            for l in &r.meta[i].links {
                assert_eq!(&*l.url, "https://x.dev/l");
                assert!(!line[l.start..l.end].trim().is_empty(), "{line:?} at {l:?}");
            }
        }
    }

    #[test]
    fn control_and_bidi_characters_render_as_placeholders() {
        let (hl, p) = setup();
        let t = texts(&render_lines("a\u{1b}[31mb \u{202e}evil", 80, &hl, &p));
        assert_eq!(t[0], "a�[31mb �evil");
    }

    #[test]
    fn breaks_soft_is_space_hard_is_line() {
        let (hl, p) = setup();
        let t = texts(&render_lines("one\ntwo", 80, &hl, &p));
        assert_eq!(t, vec!["one two"]);
        let t = texts(&render_lines("one  \ntwo", 80, &hl, &p));
        assert_eq!(t, vec!["one", "two"]);
    }

    #[test]
    fn blocks_separate_with_one_blank_line() {
        let (hl, p) = setup();
        let t = texts(&render_lines("first\n\nsecond", 80, &hl, &p));
        assert_eq!(t, vec!["first", "", "second"]);
    }

    #[test]
    fn raw_html_shows_dim_source() {
        let (hl, p) = setup();
        let lines = render_lines("<details>\n<summary>hi</summary>\n</details>", 80, &hl, &p);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.contains("<details>")), "{t:?}");
        let span = lines[0].spans.iter().find(|s| s.content.contains("<details>")).unwrap();
        assert_eq!(span.style.fg, Some(p.overlay0));
    }

    #[test]
    fn thematic_break_is_a_dim_rule() {
        let (hl, p) = setup();
        let t = texts(&render_lines("a\n\n---\n\nb", 80, &hl, &p));
        assert!(t.iter().any(|l| l.starts_with('─')), "{t:?}");
    }

    #[test]
    fn every_prose_color_comes_from_the_palette() {
        // Prose only — no fenced code, whose colors come from the syntax theme instead.
        let (hl, p) = setup();
        let lines = render_lines(
            "# H\n> q *i*\n- [x] t `c` [l](u://d)\n\n| a |\n|---|\n| b |",
            60,
            &hl,
            &p,
        );
        let named = [
            p.text, p.subtext0, p.overlay0, p.overlay1, p.mauve, p.lavender, p.peach, p.red,
            p.green, p.yellow,
        ];
        for line in &lines {
            for span in &line.spans {
                if let Some(fg) = span.style.fg {
                    assert!(named.contains(&fg), "off-palette color {fg:?} in {:?}", span.content);
                }
            }
        }
    }

    #[test]
    fn empty_and_plain_inputs_render() {
        let (hl, p) = setup();
        assert!(render_lines("", 80, &hl, &p).is_empty());
        let t = texts(&render_lines("just plain prose", 80, &hl, &p));
        assert_eq!(t, vec!["just plain prose"]);
    }

    #[test]
    fn meta_maps_rendered_lines_to_their_source_blocks() {
        let (hl, p) = setup();
        // Source lines: 1 heading, 2 blank, 3 para, 4 blank, 5 fence, 6-7 code, 8 fence.
        let md = "# Title\n\nprose here\n\n```rust\nlet a = 1;\nlet b = 2;\n```\n";
        let r = render(md, 80, &hl, &p);
        let t = texts(&r.lines);
        let src =
            |needle: &str| r.meta[t.iter().position(|l| l.contains(needle)).unwrap()].source_line;
        assert_eq!(src("Title"), 1);
        assert_eq!(src("prose here"), 3);
        assert_eq!(src("let a = 1;"), 6, "fenced code maps line-accurately");
        assert_eq!(src("let b = 2;"), 7);
        let sorted = r.meta.iter().map(|m| m.source_line).collect::<Vec<_>>();
        let mut expect = sorted.clone();
        expect.sort_unstable();
        assert_eq!(sorted, expect, "source lines are non-decreasing, so lookups can bisect");
    }

    #[test]
    fn meta_carries_link_spans_including_the_dim_destination() {
        let (hl, p) = setup();
        let r = render("see [the run](https://ci.example/1) now", 80, &hl, &p);
        let line = texts(&r.lines).remove(0);
        let spans: &[LinkSpan] = &r.meta[0].links;
        assert_eq!(spans.len(), 1, "text and destination fuse into one click target");
        let s = &spans[0];
        assert_eq!(&*s.url, "https://ci.example/1");
        assert_eq!(&line[s.start..s.end], "the run (https://ci.example/1)");
    }

    #[test]
    fn a_wrapped_link_is_clickable_on_every_row() {
        let (hl, p) = setup();
        let r = render("[alpha beta gamma](https://x.dev/l)", 12, &hl, &p);
        let with_links: Vec<usize> =
            (0..r.meta.len()).filter(|&i| !r.meta[i].links.is_empty()).collect();
        assert!(with_links.len() >= 2, "the link wraps and stays clickable: {:?}", texts(&r.lines));
        for i in with_links {
            assert!(r.meta[i].links.iter().all(|l| &*l.url == "https://x.dev/l"));
        }
    }

    #[test]
    fn table_cells_carry_their_link_spans() {
        let (hl, p) = setup();
        let md = "| a |\n|---|\n| [x](https://t.co/x) |";
        let r = render(md, 40, &hl, &p);
        let i = (0..r.meta.len()).find(|&i| !r.meta[i].links.is_empty()).expect("a cell link");
        let line = texts(&r.lines).remove(i);
        let s = &r.meta[i].links[0];
        assert!(&line[s.start..s.end].contains('x'), "{line:?} at {s:?}");
        assert_eq!(&*s.url, "https://t.co/x");
    }

    #[test]
    fn headings_carry_github_slugs_with_duplicates_numbered() {
        let (hl, p) = setup();
        let r = render("# My Título!\n\n## Dup\n\ntext\n\n## Dup\n", 80, &hl, &p);
        let slugs: Vec<&str> = r.anchors.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(slugs, vec!["my-título", "dup", "dup-1"]);
        // The anchor points at the heading's line, not at a separator blank.
        let t = texts(&r.lines);
        let i = t.iter().position(|l| l.contains("My Título")).unwrap();
        assert_eq!(r.anchors[0].1, i);
    }

    #[test]
    fn a_heading_slug_never_includes_a_links_destination() {
        let (hl, p) = setup();
        let r = render("## See [docs](https://x.dev)\n", 80, &hl, &p);
        assert_eq!(r.anchors[0].0, "see-docs", "the dim destination stays out of the slug");
    }

    #[test]
    fn inline_html_stays_inline_and_out_of_the_slug() {
        let (hl, p) = setup();
        // The ubiquitous explicit-anchor pattern: one heading line, GitHub's slug.
        let r = render("## <a name=\"install\"></a>Install\n", 80, &hl, &p);
        let t = texts(&r.lines);
        assert_eq!(t.len(), 1, "the heading stays one line: {t:?}");
        assert!(t[0].contains("Install"));
        assert_eq!(r.anchors[0].0, "install", "tag text stays out of the slug");

        let r = render("## Hello <b>World</b>\n", 80, &hl, &p);
        assert_eq!(r.anchors[0].0, "hello-world");
    }

    #[test]
    fn image_alt_stays_out_of_the_slug() {
        let (hl, p) = setup();
        let r = render("## ![Diagram](x.png) Overview\n", 80, &hl, &p);
        assert_eq!(r.anchors[0].0, "overview", "alt text stays out, as on GitHub");
    }

    #[test]
    fn a_clicked_fragment_normalizes_like_a_slug() {
        assert_eq!(super::slug_text("Set-Up!"), "set-up");
        assert_eq!(super::slug_text("İstanbul"), "istanbul");
        assert_eq!(super::slug_text("  My Título!  "), "my-título");
    }

    #[test]
    fn a_list_item_leading_with_a_code_block_keeps_its_marker() {
        let (hl, p) = setup();
        let t = texts(&render_lines("-      code", 80, &hl, &p));
        assert!(t[0].starts_with("• "), "the bullet lands on the code line: {t:?}");
    }

    #[test]
    fn an_html_block_never_jumps_ahead_of_a_tight_items_text() {
        let (hl, p) = setup();
        let t = texts(&render_lines("- foo\n  <div>bar</div>", 80, &hl, &p));
        let foo = t.iter().position(|l| l.contains("foo")).unwrap();
        let div = t.iter().position(|l| l.contains("<div>")).unwrap();
        assert!(foo < div, "the item's text keeps its order: {t:?}");
        assert!(t[foo].starts_with("• "), "and its bullet: {t:?}");
    }

    #[test]
    fn a_list_item_leading_with_an_aligned_table_keeps_its_marker() {
        let (hl, p) = setup();
        let md = "- | a | b |\n  |---|---|\n  | x | y |\n\n  after table";
        let t = texts(&render_lines(md, 40, &hl, &p));
        assert!(t[0].starts_with("• "), "the bullet lands on the table's first row: {t:?}");
        let after = t.iter().find(|l| l.contains("after table")).unwrap();
        assert!(!after.contains('•'), "and never leaks onto a later block: {t:?}");
    }

    #[test]
    fn an_empty_heading_anchors_nothing() {
        let (hl, p) = setup();
        let r = render("#\n\nhi\n", 80, &hl, &p);
        assert!(r.anchors.is_empty(), "no slug leaks onto the next block: {:?}", r.anchors);
    }

    #[test]
    fn cache_reuses_by_text_and_width() {
        let (hl, p) = setup();
        let mut cache = RenderCache::default();
        let first = cache.get("**hi**", 80, &hl, &p);
        let repeat = cache.get("**hi**", 80, &hl, &p);
        assert_eq!(first.lines, repeat.lines);
        // The key invalidates on changed text — a stale hit would return "hi" here.
        let changed = cache.get("**bye**", 80, &hl, &p);
        assert_ne!(texts(&changed.lines), texts(&first.lines), "changed text re-renders");
        let narrow = cache.get("**hi**", 10, &hl, &p);
        assert_eq!(
            texts(&narrow.lines),
            texts(&first.lines),
            "same short content at a narrower budget"
        );
        cache.clear();
        assert_eq!(cache.get("**hi**", 80, &hl, &p).lines, first.lines);
    }

    #[test]
    fn external_output_keeps_sgr_and_strips_terminal_controls() {
        use ratatui::style::Color;

        let rendered = super::external_output(concat!(
            "\u{1b}[34mblue\u{1b}[0m\u{1b}[2J",
            "\u{1b}]8;;https://evil.example\u{7}link\u{1b}]8;;\u{7}",
            "\u{1b}Psecret\u{7}still-secret\u{1b}\\\u{009d}also-secret\u{009c}",
            "\u{009b}2J\u{009b}31mred\u{009b}0m\n",
        ))
        .unwrap();
        assert_eq!(texts(&rendered.lines), ["bluelinkred"]);
        assert_eq!(rendered.lines[0].spans[0].style.fg, Some(Color::Blue));
        assert_eq!(rendered.lines[0].spans.last().unwrap().style.fg, Some(Color::Red));
        assert!(rendered.meta.iter().all(|meta| meta.links.is_empty()));
        assert!(rendered.anchors.is_empty());
    }

    #[test]
    fn external_command_substitutes_width_and_light_style_in_argv() {
        let command = ["printf".to_string(), "style={style},width={width}".to_string()];
        let rendered = super::render_external(
            &command,
            "untrusted input",
            37,
            crate::theme::Appearance::Light,
            Duration::from_secs(1),
        )
        .expect("printf executes directly");
        assert_eq!(texts(&rendered.lines), ["style=light,width=37"]);
    }

    #[test]
    fn external_command_timeout_is_a_fallback_failure() {
        let command = ["sh".to_string(), "-c".to_string(), "sleep 2 & wait".to_string()];
        let started = Instant::now();
        assert!(
            super::render_external(
                &command,
                "untrusted input",
                80,
                crate::theme::Appearance::Dark,
                Duration::from_millis(20),
            )
            .is_none(),
            "a timed-out command returns no external render, selecting the built-in fallback"
        );
        assert!(started.elapsed() < Duration::from_millis(500), "descendants must die with parent");
    }

    fn assert_process_gone(pid: i32) {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        for _ in 0..50 {
            if kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("renderer descendant {pid} survived process-group cleanup");
    }

    #[test]
    fn successful_renderer_cleans_up_a_detached_stdio_descendant() {
        let command = [
            "sh".to_string(),
            "-c".to_string(),
            "sleep 2 >/dev/null 2>&1 </dev/null & printf $!".to_string(),
        ];
        let rendered = super::render_external(
            &command,
            "input",
            80,
            crate::theme::Appearance::Dark,
            Duration::from_secs(1),
        )
        .unwrap();
        let pid = texts(&rendered.lines)[0].parse().unwrap();
        assert_process_gone(pid);
    }

    #[test]
    fn failed_renderer_cleans_up_a_detached_stdio_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let command = [
            "sh".to_string(),
            "-c".to_string(),
            "sleep 2 >/dev/null 2>&1 </dev/null & echo $! > \"$1\"; exit 7".to_string(),
            "sh".to_string(),
            pid_file.to_string_lossy().into_owned(),
        ];
        assert!(
            super::render_external(
                &command,
                "input",
                80,
                crate::theme::Appearance::Dark,
                Duration::from_secs(1),
            )
            .is_none()
        );
        let pid: i32 = std::fs::read_to_string(pid_file).unwrap().trim().parse().unwrap();
        assert_process_gone(pid);
    }

    #[test]
    fn external_command_must_accept_the_complete_stdin() {
        let command = ["true".to_string()];
        assert!(
            super::render_external(
                &command,
                &"x".repeat(1024 * 1024),
                80,
                crate::theme::Appearance::Dark,
                Duration::from_secs(1),
            )
            .is_none(),
            "an early stdin close selects the built-in fallback"
        );
    }

    #[test]
    fn excessive_external_style_runs_are_rejected_before_ansi_conversion() {
        let raw = "\u{1b}[31m".repeat(super::MAX_EXTERNAL_STYLE_RUNS + 1);
        assert!(super::external_output(&raw).is_none());
    }
}
