//! Syntax highlighting via `syntect`, themed by the active theme's paired syntax theme.
//!
//! The highlighter is rebuilt when the theme
//! changes and produces per-line foreground spans; the pane keeps the terminal's own
//! background, so only token colors come from the theme.

use std::fmt;
use std::io::Cursor;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use std::sync::OnceLock;

use ratatui::style::Color;

use crate::diff::Span;
use crate::theme::SyntaxChoice;

/// The default text color when a theme carries none, or its syntax theme fails to load.
const DEFAULT_FG: Color = Color::Rgb(0xcd, 0xd6, 0xf4);

/// The broad bat/two-face syntax set, built once per process (it is expensive to
/// deserialize) and shared across every `Highlighter`.
fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// The two-face embedded theme set, deserialized once and shared — like [`syntaxes`], so a
/// theme switch clones one theme out of the cached set instead of rebuilding the whole dump.
fn embedded_themes() -> &'static two_face::theme::EmbeddedLazyThemeSet {
    static THEMES: OnceLock<two_face::theme::EmbeddedLazyThemeSet> = OnceLock::new();
    THEMES.get_or_init(two_face::theme::extra)
}

/// Holds the active syntax theme (absent when it failed to load); highlights file content
/// into spans against the shared syntax set.
pub struct Highlighter {
    theme: Option<Theme>,
    default_fg: Color,
    terminal: bool,
}

impl fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Highlighter").finish_non_exhaustive()
    }
}

impl Highlighter {
    /// Build from a theme's paired syntax source: a bundled `.tmTheme` (parsed from vendored
    /// bytes), or a theme from the `two-face` embedded set. A bundled theme that fails to
    /// parse leaves the highlighter theme-less, so highlighting degrades to plain spans
    /// rather than crashing. Most files color out of the box via the
    /// broad two-face syntax set.
    pub fn new(syntax: SyntaxChoice) -> Self {
        let terminal = matches!(syntax, SyntaxChoice::Terminal);
        let theme = match syntax {
            SyntaxChoice::Terminal => {
                match ThemeSet::load_from_reader(&mut Cursor::new(crate::theme::TERMINAL_TM)) {
                    Ok(theme) => Some(theme),
                    Err(error) => {
                        crate::logln!("terminal syntax theme failed to parse: {error}");
                        None
                    }
                }
            }
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
            .map_or(if terminal { Color::Reset } else { DEFAULT_FG }, |c| {
                syntax_color(terminal, (c.r, c.g, c.b))
            });
        Self { theme, default_fg, terminal }
    }

    /// Highlight `content` line by line. Each inner `Vec` is one line's spans. With no
    /// known `language` — or no loaded theme — every line is a single plain span in the
    /// default color. `language` matches as an extension first (paths), then as a token
    /// name (markdown fence tags like `rust` or `python`).
    pub fn highlight(&self, content: &str, language: Option<&str>) -> Vec<Vec<Span>> {
        let syntaxes = syntaxes();
        let syntax = language.and_then(|lang| {
            syntaxes.find_syntax_by_extension(lang).or_else(|| syntaxes.find_syntax_by_token(lang))
        });
        let (Some(syntax), Some(theme)) = (syntax, self.theme.as_ref()) else {
            return content
                .lines()
                .map(|l| vec![Span { text: l.to_string(), color: self.default_fg }])
                .collect();
        };
        let mut h = HighlightLines::new(syntax, theme);
        let mut out = Vec::new();
        for line in LinesWithEndings::from(content) {
            let spans = match h.highlight_line(line, syntaxes) {
                Ok(regions) => regions
                    .into_iter()
                    .map(|(style, text)| Span {
                        text: text.trim_end_matches('\n').to_string(),
                        color: syntax_color(
                            self.terminal,
                            (style.foreground.r, style.foreground.g, style.foreground.b),
                        ),
                    })
                    .collect(),
                // A grammar error degrades to plain text rather than blocking the diff.
                Err(_) => vec![Span {
                    text: line.trim_end_matches('\n').to_string(),
                    color: self.default_fg,
                }],
            };
            out.push(spans);
        }
        out
    }
}

fn syntax_color(terminal: bool, rgb: (u8, u8, u8)) -> Color {
    if !terminal {
        return Color::Rgb(rgb.0, rgb.1, rgb.2);
    }
    match rgb {
        crate::theme::SENTINEL_COMMENT => Color::DarkGray,
        crate::theme::SENTINEL_STRING => Color::Green,
        crate::theme::SENTINEL_NUMBER => Color::Yellow,
        crate::theme::SENTINEL_KEYWORD => Color::Blue,
        crate::theme::SENTINEL_FUNCTION => Color::Cyan,
        crate::theme::SENTINEL_TYPE => Color::Magenta,
        crate::theme::SENTINEL_ATTRIBUTE => Color::LightCyan,
        crate::theme::SENTINEL_OPERATOR => Color::LightYellow,
        _ => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::Highlighter;
    use crate::theme;
    use ratatui::style::Color;

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
        assert!(spans.iter().any(|s| s.text == "let" && s.color != Color::Rgb(0xcd, 0xd6, 0xf4)));
    }

    #[test]
    fn unknown_language_is_one_plain_span_per_line() {
        let h = Highlighter::new(mocha());
        let lines = h.highlight("alpha\nbeta\n", None);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            vec![super::Span { text: "alpha".into(), color: Color::Rgb(0xcd, 0xd6, 0xf4) }]
        );
    }

