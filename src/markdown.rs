//! The one markdown renderer: `pulldown-cmark` in, themed pre-wrapped lines out.

use std::collections::HashSet;
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::highlight::Highlighter;
use crate::roles::Palette;
use crate::roles::{Fill, Ink};

/// The deepest block indent, so nesting never squeezes the content to nothing.
const MAX_NEST: usize = 8;

/// The indent code-block lines carry inside their block.
const CODE_INDENT: &str = "  ";

/// The narrowest a table column shrinks to, unless naturally narrower.
const COL_FLOOR: usize = 8;

/// Rendered markdown: styled lines, their metadata in lockstep, and heading anchors.
#[derive(Clone, Debug, Default)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub meta: Vec<LineMeta>,
    /// Each heading's GitHub slug and the rendered line it starts on.
    pub anchors: Vec<(String, usize)>,
    /// Source lines whose content renders nothing, wholly or in part; a collapsed `<details>` body excluded.
    pub silent: Vec<usize>,
    /// Every `<details>` with a summary, open or not, in close order.
    pub disclosures: Vec<Disclosure>,
    /// Every code block's source range, fences included, which review treats as one unit.
    pub code_blocks: Vec<(usize, usize)>,
}

/// One `<details>` element's source lines: open tag, first body line, close tag or file end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disclosure {
    pub key: String,
    pub start: usize,
    pub body: usize,
    pub end: usize,
}

/// One rendered line's source range and links; ranges never overlap.
#[derive(Clone, Debug)]
pub struct LineMeta {
    pub source_line: usize,
    pub source_end: usize,
    pub links: Vec<LinkSpan>,
    /// A `<details>` summary on this line, when the line is that disclosure.
    pub details: Option<DetailsHit>,
    /// Whether the line is the blank gap set above its block, not its content.
    pub gap: bool,
    /// The source lines this line's own text comes from; a gap's is its block's first.
    pub lines: (usize, usize),
}

/// A `<details>` summary's click target, keyed by summary and repeat count (`Details#1`).
#[derive(Clone, Debug)]
pub struct DetailsHit {
    pub start: usize,
    pub end: usize,
    pub key: std::sync::Arc<str>,
}

/// A clickable span: display columns and a shared destination, cheap to clone.
#[derive(Clone, Debug)]
pub struct LinkSpan {
    pub start: usize,
    pub end: usize,
    pub url: std::sync::Arc<str>,
}

/// Render `text` as styled lines wrapped to `width` columns. All details start collapsed.
pub fn render(text: &str, width: usize, hl: &Highlighter, p: &Palette) -> Rendered {
    render_expanded(text, width, hl, p, &HashSet::new())
}

/// Like [`render`], with the `<details>` whose [`DetailsHit::key`] is in `expanded` opened.
pub fn render_expanded<S: std::hash::BuildHasher>(
    text: &str,
    width: usize,
    hl: &Highlighter,
    p: &Palette,
    expanded: &HashSet<String, S>,
) -> Rendered {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut line_starts = vec![0usize];
    line_starts.extend(text.char_indices().filter(|(_, c)| *c == '\n').map(|(i, _)| i + 1));
    let expanded: HashSet<String> = expanded.iter().cloned().collect();
    let mut r = Renderer {
        hl,
        p,
        width: width.max(1),
        source: text,
        line_starts,
        block_src: 1,
        block_end: 1,
        covered: Vec::new(),
        row_lines: None,
        open_blocks: Vec::new(),
        comment_lines: Vec::new(),
        in_comment: false,
        summary_counts: std::collections::HashMap::new(),
        event_lines: (1, 1),
        inline_span: None,
        marker_line: None,
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
        pending_details: None,
        slug_counts: std::collections::HashMap::new(),
        expanded: &expanded,
        details: Vec::new(),
        skip_relative: false,
    };
    for (event, range) in Parser::new_ext(text, opts).into_offset_iter() {
        r.event(event, range);
    }
    r.flush_block(false);
    r.finish(text)
}

/// The last few renders by text, width, and open details; cleared on a theme switch.
#[derive(Debug, Default)]
pub struct RenderCache {
    slots: Vec<CacheSlot>,
}

#[derive(Debug)]
struct CacheSlot {
    text: String,
    width: usize,
    expanded: Vec<String>,
    rendered: Rendered,
}

const CACHE_SLOTS: usize = 16;

impl RenderCache {
    pub fn get(&mut self, text: &str, width: usize, hl: &Highlighter, p: &Palette) -> Rendered {
        self.get_expanded(text, width, hl, p, &HashSet::new())
    }

    pub fn get_expanded<S: std::hash::BuildHasher>(
        &mut self,
        text: &str,
        width: usize,
        hl: &Highlighter,
        p: &Palette,
        expanded: &HashSet<String, S>,
    ) -> Rendered {
        let mut expanded_key: Vec<String> = expanded.iter().cloned().collect();
        expanded_key.sort();
        if let Some(i) = self
            .slots
            .iter()
            .position(|s| s.text == text && s.width == width && s.expanded == expanded_key)
        {
            let slot = self.slots.remove(i);
            let rendered = slot.rendered.clone();
            self.slots.push(slot);
            return rendered;
        }
        let rendered = render_expanded(text, width, hl, p, expanded);
        self.slots.push(CacheSlot {
            text: text.to_string(),
            width,
            expanded: expanded_key,
            rendered: rendered.clone(),
        });
        if self.slots.len() > CACHE_SLOTS {
            self.slots.remove(0);
        }
        rendered
    }

    pub fn clear(&mut self) {
        self.slots.clear();
    }
}

/// One styled run of unwrapped inline text; `"\n"` forces a break.
#[derive(Clone, Debug)]
struct Chunk {
    text: String,
    style: Style,
    link: Option<usize>,
    /// The source line the text came from.
    line: usize,
}

/// An in-progress code block.
struct CodeBlock {
    lang: Option<String>,
    content: String,
    fenced: bool,
}

/// An in-progress table; `range` is the source the wide-table fallback shows.
struct Table {
    range: Range<usize>,
    rows: Vec<Vec<Vec<Chunk>>>,
    row_lines: Vec<(usize, usize)>,
    head_rows: usize,
}

struct Renderer<'a> {
    hl: &'a Highlighter,
    p: &'a Palette,
    width: usize,
    source: &'a str,
    /// Byte offset of each 1-based source line's start, for offset → line mapping.
    line_starts: Vec<usize>,
    /// The 1-based source lines the block being emitted spans.
    block_src: usize,
    block_end: usize,
    /// The ranges of blocks that rendered a line, so their fences and markers count as shown.
    covered: Vec<(usize, usize)>,
    /// The source lines of the lines being pushed, while narrower than their block's.
    row_lines: Option<(usize, usize)>,
    /// The open blocks: closing tag, source lines, and line count at the start.
    open_blocks: Vec<(TagEnd, usize, usize, usize)>,
    /// Lines holding HTML comment text: hidden even inside a block that renders.
    comment_lines: Vec<(usize, usize)>,
    /// An HTML comment opened on an earlier line of the same HTML block, not yet closed.
    in_comment: bool,
    /// Disclosures seen so far per summary, for their keys' occurrence count.
    summary_counts: std::collections::HashMap<String, usize>,
    /// The source lines of the event being handled.
    event_lines: (usize, usize),
    /// The source lines that fed the pending inline run.
    inline_span: Option<(usize, usize)>,
    /// The pending item marker's line, shown by the run that takes the marker.
    marker_line: Option<usize>,
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
    /// The heading's prose for its slug, without link destinations, as GitHub slugs.
    heading_text: Option<String>,
    /// Open images: alt-text start, heading length to roll back to, destination.
    images: Vec<(usize, usize, String)>,
    code: Option<CodeBlock>,
    table: Option<Table>,
    needs_blank: bool,
    /// The slug the next flushed line carries as its heading anchor.
    pending_anchor: Option<String>,
    /// A details summary the next flushed line carries as its click target.
    pending_details: Option<DetailsHit>,
    /// Slugs seen so far, so duplicate headings number like GitHub's.
    slug_counts: std::collections::HashMap<String, usize>,
    expanded: &'a HashSet<String>,
    details: Vec<DetailsFrame>,
    skip_relative: bool,
}

