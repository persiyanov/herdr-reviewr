//! Syntax highlighting via `syntect`: foreground spans per line, from the theme's syntax theme.

use std::borrow::Cow;
use std::fmt;
use std::io::Cursor;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{Scope, SyntaxSet};

use std::sync::OnceLock;

use crate::diff::{Rgb, Span};
use crate::theme::SyntaxChoice;

/// The default text color when a theme carries none, or its syntax theme fails to load.
const DEFAULT_FG: Rgb = (0xcd, 0xd6, 0xf4);

/// The two-face syntax set, deserialized once per process.
fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// The two-face theme set, deserialized once; a theme switch clones one theme out of it.
fn embedded_themes() -> &'static two_face::theme::EmbeddedLazyThemeSet {
    static THEMES: OnceLock<two_face::theme::EmbeddedLazyThemeSet> = OnceLock::new();
    THEMES.get_or_init(two_face::theme::extra)
}

/// The active syntax theme, `None` when it failed to load, which highlights as plain spans.
pub struct Highlighter {
    theme: Option<Theme>,
    default_fg: Rgb,
    markdown: MarkdownColors,
}

/// The colors a syntax theme gives rendered markdown, read once from its rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarkdownColors {
    /// H1 through H6; `None` where the theme sets no heading color.
    pub headings: [Option<Rgb>; 6],
    /// `None` where the theme sets no inline-code color.
    pub inline_code: Option<Rgb>,
}

impl MarkdownColors {
    /// Ask the theme under VS Code's and `TextMate`'s markdown scopes; highlighting a sample can't,
    /// since the bundled grammar levels only H1 and H2.
    fn of(theme: &Theme) -> Self {
        let hl = syntect::highlighting::Highlighter::new(theme);
        let fg = |stack: &[&str]| -> Option<Rgb> {
            let scopes: Vec<Scope> = stack.iter().filter_map(|s| Scope::new(s).ok()).collect();
            hl.style_mod_for_stack(&scopes).foreground.map(|c| (c.r, c.g, c.b))
        };
        // A rule for all markdown text is not a heading or code color.
        let plain = fg(&["text.html.markdown"]);
        let probe = |scope: &str| fg(&["text.html.markdown", scope]).filter(|c| Some(*c) != plain);
        let headings = std::array::from_fn(|i| {
            let n = i + 1;
            probe(&format!("heading.{n}.markdown"))
                .or_else(|| probe(&format!("markup.heading.{n}.markdown")))
                .or_else(|| probe("markup.heading.markdown"))
        });
        let inline_code = probe("markup.inline.raw.string.markdown")
            .or_else(|| probe("markup.raw.inline.markdown"));
        Self { headings, inline_code }
    }
}

impl fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Highlighter").finish_non_exhaustive()
    }
}

impl Highlighter {
    /// Build from a bundled `.tmTheme` or a `two-face` theme; one that fails to parse goes plain.
    pub fn new(syntax: SyntaxChoice) -> Self {
        let theme = match syntax {
            SyntaxChoice::Bundled(bytes) => {
                match ThemeSet::load_from_reader(&mut Cursor::new(bytes)) {
                    Ok(theme) => Some(theme),
                    Err(e) => {
                        crate::logln!("bundled syntax theme failed to parse: {e}");
                        None
                    }
                }
            }
            SyntaxChoice::Embedded(name) => Some(embedded_themes().get(name).clone()),
        };
        let default_fg = theme
            .as_ref()
            .and_then(|t| t.settings.foreground)
            .map_or(DEFAULT_FG, |c| (c.r, c.g, c.b));
        let markdown = theme.as_ref().map(MarkdownColors::of).unwrap_or_default();
        Self { theme, default_fg, markdown }
    }

    /// The theme's own colors for rendered markdown headings and inline code.
    #[must_use]
    pub fn markdown(&self) -> MarkdownColors {
        self.markdown
    }

    /// Highlight `content` line by line.
    pub fn highlight(&self, content: &str, language: Option<&str>) -> Vec<Vec<Span>> {
        self.highlight_lines(&crate::text::lines(content), language)
    }