    #[test]
    fn bundled_syntax_themes_all_parse() {
        // Each bundled `.tmTheme` must load, or highlighting silently degrades to plain spans
        // A loaded theme tokenizes rust into more than one span; a failed
        // load would yield a single plain span — so this guards the parse path for every
        // bundled theme, the only `SyntaxChoice` that can fail.
        for name in ["catppuccin", "tokyo-night", "tokyo-night-day", "rose-pine", "rose-pine-dawn"]
        {
            let h = Highlighter::new(theme::resolve(Some(name)).syntax);
            let spans = h.highlight("let x = 1;\n", Some("rs"));
            assert!(spans[0].len() > 1, "{name}: bundled syntax theme failed to load");
        }
    }

    #[test]
    fn terminal_syntax_maps_owned_sentinels_and_unknowns() {
        for (sentinel, expected) in [
            (theme::SENTINEL_COMMENT, Color::DarkGray),
            (theme::SENTINEL_STRING, Color::Green),
            (theme::SENTINEL_NUMBER, Color::Yellow),
            (theme::SENTINEL_KEYWORD, Color::Blue),
            (theme::SENTINEL_FUNCTION, Color::Cyan),
            (theme::SENTINEL_TYPE, Color::Magenta),
            (theme::SENTINEL_ATTRIBUTE, Color::LightCyan),
            (theme::SENTINEL_OPERATOR, Color::LightYellow),
        ] {
            assert_eq!(super::syntax_color(true, sentinel), expected);
        }
        assert_eq!(super::syntax_color(true, (0xaa, 0xbb, 0xcc)), Color::Reset);
    }

    #[test]
    fn terminal_highlighting_uses_no_rgb_for_known_and_unknown_languages() {
        let h = Highlighter::new(theme::resolve(Some("terminal")).syntax);
        for (content, language) in [
            ("fn main() { let n = 1; }\n", Some("rs")),
            ("def main():\n    return 1\n", Some("py")),
            ("plain\n", None),
        ] {
            let spans = h.highlight(content, language);
            assert!(
                spans
                    .iter()
                    .flatten()
                    .all(|s| !matches!(s.color, Color::Rgb(..) | Color::Indexed(..)))
            );
            if language.is_some() {
                assert!(spans.iter().flatten().any(|s| s.color != Color::Reset));
            }
        }
    }
}