/// Whether a block's whole source range shows once it renders a line.
fn shows_whole(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::Item
            | Tag::CodeBlock(_)
            | Tag::Table(_)
            | Tag::HtmlBlock
    )
}

/// An open `<details>`: its summary (while collecting), and whether its body is silent.
struct DetailsFrame {
    summary: String,
    collecting_summary: bool,
    skip: bool,
    /// The opening tag's line, and the key and body line once the summary closes.
    start: usize,
    key: Option<(String, usize)>,
}

impl Renderer<'_> {
    fn emitting(&self) -> bool {
        !self.details.iter().any(|d| d.skip) && !self.skip_relative
    }

    fn collecting_summary(&self) -> bool {
        self.details.last().is_some_and(|d| d.collecting_summary)
    }

    fn append_summary(&mut self, text: &str) {
        if let Some(d) = self.details.last_mut() {
            d.summary.push_str(text);
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        self.event_lines = (self.src_line(range.start), self.end_line(&range));
        // A comment never outlives its HTML block: pulldown ends the block by then.
        if matches!(event, Event::Start(Tag::HtmlBlock) | Event::End(TagEnd::HtmlBlock)) {
            self.in_comment = false;
        }
        // A tight item's pending text flushes first, never counting as the next block's line.
        if let Event::Start(tag) = &event
            && shows_whole(tag)
        {
            if matches!(tag, Tag::HtmlBlock) && self.emitting() && !self.collecting_summary() {
                self.flush_block(true);
            }
            let (start, end) = self.event_lines;
            self.open_blocks.push((tag.to_end(), start, end, self.out.lines.len()));
        }
        let closes = match &event {
            Event::End(end) => self.open_blocks.last().is_some_and(|b| b.0 == *end),
            _ => false,
        };
        self.dispatch(event, range);
        if closes
            && let Some((_, start, end, before)) = self.open_blocks.pop()
            && self.out.lines.len() > before
        {
            self.covered.push((start, end));
        }
    }

    fn dispatch(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::InlineHtml(t) => {
                self.handle_html(&t, true, &range);
            }
            Event::Html(t) => {
                if self.emitting() && !self.collecting_summary() {
                    // A tight item's pending text emits first, so HTML never jumps ahead.
                    self.flush_block(true);
                }
                self.block_src = self.src_line(range.start);
                self.block_end = self.end_line(&range);
                self.handle_html(&t, false, &range);
            }
            Event::Start(tag) if self.emitting() && !self.collecting_summary() => {
                self.start(tag, range);
            }
            Event::End(tag) if self.emitting() && !self.collecting_summary() => self.end(tag),
            Event::Text(t) => {
                if self.collecting_summary() {
                    self.append_summary(&t);
                } else if !self.emitting() {
                } else if let Some(code) = &mut self.code {
                    code.content.push_str(&t);
                } else {
                    self.push_text(&t, self.current_style());
                }
            }
            Event::Code(t) if self.emitting() && !self.collecting_summary() => {
                // Inline code sits on a chip, so it reads as code in every theme, in the
                // syntax theme's code color where it sets one.
                let fg = self.hl.markdown().inline_code.map_or_else(
                    || self.p.ink(Ink::Text, Fill::Code),
                    |c| self.readable(c, Fill::Code),
                );
                let style = self.current_style().fg(fg).bg(self.p.fill(Fill::Code));
                self.push_code_span(&t, &range, style);
            }
            Event::SoftBreak if self.collecting_summary() => self.append_summary(" "),
            Event::SoftBreak if self.emitting() => self.push_text(" ", self.current_style()),
            Event::HardBreak if self.emitting() && !self.collecting_summary() => {
                let style = self.current_style();
                let link = self.current_link();
                self.push_chunk("\n".into(), style, link);
            }
            Event::Rule if self.emitting() => {
                self.flush_block(true);
                self.block_src = self.src_line(range.start);
                self.block_end = self.block_src;
                self.blank_before_block();
                let budget = self.budget(self.prefix(None).0.width());
                let line = Line::from(vec![
                    self.prefix(None).0,
                    Span::styled(
                        "─".repeat(budget),
                        Style::default().fg(self.p.mark(Ink::Border, Fill::Base)),
                    ),
                ]);
                self.push_plain_line(line);
                self.needs_blank = true;
            }
            Event::TaskListMarker(done) if self.emitting() => {
                self.marker = Some(if done { "☑ ".into() } else { "☐ ".into() });
            }
            // Footnotes, math, and events inside a collapsed details body are silent.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        // A block-level tag stamps the source line its rendered lines map to.
        if matches!(
            tag,
            Tag::Paragraph | Tag::Heading { .. } | Tag::Item | Tag::CodeBlock(_) | Tag::Table(_)
        ) {
            self.block_src = self.src_line(range.start);
            self.block_end = self.end_line(&range);
            if matches!(tag, Tag::CodeBlock(_)) && self.emitting() {
                self.out.code_blocks.push((self.block_src, self.block_end));
            }
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
                self.marker_line = Some(self.event_lines.0);
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
                let accent = self.p.ink(Ink::Accent, Fill::Base);
                self.push_style(|s| s.fg(accent).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { dest_url, .. } => {
                let at = self.chunk_len();
                let heading_len = self.heading_text.as_ref().map_or(0, String::len);
                self.images.push((at, heading_len, dest_url.as_ref().to_string()));
            }
            Tag::Table(_) => {
                self.flush_block(true);
                self.table =
                    Some(Table { range, rows: Vec::new(), row_lines: Vec::new(), head_rows: 0 });
            }
            Tag::TableHead | Tag::TableRow => {
                let lines = (self.src_line(range.start), self.end_line(&range));
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                    t.row_lines.push(lines);
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
            TagEnd::Heading(_) => self.end_heading(),
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
        if self.table.is_none() {
            let (start, end) = self.event_lines;
            self.inline_span = Some(match self.inline_span {
                Some((s, e)) => (s.min(start), e.max(end)),
                None => (start, end),
            });
        }
        // A run reads forward: a link's appended url never maps before the text ahead of it.
        let after = self.chunks_mut().last().map_or(0, |c| c.line);
        let source = self.event_lines.0.max(after);
        self.chunks_mut().push(Chunk { text, style, link, line: source });
    }

    /// Push a code span, each piece on its own source line, by aligning its text to the raw source.
    fn push_code_span(&mut self, text: &str, range: &Range<usize>, style: Style) {
        let raw = self.source.get(range.clone()).unwrap_or_default();
        let inner = raw.trim_matches('`');
        if !inner.contains('\n') {
            self.push_text(text, style);
            return;
        }
        let first = self.event_lines.0;
        let mut line = first;
        let mut piece = String::new();
        let mut raw_chars = inner.chars();
        for ch in text.chars() {
            for r in raw_chars.by_ref() {
                if r == '\n' {
                    if !piece.is_empty() {
                        self.event_lines.0 = line;
                        self.push_text(&std::mem::take(&mut piece), style);
                    }
                    line += 1;
                    if ch == ' ' {
                        break;
                    }
                } else if r == ch || (r == '\r' && ch == ' ') {
                    // CommonMark turns a span's line ending into a space, `\r` included.
                    break;
                }
            }
            piece.push(ch);
        }
        if !piece.is_empty() {
            self.event_lines.0 = line;
            self.push_text(&piece, style);
        }
        self.event_lines.0 = first;
    }

    /// The innermost open link's url index, stamped onto every chunk inside it.
    fn current_link(&self) -> Option<usize> {
        self.links.last().map(|(id, _)| *id)
    }

    fn push_style(&mut self, patch: impl FnOnce(Style) -> Style) {
        self.styles.push(patch(self.current_style()));
    }

    fn current_style(&self) -> Style {
        self.styles
            .last()
            .copied()
            .unwrap_or_else(|| Style::default().fg(self.p.ink(Ink::Text, Fill::Base)))
    }

    /// Heading style: bold in the theme's color for its level, else body text (deeper levels
    /// secondary); never the accent, since a heading is never clicked.
    fn heading_style(&self, level: HeadingLevel) -> Style {
        let fallback = match level {
            HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => Ink::Text,
            _ => Ink::TextSecondary,
        };
        let fg = self.hl.markdown().headings[level as usize - 1]
            .map_or_else(|| self.p.ink(fallback, Fill::Base), |c| self.readable(c, Fill::Base));
        Style::default().fg(fg).add_modifier(Modifier::BOLD)
    }

    /// A syntax theme's color, read as text on `on`.
    fn readable(&self, (r, g, b): crate::diff::Rgb, on: Fill) -> Color {
        self.p.readable(Color::Rgb(r, g, b), on)
    }

    /// Close a link, appending its destination dim when the text differs.
    fn end_link(&mut self) {
        self.styles.pop();
        let Some((id, start)) = self.links.pop() else {
            return;
        };
        let dest = self.urls[id].clone();
        let text: String = self.chunks_mut()[start..].iter().map(|c| c.text.as_str()).collect();
        if !dest.is_empty() && text != *dest {
            let style = Style::default().fg(self.p.ink(Ink::TextMuted, Fill::Base));
            // The dim destination shares the click target with the text.
            self.push_chunk(format!(" ({})", sanitize(&dest)), style, Some(id));
        }
    }

    /// Collapse an image to a severity chip or a dim `⧉ alt-text` link to its url.
    fn end_image(&mut self) {
        let Some((start, heading_len, dest)) = self.images.pop() else {
            return;
        };
        if let Some(h) = &mut self.heading_text {
            h.truncate(heading_len);
        }
        let chunks = self.chunks_mut();
        let alt: String = chunks[start..].iter().map(|c| c.text.as_str()).collect();
        chunks.truncate(start);
        self.emit_image(alt.trim(), &dest);
    }

    fn emit_image(&mut self, alt: &str, dest: &str) {
        if let Some(label) = badge_label(alt) {
            let fg = match label {
                "P1" => self.p.ink(Ink::Danger, Fill::Base),
                "P2" => self.p.ink(Ink::Warning, Fill::Base),
                _ => self.p.ink(Ink::TextSecondary, Fill::Base),
            };
            let style = Style::default().fg(fg).add_modifier(Modifier::BOLD);
            self.push_chunk(label.to_string(), style, self.current_link());
            return;
        }
        let alt = sanitize(alt);
        let dest = sanitize(dest);
        let text = if alt.is_empty() { "⧉ image".to_string() } else { format!("⧉ {alt}") };
        let style = Style::default().fg(self.p.ink(Ink::TextMuted, Fill::Base));
        let link = if dest.is_empty() {
            self.current_link()
        } else {
            self.urls.push(std::sync::Arc::from(dest));
            Some(self.urls.len() - 1)
        };
        self.push_chunk(text, style, link);
    }

    /// Handle one HTML event. Returns whether it holds comment text, which never renders.
    fn handle_html(&mut self, html: &str, inline: bool, range: &Range<usize>) -> bool {
        let lines = (self.src_line(range.start), self.end_line(range));
        let mut html = html;
        let mut commented = false;
        // A comment an earlier HTML line opened swallows everything up to its close.
        if self.in_comment {
            commented = true;
            match html.find("-->") {
                Some(at) => {
                    html = &html[at + 3..];
                    self.in_comment = false;
                }
                None => html = "",
            }
        }
        if let Some(open) = html.rfind("<!--")
            && !html[open..].contains("-->")
        {
            self.in_comment = true;
        }
        let produced = self.inline.len();
        for tok in html_tokens(html) {
            match tok {
                HtmlTok::Comment => commented = true,
                HtmlTok::Text(t) => {
                    if self.collecting_summary() {
                        self.append_summary(t);
                    } else if self.emitting() && !t.trim().is_empty() {
                        self.push_text(t, self.current_style());
                    }
                }
                HtmlTok::Tag { name, closing, self_closing, raw } => {
                    self.handle_html_tag(name, closing, self_closing, raw);
                }
            }
        }
        if commented {
            self.comment_lines.push(lines);
        }
        if !inline && self.inline.len() > produced && self.emitting() && !self.collecting_summary()
        {
            self.flush_block(true);
        }
        commented
    }

    fn handle_html_tag(&mut self, name: &str, closing: bool, self_closing: bool, raw: &str) {
        let name = html_tag_name(name);
        match (name.as_str(), closing, self_closing) {
            ("details", false, false) => {
                if self.emitting() {
                    self.flush_block(true);
                }
                self.details.push(DetailsFrame {
                    summary: String::new(),
                    collecting_summary: false,
                    skip: true,
                    start: self.event_lines.0,
                    key: None,
                });
            }
            ("details", true, _) => {
                if self.emitting() {
                    self.flush_block(true);
                }
                if let Some(DetailsFrame { start, key: Some((key, body)), .. }) = self.details.pop()
                {
                    let end = self.event_lines.1.max(start);
                    self.out.disclosures.push(Disclosure { key, start, body, end });
                }
            }
            ("summary", false, _) => {
                if let Some(d) = self.details.last_mut() {
                    d.collecting_summary = true;
                    d.summary.clear();
                }
            }
            ("summary", true, _) => self.finish_summary(),
            ("br", _, _) if self.emitting() && !self.collecting_summary() => {
                let style = self.current_style();
                let link = self.current_link();
                self.push_chunk("\n".into(), style, link);
            }
            ("relative-time", false, false) => self.skip_relative = true,
            ("relative-time", true, _) | ("relative-time", false, true) => {
                self.skip_relative = false;
            }
            ("img", _, _) if self.emitting() && !self.collecting_summary() => {
                let alt = html_attr(raw, "alt").unwrap_or_default();
                let src = html_attr(raw, "src").unwrap_or_default();
                self.emit_image(&alt, &src);
            }
            (h @ ("h1" | "h2" | "h3" | "h4" | "h5" | "h6"), false, false)
                if self.emitting() && !self.collecting_summary() =>
            {
                self.flush_block(true);
                self.heading_text = Some(String::new());
                self.styles.push(self.heading_style(html_heading_level(h)));
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true, _) => self.end_heading(),
            _ => {}
        }
    }

    fn end_heading(&mut self) {
        let Some(text) = self.heading_text.take() else {
            return;
        };
        let slug = self.slugify(&text);
        if !slug.is_empty() {
            self.pending_anchor = Some(slug);
        }
        self.flush_block(true);
        self.styles.pop();
    }

    fn finish_summary(&mut self) {
        let Some(d) = self.details.last_mut() else {
            return;
        };
        d.collecting_summary = false;
        let summary = d.summary.split_whitespace().collect::<Vec<_>>().join(" ");
        let summary = sanitize(&summary);
        d.summary.clone_from(&summary);
        let seen = self.summary_counts.entry(summary.clone()).or_insert(0);
        let key = format!("{summary}#{seen}");
        *seen += 1;
        let open = self.expanded.contains(&key);
        let body = self.event_lines.1 + 1;
        if let Some(d) = self.details.last_mut() {
            d.skip = !open;
            d.key = Some((key.clone(), body));
        }
        if !self.details.iter().rev().skip(1).any(|d| d.skip) {
            self.emit_details_summary(&summary, &key, open);
        }
    }

    fn emit_details_summary(&mut self, summary: &str, key: &str, open: bool) {
        self.flush_block(true);
        self.blank_before_block();
        let glyph = if open { "▾ " } else { "▸ " };
        let (first, _) = self.prefix(None);
        let off = first.width();
        let glyph_style = Style::default().fg(self.p.ink(Ink::TextMuted, Fill::Base));
        let text_style = Style::default().fg(self.p.ink(Ink::Text, Fill::Base));
        let summary_w = summary.width();
        self.pending_details = Some(DetailsHit {
            start: off,
            end: off + glyph.width() + summary_w,
            key: std::sync::Arc::from(key),
        });
        self.push_line(
            Line::from(vec![
                first,
                Span::styled(glyph, glyph_style),
                Span::styled(summary.to_string(), text_style),
            ]),
            Vec::new(),
        );
        self.needs_blank = open;
    }

    // ---- block emission ------------------------------------------------------------------

    /// The `(first, continuation)` block prefixes, the marker on the first.
    fn prefix(&self, marker: Option<&str>) -> (Span<'static>, Span<'static>) {
        let bars = "▎".repeat(self.quote.min(MAX_NEST));
        let gap = if bars.is_empty() { "" } else { " " };
        let indent = "  ".repeat(self.lists.len().min(MAX_NEST).saturating_sub(1));
        let marker = marker.unwrap_or("");
        let first = format!("{bars}{gap}{indent}{marker}");
        let cont = format!("{bars}{gap}{indent}{}", " ".repeat(marker.width()));
        let style = Style::default().fg(self.p.ink(Ink::TextMuted, Fill::Base));
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

    /// The last source line of an event's end-exclusive byte range.
    fn end_line(&self, range: &Range<usize>) -> usize {
        self.src_line(range.end.saturating_sub(1).max(range.start))
    }

    /// Settle the source ranges, each ending where the next begins, and the silent lines.
    fn finish(mut self, text: &str) -> Rendered {
        // A disclosure left open runs to the end of the file.
        let last = text.lines().count().max(1);
        for d in std::mem::take(&mut self.details).into_iter().rev() {
            if let Some((key, body)) = d.key {
                let end = last.max(d.start);
                self.out.disclosures.push(Disclosure { key, start: d.start, body, end });
            }
        }
        // Blank once container markers go: a `>`-only line hides nothing.
        let blank: Vec<bool> = text
            .lines()
            .map(|l| l.trim_start_matches(|c: char| c == '>' || c.is_whitespace()).is_empty())
            .collect();
        let is_blank = |line: usize| blank.get(line.wrapping_sub(1)).copied().unwrap_or(true);
        let meta = &mut self.out.meta;
        let mut next_start = usize::MAX;
        for i in (0..meta.len()).rev() {
            if let Some(after) = meta.get(i + 1)
                && after.source_line > meta[i].source_line
            {
                next_start = after.source_line;
            }
            let m = &mut meta[i];
            let mut end = m.source_end.min(next_start.saturating_sub(1)).max(m.source_line);
            // A block's range can run through the blank lines after it; it ends at its text.
            while end > m.source_line && is_blank(end) {
                end -= 1;
            }
            m.source_end = end;
        }
        // A collapsed disclosure's summary spans its element, so its body counts as shown.
        let mut folded: Vec<(usize, usize)> = Vec::new();
        for d in self.out.disclosures.iter().filter(|d| !self.expanded.contains(&d.key)) {
            let summary = |m: &LineMeta| m.details.as_ref().is_some_and(|h| *h.key == d.key);
            let Some(src) = meta.iter().find(|m| summary(m)).map(|m| m.source_line) else {
                continue;
            };
            let end = meta.iter().filter(|m| m.source_line == src).map(|m| m.source_end);
            let end = end.max().unwrap_or(src).max(d.end);
            for m in meta.iter_mut().filter(|m| m.source_line == src) {
                m.source_end = end;
            }
            folded.push((src, end));
        }
        let mut shown = vec![false; text.lines().count() + 2];
        let spans = meta.iter().map(|m| (m.source_line, m.source_end)).chain(self.covered);
        for (start, end) in spans {
            for line in start..=end.min(shown.len() - 1) {
                shown[line] = true;
            }
        }
        let mut hidden = vec![false; shown.len()];
        for (start, end) in self.comment_lines {
            for line in start..=end.min(hidden.len() - 1) {
                hidden[line] = true;
            }
        }
        let in_fold = |line: usize| folded.iter().any(|&(s, e)| (s..=e).contains(&line));
        self.out.silent = (1..=blank.len())
            .filter(|&line| !in_fold(line))
            .filter(|&line| hidden[line] || (!is_blank(line) && !shown[line]))
            .collect();
        self.out
    }

    /// Emit one rendered line with its metadata, and any pending heading anchor.
    fn push_line(&mut self, line: Line<'static>, links: Vec<LinkSpan>) {
        if let Some(slug) = self.pending_anchor.take() {
            self.out.anchors.push((slug, self.out.lines.len()));
        }
        let own = self.row_lines.unwrap_or((self.block_src, self.block_end.max(self.block_src)));
        self.out.meta.push(LineMeta {
            source_line: self.block_src,
            source_end: self.block_end.max(self.block_src),
            links,
            details: self.pending_details.take(),
            gap: false,
            lines: own,
        });
        self.out.lines.push(line);
    }

    /// Emit one rendered line with no links, mapped to the current block.
    fn push_plain_line(&mut self, line: Line<'static>) {
        self.push_line(line, Vec::new());
    }

    /// A blank line between blocks, which never takes a pending anchor.
    fn blank_before_block(&mut self) {
        if self.needs_blank && !self.out.lines.is_empty() {
            let bars = "▎".repeat(self.quote.min(MAX_NEST));
            self.out.meta.push(LineMeta {
                source_line: self.block_src,
                source_end: self.block_end.max(self.block_src),
                links: Vec::new(),
                details: None,
                gap: true,
                lines: (self.block_src, self.block_src),
            });
            self.out.lines.push(if bars.is_empty() {
                Line::default()
            } else {
                Line::from(Span::styled(
                    bars,
                    Style::default().fg(self.p.ink(Ink::TextMuted, Fill::Base)),
                ))
            });
        }
        self.needs_blank = false;
    }

    /// Wrap and emit the pending inline run; `set_blank` puts a gap before the next block.
    fn flush_block(&mut self, set_blank: bool) {
        let span = self.inline_span.take();
        if self.inline.iter().all(|c| c.text.trim().is_empty()) {
            self.inline.clear();
            if set_blank {
                self.needs_blank = true;
            }
            return;
        }
        // The run maps to the lines that fed it, marker line included.
        let stamp = (self.block_src, self.block_end);
        if let Some((mut start, mut end)) = span {
            if self.marker.is_some()
                && let Some(line) = self.marker_line.take()
            {
                start = start.min(line);
                end = end.max(line);
            }
            self.block_src = start;
            self.block_end = end;
        }
        self.blank_before_block();
        let marker = self.marker.take();
        let (first, cont) = self.prefix(marker.as_deref());
        let chunks = std::mem::take(&mut self.inline);
        // Wrapping touches only spaces and breaks, so counting the rest maps lines to source.
        let placed = |c: char| c != ' ' && c != '\n';
        let mut shares: Vec<(usize, usize)> = Vec::new();
        let mut total = 0;
        for c in &chunks {
            total += c.text.chars().filter(|&ch| placed(ch)).count();
            shares.push((total, c.line));
        }
        let line_of = |k: usize| shares.iter().find(|&&(end, _)| k < end).map(|&(_, l)| l);
        let fragments: Vec<Fragment> =
            chunks.into_iter().map(|c| (c.text, c.style, c.link)).collect();
        let wrapped = wrap_fragments(&fragments, self.budget(first.width()), true);
        let mut seen = 0;
        for (i, (spans, links)) in wrapped.into_iter().enumerate() {
            let n: usize =
                spans.iter().map(|s| s.content.chars().filter(|&ch| placed(ch)).count()).sum();
            self.row_lines = match (line_of(seen), n.checked_sub(1).and_then(|l| line_of(seen + l)))
            {
                (Some(a), Some(b)) => Some((a, b)),
                _ => Some((self.block_src, self.block_src)),
            };
            seen += n;
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
        self.row_lines = None;
        (self.block_src, self.block_end) = stamp;
        if set_blank {
            self.needs_blank = true;
        }
    }

    /// Take the pending item marker for a non-inline block, its line with it.
    fn take_marker(&mut self) -> Option<String> {
        let marker = self.marker.take();
        if marker.is_some()
            && let Some(line) = self.marker_line.take()
        {
            self.covered.push((line, line));
        }
        marker
    }

    /// Emit styled fragments char-wrapped under the block prefix, a pending marker first.
    fn emit_fragments(&mut self, fragments: Vec<(String, Style)>, extra_indent: &str) {
        let marker = self.take_marker();
        let (first, cont) = self.prefix(marker.as_deref());
        let style = Style::default().fg(self.p.ink(Ink::TextMuted, Fill::Base));
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

    /// Close a code block: highlight it whole, emit it indented and char-wrapped.
    fn end_code(&mut self) {
        let Some(CodeBlock { lang, content, fenced }) = self.code.take() else {
            return;
        };
        self.blank_before_block();
        if lang.as_deref().is_some_and(|l| l.eq_ignore_ascii_case("mermaid")) {
            self.emit_mermaid_placeholder(&content);
            self.needs_blank = true;
            return;
        }
        let content = content.replace('\t', "    ");
        let highlighted = self.hl.highlight(&content, lang.as_deref());
        // Each fence maps to the code line beside it, so a fence edit marks its code.
        let (fence, close) = (self.block_src, self.block_end);
        let block_start = fence + usize::from(fenced);
        let last = highlighted.len().saturating_sub(1);
        for (i, line) in highlighted.into_iter().enumerate() {
            self.block_src = if i == 0 { fence } else { block_start + i };
            self.block_end = if fenced && i == last { close } else { block_start + i };
            self.row_lines = Some((block_start + i, block_start + i));
            let fragments: Vec<(String, Style)> = line
                .into_iter()
                .map(|s| (sanitize(&s.text), Style::default().fg(crate::ui::rgb(s.color))))
                .collect();
            self.emit_fragments(fragments, CODE_INDENT);
        }
        self.row_lines = None;
        self.needs_blank = true;
    }

    fn emit_mermaid_placeholder(&mut self, content: &str) {
        let kind = mermaid_kind(content);
        let text = match kind {
            Some(k) => format!("⧉ mermaid {k}"),
            None => "⧉ mermaid".to_string(),
        };
        let style = Style::default().fg(self.p.ink(Ink::TextMuted, Fill::Base));
        self.emit_fragments(vec![(text, style)], "");
    }

    /// Close a table: aligned, widest columns shrunk to fit, dim source when floors overflow.
    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        self.blank_before_block();
        // `first` and `cont` share a width, so the marker never shifts columns.
        let marker = self.take_marker();
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
        // Each pass levels the widest columns to the next width or their floor: passes ≤ columns.
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
            let style = Style::default().fg(self.p.ink(Ink::TextSecondary, Fill::Base));
            let src = self.source.get(table.range.clone()).unwrap_or("");
            let first_line = self.src_line(table.range.start);
            for (i, src_line) in src.trim_end_matches('\n').split('\n').enumerate() {
                self.block_src = first_line + i;
                self.block_end = self.block_src;
                self.emit_fragments(vec![(sanitize(src_line), style)], "");
            }
            self.needs_blank = true;
            return;
        }

        let dim = Style::default().fg(self.p.mark(Ink::Border, Fill::Base));
        for (r, row) in table.rows.iter().enumerate() {
            let head = r < table.head_rows;
            let style_of =
                |c: &Chunk| if head { c.style.add_modifier(Modifier::BOLD) } else { c.style };
            // Only a cell wider than its column wraps; the row is as tall as its tallest.
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
            // Each row maps to its own source lines, so a comment on one row anchors it.
            if let Some(&(start, end)) = table.row_lines.get(r) {
                self.block_src = start;
                self.block_end = end;
            }
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
                // The header rule stands for the delimiter row, the line under the header.
                self.block_src = self.block_end + 1;
                self.block_end = self.block_src;
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
    /// GitHub's heading slug, numbering duplicates `-1`, `-2`, ….
    fn slugify(&mut self, text: &str) -> String {
        let slug = slug_text(text);
        let n = self.slug_counts.entry(slug.clone()).or_insert(0);
        let out = if *n == 0 { slug.clone() } else { format!("{slug}-{n}") };
        *n += 1;
        out
    }
}

/// GitHub's slug normalization, shared by headings and clicked fragments.
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

/// A control or bidi-override character, refused by both the display and the link opener.
pub(crate) fn hostile_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
}

fn mermaid_kind(content: &str) -> Option<&str> {
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("%%") {
            continue;
        }
        let word = t.split(|c: char| c.is_whitespace() || c == '{' || c == '-').next()?;
        if word.is_empty() {
            continue;
        }
        return Some(word);
    }
    None
}

fn html_heading_level(name: &str) -> HeadingLevel {
    match name {
        "h1" => HeadingLevel::H1,
        "h2" => HeadingLevel::H2,
        "h3" => HeadingLevel::H3,
        "h4" => HeadingLevel::H4,
        "h5" => HeadingLevel::H5,
        _ => HeadingLevel::H6,
    }
}

/// A severity badge encoded as an image alt: `P1` / `P2` / `P3`, optional ` Badge`.
fn badge_label(alt: &str) -> Option<&'static str> {
    match alt.trim().to_ascii_uppercase().as_str() {
        "P1" | "P1 BADGE" => Some("P1"),
        "P2" | "P2 BADGE" => Some("P2"),
        "P3" | "P3 BADGE" => Some("P3"),
        _ => None,
    }
}

enum HtmlTok<'a> {
    Tag { name: &'a str, closing: bool, self_closing: bool, raw: &'a str },
    Text(&'a str),
    Comment,
}

fn html_tokens(s: &str) -> Vec<HtmlTok<'_>> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with("<!--") {
            let end = s[i + 4..].find("-->").map_or(s.len(), |n| i + 4 + n + 3);
            out.push(HtmlTok::Comment);
            i = end;
            continue;
        }
        if bytes[i] == b'<' {
            let end = s[i + 1..].find('>').map_or(s.len(), |n| i + 1 + n + 1);
            let raw = &s[i..end];
            let inner = raw.trim_start_matches('<').trim_end_matches('>');
            let closing = inner.starts_with('/');
            let self_closing = !closing && inner.trim_end().ends_with('/');
            let name_src = inner.trim_start_matches('/').trim_end_matches('/').trim();
            let name_end =
                name_src.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(name_src.len());
            let name = &name_src[..name_end];
            out.push(HtmlTok::Tag { name, closing, self_closing, raw });
            i = end;
            continue;
        }
        let next = s[i..].find('<').map_or(s.len(), |n| i + n);
        if next > i {
            out.push(HtmlTok::Text(&s[i..next]));
        }
        i = next;
    }
    out
}