    /// Highlight `lines` into spans per line, plain with no known language or theme.
    pub fn highlight_lines(&self, lines: &[&str], language: Option<&str>) -> Vec<Vec<Span>> {
        let syntaxes = syntaxes();
        let syntax = language.and_then(|lang| {
            syntaxes.find_syntax_by_extension(lang).or_else(|| syntaxes.find_syntax_by_token(lang))
        });
        let (Some(syntax), Some(theme)) = (syntax, self.theme.as_ref()) else {
            return lines
                .iter()
                .map(|l| {
                    vec![Span {
                        text: crate::text::line_body(l).0.to_string(),
                        color: self.default_fg,
                    }]
                })
                .collect();
        };
        let mut h = HighlightLines::new(syntax, theme);
        let mut out = Vec::new();
        for &line in lines {
            // A CR-ended line highlights as its LF form.
            let (text, cr) = crate::text::line_body(line);
            let line: Cow<'_, str> =
                if cr { Cow::Owned(format!("{text}\n")) } else { Cow::Borrowed(line) };
            let spans = match h.highlight_line(&line, syntaxes) {
                Ok(regions) => regions
                    .into_iter()
                    .map(|(style, text)| Span {
                        text: text.trim_end_matches('\n').to_string(),
                        color: (style.foreground.r, style.foreground.g, style.foreground.b),
                    })
                    .collect(),
                // A grammar error degrades to plain text rather than blocking the diff.
                Err(_) => vec![Span { text: text.to_string(), color: self.default_fg }],
            };
            out.push(spans);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Highlighter;
    use crate::theme;

    /// The bundled Catppuccin Mocha syntax, the default theme's pairing.
    fn mocha() -> super::SyntaxChoice {
        theme::resolve(Some("catppuccin")).syntax
    }

    #[test]
    fn highlights_rust_into_colored_spans() {
        let h = Highlighter::new(mocha());
        let lines = h.highlight("let x = 1;\n", Some("rs"));
        assert_eq!(lines.len(), 1);
        let spans = &lines[0];
        assert!(spans.len() > 1, "rust tokenizes into several spans");
        assert_eq!(spans.iter().map(|s| s.text.as_str()).collect::<String>(), "let x = 1;");
        // The Catppuccin keyword color (purple) differs from the default text color.
        assert!(spans.iter().any(|s| s.text == "let" && s.color != (0xcd, 0xd6, 0xf4)));
    }

    #[test]
    fn unknown_language_is_one_plain_span_per_line() {
        let h = Highlighter::new(mocha());
        let lines = h.highlight("alpha\nbeta\n", None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], vec![super::Span { text: "alpha".into(), color: (0xcd, 0xd6, 0xf4) }]);
    }

    #[test]
    fn a_line_ending_cr_is_never_span_text() {
        let h = Highlighter::new(mocha());
        let text = |lines: Vec<Vec<super::Span>>| -> Vec<String> {
            lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect()
        };
        // A CRLF line, an LF line, a final bare CR; an inner CR stays.
        let content = "let a = 1;\r\nlet b = 2;\nlet c\r= 3;\r";
        let want = ["let a = 1;", "let b = 2;", "let c\r= 3;"];
        assert_eq!(text(h.highlight(content, Some("rs"))), want);
        assert_eq!(text(h.highlight(content, None)), want);
        // The CRLF line tokenizes as its LF twin does.
        assert_eq!(
            h.highlight("let a = 1;\r\n", Some("rs")),
            h.highlight("let a = 1;\n", Some("rs"))
        );
    }

    #[test]
    fn the_onig_only_grammars_highlight_javascript_and_powershell() {
        // The fancy-regex dumps leave these out, so a swapped regex engine shows up here.
        for (ext, want) in [
            ("js", "JavaScript (Babel)"),
            ("jsx", "JavaScript (Babel)"),
            ("mjs", "JavaScript (Babel)"),
            ("cjs", "JavaScript (Babel)"),
            ("ps1", "PowerShell"),
        ] {
            let found = super::syntaxes().find_syntax_by_extension(ext).map(|s| s.name.as_str());
            assert_eq!(found, Some(want), ".{ext}");
        }
    }

    #[test]
    fn bundled_syntax_themes_all_parse() {
        // Each bundled theme must load: a failed load would yield one plain span.
        for name in [
            "catppuccin",
            "tokyo-night",
            "tokyo-night-day",
            "rose-pine",
            "rose-pine-dawn",
            "ayu",
            "everforest",
        ] {
            let h = Highlighter::new(theme::resolve(Some(name)).syntax);
            let spans = h.highlight("let x = 1;\n", Some("rs"));
            assert!(spans[0].len() > 1, "{name}: bundled syntax theme failed to load");
        }
    }
}