fn html_tag_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn html_attr(tag: &str, key: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{key}=");
    let at = lower.find(&needle)?;
    let rest = tag.get(at + needle.len()..)?;
    let rest = rest.trim_start();
    let bytes = rest.as_bytes();
    if bytes.first().copied() == Some(b'"') {
        let end = rest[1..].find('"')?;
        return Some(rest[1..=end].to_string());
    }
    if bytes.first().copied() == Some(b'\'') {
        let end = rest[1..].find('\'')?;
        return Some(rest[1..=end].to_string());
    }
    let end = rest.find(|c: char| c.is_whitespace() || c == '/' || c == '>').unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Hostile characters as a visible placeholder, tabs as spaces.
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

/// One wrapped line: spans, and link runs as `(start, end, url index)`.
type WrappedLine = (Vec<Span<'static>>, Vec<(usize, usize, usize)>);

/// Wrap fragments to `budget` cells, at words when `by_words`, else anywhere.
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

/// The wrap state; a word can span styled fragments.
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

    /// Append `text`, merging into the last span and link run where they continue.
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

    /// Place the word, breaking the line before it, or it, when it doesn't fit.
    fn place_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let needs_sep = self.line_w > 0;
        if needs_sep && self.line_w + 1 + self.word_w > self.budget && self.word_w <= self.budget {
            self.flush_line();
        } else if needs_sep {
            // A separator keeps a style only both sides share, so underlines never bleed.
            let next = self.word[0].1;
            let style = match self.line.last() {
                Some(prev) if prev.style == next => next,
                _ => Style::default(),
            };
            // A gap inside one link stays clickable, even unstyled.
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
    use super::{LinkSpan, RenderCache, Rendered, render, render_expanded};
    use crate::highlight::Highlighter;
    use crate::roles::Palette;
    use crate::roles::{Fill, Ink};
    use crate::theme;
    use ratatui::style::{Color, Modifier};
    use ratatui::text::Line;
    use std::collections::HashSet;

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
    fn html_headings_match_markdown_headings() {
        let (hl, p) = setup();
        let md = "<h3>Greptile Summary</h3>\n\nMoves action admission.\n\n<h3>Confidence Score: 4/5</h3>\n";
        let lines = render_lines(md, 80, &hl, &p);
        let t = texts(&lines);
        let i = t
            .iter()
            .position(|l| l.contains("Greptile Summary"))
            .unwrap_or_else(|| panic!("{t:?}"));
        assert!(!t[i].contains("<h3>"), "{t:?}");
        let span = lines[i].spans.iter().find(|s| s.content.contains("Greptile")).unwrap();
        // Catppuccin's H3 yellow, the same as a `###` heading.
        assert_eq!(span.style.fg, Some(Color::Rgb(0xf9, 0xe2, 0xaf)), "h3 takes the h3 color");
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        let j = t.iter().position(|l| l.contains("Confidence Score")).unwrap();
        let span = lines[j].spans.iter().find(|s| s.content.contains("Confidence")).unwrap();
        assert_eq!(span.style.fg, Some(Color::Rgb(0xf9, 0xe2, 0xaf)));
    }

    #[test]
    fn crlf_source_renders_as_its_lf_twin() {
        // A CRLF ending is a line ending, never a shown `�`.
        let (hl, p) = setup();
        let md =
            "# T\r\n\r\nA paragraph\r\nthat wraps.\r\n\r\n| a | b |\r\n|---|---|\r\n| 1 | 2 |\r\n";
        let crlf = render(md, 80, &hl, &p);
        let lf = render(&md.replace("\r\n", "\n"), 80, &hl, &p);
        let t = texts(&crlf.lines);
        assert_eq!(t, ["T", "", "A paragraph that wraps.", "", "a │ b", "─────", "1 │ 2"]);
        assert_eq!(t, texts(&lf.lines));
        let lines = |r: &Rendered| r.meta.iter().map(|m| m.lines).collect::<Vec<_>>();
        assert_eq!(lines(&crlf), lines(&lf), "the same source lines");
    }

    #[test]
    fn heading_is_bold_in_its_theme_color_without_markers() {
        let (hl, p) = setup();
        let lines = render_lines("## Install", 80, &hl, &p);
        assert_eq!(texts(&lines), vec!["Install"]);
        let span = &lines[0].spans[1]; // [0] is the (empty) prefix
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        // Catppuccin's H2 peach.
        assert_eq!(span.style.fg, Some(Color::Rgb(0xfa, 0xb3, 0x87)));
    }

    /// Each level takes the theme's color for it: Catppuccin's markdown rainbow, red through
    /// lavender. A theme with no heading colors falls back to body text, deeper levels secondary.
    #[test]
    fn each_heading_level_takes_its_theme_color() {
        let (hl, p) = setup();
        let rainbow = [0xf3_8b_a8, 0xfa_b3_87, 0xf9_e2_af, 0xa6_e3_a1, 0x74_c7_ec, 0xb4_be_fe];
        for (level, rgb) in (1..=6).zip(rainbow) {
            let md = format!("{} Title", "#".repeat(level));
            let lines = render_lines(&md, 80, &hl, &p);
            let [r, g, b] = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8];
            assert_eq!(lines[0].spans[1].style.fg, Some(Color::Rgb(r, g, b)), "h{level}");
        }
        let t = theme::resolve(Some("one-dark"));
        let (hl, p) = (Highlighter::new(t.syntax), t.palette);
        let fg = |md: &str| render_lines(md, 80, &hl, &p)[0].spans[1].style.fg;
        assert_eq!(fg("## Title"), Some(p.ink(Ink::Text, Fill::Base)));
        assert_eq!(fg("#### Title"), Some(p.ink(Ink::TextSecondary, Fill::Base)));
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

    /// Every heading level and the code chip clear 4.5:1 in every theme, whatever its colors.
    #[test]
    fn markdown_colors_read_in_every_theme() {
        use crate::roles::{TEXT_FLOOR, contrast};
        let md = "# a\n\n## b\n\n### c\n\n#### d\n\n##### e\n\n###### f\n\nrun `x` now\n";
        for name in theme::NAMES {
            let t = theme::resolve(Some(name));
            let (hl, p) = (Highlighter::new(t.syntax), t.palette);
            let lines = render_lines(md, 80, &hl, &p);
            let fg_of = |needle: &str| {
                let span = lines.iter().flat_map(|l| &l.spans).find(|s| s.content == needle);
                span.unwrap_or_else(|| panic!("{name}: {needle:?}")).style
            };
            for h in ["a", "b", "c", "d", "e", "f"] {
                let c = contrast(fg_of(h).fg.unwrap(), p.fill(Fill::Base));
                assert!(c >= TEXT_FLOOR, "{name}: heading {h} {c:.2}");
            }
            let code = fg_of("x");
            assert_eq!(code.bg, Some(p.fill(Fill::Code)), "{name}: the code chip");
            let c = contrast(code.fg.unwrap(), p.fill(Fill::Code));
            assert!(c >= TEXT_FLOOR, "{name}: inline code {c:.2}");
            let level = |h| fg_of(h).fg;
            match name {
                "everforest" => assert_ne!(level("a"), level("b"), "everforest colors each level"),
                "ayu" => assert!(["b", "c", "f"].iter().all(|&h| level(h) == level("a"))),
                _ => {}
            }
        }
    }

    /// Inline code sits on the code chip in the theme's code color: Catppuccin's green on
    /// surface0.
    #[test]
    fn inline_code_sits_on_a_chip_in_its_theme_color() {
        let (hl, p) = setup();
        let lines = render_lines("run `cargo test` now", 80, &hl, &p);
        let code = lines[0].spans.iter().find(|s| s.content.contains("cargo test")).unwrap();
        assert_eq!(code.style.fg, Some(Color::Rgb(0xa6, 0xe3, 0xa1)));
        assert_eq!(code.style.bg, Some(Color::Rgb(0x31, 0x32, 0x44)));
        let prose = lines[0].spans.iter().find(|s| s.content.contains("run")).unwrap();
        assert_eq!(prose.style.bg, None, "only the code wears the chip");
    }

    #[test]
    fn a_mermaid_fence_collapses_to_a_placeholder() {
        let (hl, p) = setup();
        let md = "```mermaid\n%%{init: {'theme': 'neutral'}}%%\nflowchart TD\n  A[Submit] --> B[Validate]\n```\n";
        let t = texts(&render_lines(md, 80, &hl, &p));
        assert!(t.iter().any(|l| l.contains("⧉ mermaid flowchart")), "{t:?}");
        assert!(t.iter().all(|l| !l.contains("Submit") && !l.contains("-->")), "{t:?}");
    }

    #[test]
    fn fenced_rust_highlights_like_the_diff_pane() {
        let (hl, p) = setup();
        let lines = render_lines("```rust\nlet x = 1;\n```", 80, &hl, &p);
        assert_eq!(texts(&lines), vec!["  let x = 1;"]);
        // `let` keyword takes a syntax color different from the plain text color.
        let colors: Vec<_> = lines[0].spans.iter().filter_map(|s| s.style.fg).collect();
        assert!(colors.len() > 2, "rust tokenizes into several colored spans: {colors:?}");
        assert!(colors.iter().any(|&c| c != p.ink(Ink::Text, Fill::Base)));
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
        assert_eq!(dest.style.fg, Some(p.ink(Ink::TextMuted, Fill::Base)));
        let label = lines[0].spans.iter().find(|s| s.content.contains("the run")).unwrap();
        assert!(label.style.add_modifier.contains(Modifier::UNDERLINED));

        let auto = render_lines("<https://ci.example/1>", 80, &hl, &p);
        assert_eq!(text_of(&auto[0]), "https://ci.example/1");
    }

    #[test]
    fn the_underline_never_bleeds_into_the_gaps_around_a_link() {
        let (hl, p) = setup();
        // A one-word link with its destination, and a two-word link's interior space.
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
        let r = render("![build badge](https://img.example/b.svg)", 80, &hl, &p);
        assert_eq!(text_of(&r.lines[0]), "⧉ build badge");
        assert_eq!(&*r.meta[0].links[0].url, "https://img.example/b.svg");
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
        // At pane 24 the 26-wide note column shrinks to 19 and wraps; id keeps its width.
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
        // A 15-cell pane leaves prose exactly its 8-cell floor: still a grid.
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
    fn details_collapse_to_a_summary_line() {
        let (hl, p) = setup();
        let r = render("<details>\n<summary>hi</summary>\nchrome\n</details>", 80, &hl, &p);
        let t = texts(&r.lines);
        assert!(t.iter().any(|l| l.contains("▸ hi")), "{t:?}");
        assert!(t.iter().all(|l| !l.contains("chrome") && !l.contains("<details>")), "{t:?}");
        let hit = r.meta.iter().find_map(|m| m.details.as_ref()).expect("summary hit");
        assert_eq!(&*hit.key, "hi#0");
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
        let named: Vec<_> = crate::roles::INKS
            .iter()
            .flat_map(|&ink| {
                crate::roles::FILLS.iter().flat_map(move |&on| [p.ink(ink, on), p.mark(ink, on)])
            })
            .collect();
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
        assert_eq!(render_lines("", 80, &hl, &p), Vec::new());
        let t = texts(&render_lines("just plain prose", 80, &hl, &p));
        assert_eq!(t, vec!["just plain prose"]);
    }

    #[test]
    fn meta_maps_rendered_lines_to_their_source_blocks() {
        let (hl, p) = setup();
        // Lines 6-7 are code between fences 5 and 8, each fence joining its neighbor.
        let md = "# Title\n\nprose here\n\n```rust\nlet a = 1;\nlet b = 2;\n```\n";
        let r = render(md, 80, &hl, &p);
        let t = texts(&r.lines);
        let src =
            |needle: &str| r.meta[t.iter().position(|l| l.contains(needle)).unwrap()].source_line;
        assert_eq!(src("Title"), 1);
        assert_eq!(src("prose here"), 3);
        assert_eq!(src("let a = 1;"), 5, "the first code line holds its fence");
        assert_eq!(src("let b = 2;"), 7, "a later one its own line");
        let sorted = r.meta.iter().map(|m| m.source_line).collect::<Vec<_>>();
        let mut expect = sorted.clone();
        expect.sort_unstable();
        assert_eq!(sorted, expect, "source lines are non-decreasing, so lookups can bisect");
    }

    /// `(first line, last line)` of the rendered line containing `needle`.
    fn span_of(r: &Rendered, needle: &str) -> (usize, usize) {
        let t = texts(&r.lines);
        let m = &r.meta[t.iter().position(|l| l.contains(needle)).unwrap()];
        (m.source_line, m.source_end)
    }

    #[test]
    fn every_rendered_line_maps_to_its_blocks_whole_source_range() {
        let (hl, p) = setup();
        let md = "para one\ncontinues here\n\n- item a\n  more a\n- item b\n  - nested\n\n\
                  ```\nx = 1\n```\n";
        let r = render(md, 80, &hl, &p);
        assert_eq!(span_of(&r, "para one"), (1, 2), "a paragraph spans its lines");
        assert_eq!(span_of(&r, "item a"), (4, 5), "an item ends where the next begins");
        assert_eq!(span_of(&r, "item b"), (6, 6), "a parent item never claims its nested list");
        assert_eq!(span_of(&r, "nested"), (7, 7));
        assert_eq!(span_of(&r, "x = 1"), (9, 11), "a lone code line holds both its fences");
        for m in &r.meta {
            assert!(m.source_line <= m.source_end, "{m:?}");
        }
        assert!(r.silent.is_empty(), "every line shows: {:?}", r.silent);
    }

    #[test]
    fn a_lines_own_source_lines_run_forward_through_links_and_code_spans() {
        let (hl, p) = setup();
        // A link and a code span across line breaks: each piece keeps its own line.
        let md = "aaa [link text\nmore words](http://example.com/a/b/c) end of\n\n\
                  x `code one\ncode two` y\n";
        for width in [16, 17, 80] {
            let r = render(md, width, &hl, &p);
            for (line, m) in r.lines.iter().zip(&r.meta) {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(m.lines.0 <= m.lines.1, "{text:?} at {width}: {:?}", m.lines);
            }
        }
        let r = render(md, 16, &hl, &p);
        let t = texts(&r.lines);
        let at = |needle: &str| r.meta[t.iter().position(|l| l.contains(needle)).unwrap()].lines;
        assert_eq!(at("://example.com/a"), (2, 2), "the url follows line 2's text: {t:?}");
        assert_eq!(at("x code one code"), (4, 5), "`code` and `one` from line 4: {t:?}");
        assert_eq!(at("two y"), (5, 5), "{t:?}");
        // Container prefixes never shift a span's piece onto the wrong line.
        for (md, width, needle, want) in [
            ("- x `aa\n  bb\n  cc` y\n", 4, "bb", (2, 2)),
            ("- x `aa\n  bb\n  cc` y\n", 6, "bb", (2, 2)),
            ("> - x `aa\n>   bb\n>   cc` y\n", 6, "cc", (3, 3)),
            ("Z0 `e\r\nff hfi\r\nn` Z1\n", 6, "ff", (2, 2)),
            ("Z0 `e\r\nff hfi\r\nn` Z1\n", 6, "n Z1", (3, 3)),
        ] {
            let r = render(md, width, &hl, &p);
            let t = texts(&r.lines);
            let row = t.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{t:?}"));
            assert_eq!(r.meta[row].lines, want, "{md:?} at {width}: {t:?}");
        }
    }

    #[test]
    fn table_rows_map_to_their_own_source_lines() {
        let (hl, p) = setup();
        let md = "| a | b |\n|---|---|\n| one | two |\n| three | four |\n";
        let r = render(md, 80, &hl, &p);
        assert_eq!(span_of(&r, "a"), (1, 1), "the header row");
        assert_eq!(span_of(&r, "─"), (2, 2), "the rule stands for the delimiter row");
        assert_eq!(span_of(&r, "one"), (3, 3));
        assert_eq!(span_of(&r, "three"), (4, 4));
        // Over-wide at every floor: the source fallback maps line by line too.
        let wide = format!("| {} | {} |\n|---|---|\n| x | y |\n", "w".repeat(60), "v".repeat(60));
        let r = render(&wide, 10, &hl, &p);
        assert_eq!(span_of(&r, "| x | y |"), (3, 3), "{:?}", texts(&r.lines));
    }

    #[test]
    fn lines_that_render_nothing_are_silent() {
        let (hl, p) = setup();
        // 1 para, 3 comment, 5 ref def, 7-10 collapsed details, 12 tail.
        let md = "shown\n\n<!-- hidden note -->\n\n[ref]: https://x.dev\n\n<details>\n<summary>More</summary>\n\
                  inside\n</details>\n\ntail\n";
        let r = render(md, 80, &hl, &p);
        assert_eq!(r.silent, [3, 5], "only what nothing shows");
        let summary = r.meta.iter().find(|m| m.details.is_some()).expect("the summary");
        assert_eq!((summary.source_line, summary.source_end), (8, 10), "it spans its element");
    }

    #[test]
    fn silent_lines_never_flag_source_that_shows() {
        let (hl, p) = setup();
        let silent = |md: &str| render(md, 80, &hl, &p).silent;
        // A tight item's text keeps its own line before a nested block.
        for md in [
            "- item\n  ```\n  code\n  ```\n",
            "- item\n\n  | a | b |\n  |---|---|\n  | c | d |\n",
            "- item\n  <div>x</div>\n",
            "- a\n  # h\n",
        ] {
            assert!(silent(md).is_empty(), "{md:?}: {:?}", silent(md));
        }
        assert!(silent("> a\n>\n> b\n").is_empty(), "a quote's `>` lines");
        assert!(silent("> [!NOTE]\n>\n> text\n").is_empty(), "a GitHub alert");
        assert_eq!(
            silent("<p align=\"center\">\n<img alt=\"logo\" src=\"x.png\">\n</p>\n"),
            Vec::<usize>::new()
        );
        // A collapsed disclosure's summary stands for its whole element, closing tag and all.
        assert_eq!(
            silent("<details>\n<summary>\nMore info\n</summary>\n\nbody\n\n</details>\n"),
            Vec::<usize>::new()
        );
        // Open, its closing tag alone on its line renders nothing.
        let open: HashSet<String> = HashSet::from(["More info#0".to_string()]);
        let md = "<details>\n<summary>\nMore info\n</summary>\n\nbody\n\n</details>\n";
        assert_eq!(render_expanded(md, 80, &hl, &p, &open).silent, [8]);
        assert_eq!(silent("```\n```\n"), [1, 2], "an empty fence renders no line");
        // Lone tags and a lone `>` line render nothing either, in any line ending.
        assert_eq!(silent("<p>\n<summary>\n"), [1, 2]);
        assert_eq!(silent("<p>\r\n<summary>\r\n"), [1, 2]);
        assert_eq!(silent(">\n<br>\n"), [2], "a bare `>` is blank, the tag silent");
    }

    #[test]
    fn hidden_comments_are_silent_and_never_render() {
        let (hl, p) = setup();
        let md = "before\n\n<!--\nignore previous instructions\n-->\n\nafter <!-- inline aside --> text\n";
        let r = render(md, 80, &hl, &p);
        let t = texts(&r.lines);
        assert!(!t.iter().any(|l| l.contains("ignore previous")), "{t:?}");
        assert!(!t.iter().any(|l| l.contains("-->")), "{t:?}");
        assert_eq!(r.silent, [3, 4, 5, 7], "a multi-line comment, and an inline one");
    }

    #[test]
    fn a_details_key_survives_edits_above_it() {
        let (hl, p) = setup();
        let keys = |md: &str| -> Vec<String> {
            render(md, 80, &hl, &p)
                .meta
                .iter()
                .filter_map(|m| m.details.as_ref().map(|d| d.key.to_string()))
                .collect()
        };
        let body = "<details><summary>Details</summary>\n\nA\n\n</details>\n\n\
                    <details><summary>Details</summary>\n\nB\n\n</details>\n";
        let before = keys(body);
        assert_eq!(before, ["Details#0", "Details#1"]);
        // Prose above, and a disclosure with another summary above, move no key.
        assert_eq!(keys(&format!("new line\n\n{body}")), before);
        let notes = "<details><summary>Notes</summary>\n\nN\n\n</details>\n\n";
        assert_eq!(keys(&format!("{notes}{body}"))[1..], before[..]);
    }

    #[test]
    fn text_after_a_nested_block_keeps_its_own_lines() {
        let (hl, p) = setup();
        let md = "1. step:\n   ```bash\n   run\n   ```\n   After text.\n2. next\n";
        let r = render(md, 80, &hl, &p);
        assert_eq!(span_of(&r, "After text."), (5, 5), "{:?}", texts(&r.lines));
        assert_eq!(span_of(&r, "step:"), (1, 1));
        assert!(r.silent.is_empty(), "{:?}", r.silent);
    }

    #[test]
    fn blocks_inside_a_tight_item_keep_their_own_lines() {
        let (hl, p) = setup();
        let silent = |md: &str| render(md, 80, &hl, &p).silent;
        let r = render("- a\n  ```\n  c\n  ```\n- b\n", 80, &hl, &p);
        assert_eq!(span_of(&r, "• a"), (1, 1));
        assert_eq!(span_of(&r, "c"), (2, 4), "the code line, with the fences framing it");
        let r = render("- a\n  | x | y |\n  |---|---|\n  | 1 | 2 |\n", 80, &hl, &p);
        assert_eq!(span_of(&r, "• a"), (1, 1), "{:?}", texts(&r.lines));
        assert_eq!(span_of(&r, "1"), (4, 4), "{:?}", texts(&r.lines));
        assert!(silent("- [ ] \n  ```\n  c\n  ```\n").is_empty(), "a task line over a code block");
        assert!(silent("- [x]\n  [link](https://x.dev)\n").is_empty(), "the ☑ line shows");
        assert!(silent("-\n  text on next\n").is_empty(), "the bullet line shows");
        assert!(silent("- item\n  </Note>\n").is_empty(), "a tag-only inline tag, as in a block");
    }

    #[test]
    fn a_comment_never_swallows_html_after_its_block() {
        let (hl, p) = setup();
        let md = "<!-- a --> b <!-- c\nstill hidden\n-->\n\n<details><summary>S</summary>\n\nbody\n\n</details>\n";
        let t = texts(&render(md, 80, &hl, &p).lines);
        assert!(t.iter().any(|l| l.contains("▸ S")), "the later disclosure renders: {t:?}");
        let md = "<div>\n<!-- start\n\nmid\n-->\n</div>\n\nend <img alt=\"x\">\n";
        let t = texts(&render(md, 80, &hl, &p).lines);
        assert!(t.iter().any(|l| l.contains("⧉ x")), "{t:?}");
    }

    #[test]
    fn details_sharing_a_summary_open_independently() {
        let (hl, p) = setup();
        let md = "<details><summary>Details</summary>\n\nfirst body\n\n</details>\n\n\
                  <details><summary>Details</summary>\n\nsecond body\n\n</details>\n";
        let closed = render(md, 80, &hl, &p);
        let keys: Vec<String> = closed
            .meta
            .iter()
            .filter_map(|m| m.details.as_ref().map(|d| d.key.to_string()))
            .collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1], "one summary, two disclosures");
        let open: HashSet<String> = [keys[1].clone()].into();
        let t = texts(&render_expanded(md, 80, &hl, &p, &open).lines);
        assert!(t.iter().any(|l| l.contains("second body")), "{t:?}");
        assert!(!t.iter().any(|l| l.contains("first body")), "{t:?}");
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
        let bar = t.iter().position(|l| l.contains("bar")).unwrap();
        assert!(foo < bar, "the item's text keeps its order: {t:?}");
        assert!(t.iter().all(|l| !l.contains("<div>")), "unknown tags drop: {t:?}");
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
    fn a_codex_badge_paints_as_a_chip() {
        let (hl, p) = setup();
        let md = "**<sub><sub>![P2 Badge](https://img.shields.io/badge/P2-yellow?style=flat)</sub></sub>  Serialize the first write**";
        let lines = render_lines(md, 80, &hl, &p);
        let t = text_of(&lines[0]);
        assert!(t.contains("P2"), "{t}");
        assert!(t.contains("Serialize the first write"), "{t}");
        assert!(!t.contains("<sub>"), "{t}");
        assert!(!t.contains("⧉"), "{t}");
        let chip = lines[0].spans.iter().find(|s| s.content == "P2").unwrap();
        assert_eq!(chip.style.fg, Some(p.ink(Ink::Warning, Fill::Base)));
        assert!(chip.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_greptile_html_img_paints_as_a_chip() {
        let (hl, p) = setup();
        let md = "<a href=\"#\"><img alt=\"P1\" src=\"https://example.com/p1.svg\"></a> **Backfill misses live updates**";
        let lines = render_lines(md, 80, &hl, &p);
        let t = text_of(&lines[0]);
        assert!(t.contains("P1"), "{t}");
        assert!(t.contains("Backfill misses live updates"), "{t}");
        assert!(!t.contains("<img"), "{t}");
        let chip = lines[0].spans.iter().find(|s| s.content == "P1").unwrap();
        assert_eq!(chip.style.fg, Some(p.ink(Ink::Danger, Fill::Base)));
    }

    #[test]
    fn an_opened_details_body_is_markdown() {
        let (hl, p) = setup();
        let md = "<details> <summary>About Codex</summary>\n\n- one\n\n</details>";
        let mut open = HashSet::new();
        open.insert("About Codex#0".into());
        let r = render_expanded(md, 80, &hl, &p, &open);
        let t = texts(&r.lines);
        assert!(t.iter().any(|l| l.contains("▾ About Codex")), "{t:?}");
        assert!(t.iter().any(|l| l.contains("• one")), "{t:?}");
    }

    #[test]
    fn a_details_without_a_summary_stays_hidden() {
        let (hl, p) = setup();
        let t = texts(&render_lines("<details>\nchrome lives here\n</details>", 80, &hl, &p));
        assert!(t.iter().all(|l| !l.contains("chrome lives here")), "{t:?}");
    }

    #[test]
    fn a_self_closing_details_does_not_eat_the_rest() {
        let (hl, p) = setup();
        let t = texts(&render_lines("before <details/> after", 80, &hl, &p));
        let joined = t.join(" ");
        assert!(joined.contains("before"), "{t:?}");
        assert!(joined.contains("after"), "{t:?}");
    }

    #[test]
    fn a_self_closing_relative_time_does_not_eat_the_rest() {
        let (hl, p) = setup();
        let t = texts(&render_lines(
            "before <relative-time datetime=\"2026-09-16T13:42:37Z\"/> after",
            80,
            &hl,
            &p,
        ));
        let joined = t.join(" ");
        assert!(joined.contains("before"), "{t:?}");
        assert!(joined.contains("after"), "{t:?}");
    }

    #[test]
    fn relative_time_contributes_no_text() {
        let (hl, p) = setup();
        let t = texts(&render_lines(
            "Completed <relative-time datetime=\"2026-09-16T13:42:37Z\">2026-09-16</relative-time> ok",
            80,
            &hl,
            &p,
        ));
        let joined = t.join(" ");
        assert!(joined.contains("Completed"), "{t:?}");
        assert!(joined.contains("ok"), "{t:?}");
        assert!(!joined.contains("2026-09-16"), "{t:?}");
        assert!(!joined.contains("relative-time"), "{t:?}");
    }
}
