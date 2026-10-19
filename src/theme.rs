//! The theme catalog: each theme's primitives, paired syntax theme, and own fills, one selection
//! for both so they never desync; [`crate::roles`] derives every painted color from these.

// This file is a color table; 6-digit `0xRRGGBB` literals read better grouped as one value.
#![allow(clippy::unreadable_literal)]

use ratatui::style::Color;
use two_face::theme::EmbeddedThemeName;

use crate::roles::{Cast, Fill, Overrides, Palette, Primitives};

/// The default theme name; the fallback for an unset CLI value.
pub const DEFAULT: &str = "catppuccin";

/// Every built-in theme, dark first. Names match herdr's where both ship a palette, so the value a
/// user copies from their herdr config resolves to the same palette.
pub const NAMES: [&str; 20] = [
    "catppuccin",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "catppuccin-latte",
    "tokyo-night",
    "tokyo-night-day",
    "dracula",
    "nord",
    "gruvbox",
    "gruvbox-light",
    "one-dark",
    "one-light",
    "solarized",
    "solarized-light",
    "monokai",
    "rose-pine",
    "rose-pine-dawn",
    "ayu",
    "everforest",
    "github-light",
];

/// The syntax theme paired with a palette: a bundled `.tmTheme`'s vendored bytes (for themes
/// `two-face` lacks, and for Catppuccin Mocha), or a theme from the `two-face` embedded set.
#[derive(Clone, Copy, Debug)]
pub enum SyntaxChoice {
    Bundled(&'static [u8]),
    Embedded(EmbeddedThemeName),
}

/// A resolved theme: its name, the chrome `Palette`, and its paired syntax theme.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub palette: Palette,
    pub syntax: SyntaxChoice,
}

/// Resolve a theme name to a `Theme`. `None`, an unknown name, or a not-yet-supported
/// one (including `terminal`) falls back to the default and logs; never a half-palette.
pub fn resolve(name: Option<&str>) -> Theme {
    let name = name.unwrap_or(DEFAULT);
    build(name).unwrap_or_else(|| {
        logln!("unknown theme {name:?}; using {DEFAULT}");
        build(DEFAULT).expect("the default theme is built in")
    })
}

/// Whether `name` selects a built-in theme. Plugin configuration validates against this same
/// catalog on every frame, so it checks the name without deriving a palette.
pub fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

/// The built theme for `name`, or `None` when it is not a known palette.
fn build(name: &str) -> Option<Theme> {
    let name = *NAMES.iter().find(|n| **n == name)?;
    let (syntax, primitives, fills) = entry(name)?;
    Some(Theme { name, palette: Palette::derive(primitives, fills), syntax })
}

/// A theme's syntax pairing, primitives, and the fills it sets itself.
fn entry(name: &str) -> Option<(SyntaxChoice, Primitives, Overrides)> {
    use EmbeddedThemeName as E;
    use SyntaxChoice::{Bundled, Embedded};
    let none = Overrides::default();
    Some(match name {
        "catppuccin" => (Bundled(MOCHA_TM), MOCHA, catppuccin_fills()),
        "catppuccin-latte" => (Embedded(E::CatppuccinLatte), CATPPUCCIN_LATTE, none),
        "catppuccin-frappe" => (Embedded(E::CatppuccinFrappe), FRAPPE, none),
        "catppuccin-macchiato" => (Embedded(E::CatppuccinMacchiato), MACCHIATO, none),
        "dracula" => (Embedded(E::Dracula), DRACULA, none),
        "nord" => (Embedded(E::Nord), NORD, none),
        "gruvbox" => (Embedded(E::GruvboxDark), GRUVBOX, none),
        "gruvbox-light" => (Embedded(E::GruvboxLight), GRUVBOX_LIGHT, none),
        "one-dark" => (Embedded(E::TwoDark), ONE_DARK, none),
        "one-light" => (Embedded(E::OneHalfLight), ONE_LIGHT, none),
        "solarized" => (Embedded(E::SolarizedDark), SOLARIZED, none),
        "solarized-light" => (Embedded(E::SolarizedLight), SOLARIZED_LIGHT, none),
        "github-light" => (Embedded(E::Github), GITHUB_LIGHT, none),
        "monokai" => (Embedded(E::MonokaiExtended), MONOKAI, none),
        // `two-face` lacks these syntax themes, so they pair with a vendored `.tmTheme`.
        "tokyo-night" => (Bundled(TOKYO_NIGHT_TM), TOKYO_NIGHT, none),
        "tokyo-night-day" => (Bundled(TOKYO_NIGHT_DAY_TM), TOKYO_NIGHT_DAY, none),
        "rose-pine" => (Bundled(ROSE_PINE_TM), ROSE_PINE, none),
        "rose-pine-dawn" => (Bundled(ROSE_PINE_DAWN_TM), ROSE_PINE_DAWN, none),
        "ayu" => (Bundled(AYU_TM), AYU, none),
        "everforest" => (Bundled(EVERFOREST_TM), EVERFOREST, everforest_fills()),
        _ => return None,
    })
}

/// Catppuccin ships its own surfaces and diff fills, so they enter the roles as given.
fn catppuccin_fills() -> Overrides {
    Overrides::default()
        .fill(Fill::Bar, hex(0x313244))
        .fill(Fill::Code, hex(0x313244))
        .fill(Fill::CursorInactive, hex(0x45475a))
        .fill(Fill::Cursor, hex(0x585b70))
        .fill(Fill::Removed, hex(0x45232f))
        .fill(Fill::Added, hex(0x1f3a2a))
        .fill(Fill::RemovedEmph, hex(0x6e3446))
        .fill(Fill::AddedEmph, hex(0x30553f))
        .fill(Fill::Selection, hex(0x353d7d))
}

/// Everforest ships its diff row fills (`bg_green`, `bg_red`), so the rows match the Neovim
/// theme. It has no word-emphasis fills, so those stay derived.
fn everforest_fills() -> Overrides {
    Overrides::default().fill(Fill::Added, hex(0x3c4841)).fill(Fill::Removed, hex(0x493b40))
}

/// Vendored `.tmTheme` assets for the syntax themes `two-face` does not carry, and Mocha.
/// Licenses listed in the README's License section.
const MOCHA_TM: &[u8] = include_bytes!("../assets/Catppuccin Mocha.tmTheme");
const TOKYO_NIGHT_TM: &[u8] = include_bytes!("../assets/tokyo-night.tmTheme");
const TOKYO_NIGHT_DAY_TM: &[u8] = include_bytes!("../assets/tokyo-night-day.tmTheme");
const ROSE_PINE_TM: &[u8] = include_bytes!("../assets/rose-pine.tmTheme");
const ROSE_PINE_DAWN_TM: &[u8] = include_bytes!("../assets/rose-pine-dawn.tmTheme");
const AYU_TM: &[u8] = include_bytes!("../assets/ayu-dark.tmTheme");
const EVERFOREST_TM: &[u8] = include_bytes!("../assets/everforest.tmTheme");

/// Catppuccin Mocha: its canonical values, with herdr's blue as the accent.
const MOCHA: Primitives =
    dark(0x1e1e2e, 0xcdd6f4, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0xfab387, 0xcba6f7, 0xb4befe, 0x89b4fa);
const CATPPUCCIN_LATTE: Primitives =
    light(0xeff1f5, 0x4c4f69, 0xd20f39, 0x40a02b, 0xdf8e1d, 0xfe640b, 0x8839ef, 0x7287fd, 0x1e66f5);
/// The rest, by canonical values: base, text, red, green, yellow, orange, purple, blue, and
/// the UI accent.
const DRACULA: Primitives =
    dark(0x282a36, 0xf8f8f2, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xffb86c, 0xbd93f9, 0x8be9fd, 0xbd93f9);
const NORD: Primitives =
    dark(0x2e3440, 0xd8dee9, 0xbf616a, 0xa3be8c, 0xebcb8b, 0xd08770, 0xb48ead, 0x81a1c1, 0x88c0d0);
const GRUVBOX: Primitives =
    dark(0x282828, 0xebdbb2, 0xfb4934, 0xb8bb26, 0xfabd2f, 0xfe8019, 0xd3869b, 0x83a598, 0xd79921);
const GRUVBOX_LIGHT: Primitives =
    light(0xfbf1c7, 0x3c3836, 0x9d0006, 0x79740e, 0xb57614, 0xaf3a03, 0x8f3f71, 0x076678, 0x076678);
const ONE_DARK: Primitives =
    dark(0x282c34, 0xabb2bf, 0xe06c75, 0x98c379, 0xe5c07b, 0xd19a66, 0xc678dd, 0x61afef, 0x61afef);
const ONE_LIGHT: Primitives =
    light(0xfafafa, 0x383a42, 0xe45649, 0x50a14f, 0xc18401, 0x986801, 0xa626a4, 0x4078f2, 0x4078f2);
const SOLARIZED: Primitives =
    dark(0x002b36, 0x93a1a1, 0xdc322f, 0x859900, 0xb58900, 0xcb4b16, 0x6c71c4, 0x268bd2, 0x268bd2);
const SOLARIZED_LIGHT: Primitives =
    light(0xfdf6e3, 0x586e75, 0xdc322f, 0x859900, 0xb58900, 0xcb4b16, 0x6c71c4, 0x268bd2, 0x268bd2);
const FRAPPE: Primitives =
    dark(0x303446, 0xc6d0f5, 0xe78284, 0xa6d189, 0xe5c890, 0xef9f76, 0xca9ee6, 0xbabbf1, 0x8caaee);
const MACCHIATO: Primitives =
    dark(0x24273a, 0xcad3f5, 0xed8796, 0xa6da95, 0xeed49f, 0xf5a97f, 0xc6a0f6, 0xb7bdf8, 0x8aadf4);
const GITHUB_LIGHT: Primitives =
    light(0xffffff, 0x1f2328, 0xcf222e, 0x1a7f37, 0x9a6700, 0xbc4c00, 0x8250df, 0x0969da, 0x0969da);
const MONOKAI: Primitives =
    dark(0x272822, 0xf8f8f2, 0xf92672, 0xa6e22e, 0xe6db74, 0xfd971f, 0xae81ff, 0x66d9ef, 0x66d9ef);
const TOKYO_NIGHT: Primitives =
    dark(0x1a1b26, 0xc0caf5, 0xf7768e, 0x9ece6a, 0xe0af68, 0xff9e64, 0xbb9af7, 0x7aa2f7, 0x7aa2f7);
const TOKYO_NIGHT_DAY: Primitives =
    light(0xe1e2e7, 0x3760bf, 0xf52a65, 0x587539, 0x8c6c3e, 0xb15c00, 0x9854f1, 0x2e7de9, 0x2e7de9);
const ROSE_PINE: Primitives =
    dark(0x191724, 0xe0def4, 0xeb6f92, 0x9ccfd8, 0xf6c177, 0xebbcba, 0xc4a7e7, 0x31748f, 0xc4a7e7);
const ROSE_PINE_DAWN: Primitives =
    light(0xfaf4ed, 0x575279, 0xb4637a, 0x56949f, 0xea9d34, 0xd7827e, 0x907aa9, 0x286983, 0x907aa9);
/// ayu Dark from `ayu-colors` 9.1: `ui.bg`, `editor.fg`, its syntax palette, and its blue as the
/// accent, as herdr paints ayu: `common.accent` gold reads as "modified" yellow.
const AYU: Primitives =
    dark(0x0d1017, 0xbfbdb6, 0xf07178, 0xaad94c, 0xffb454, 0xff8f40, 0xd2a6ff, 0x59c2ff, 0x59c2ff);
/// Everforest dark, hard background: `bg0`, `fg` and the accents from `autoload/everforest.vim`.
/// The accent is its aqua: upstream's green accent is its "added" color.
const EVERFOREST: Primitives =
    dark(0x272e33, 0xd3c6aa, 0xe67e80, 0xa7c080, 0xdbbc7f, 0xe69875, 0xd699b6, 0x7fbbb3, 0x7fbbb3);

/// A dark theme's primitives from `0xRRGGBB` literals, so a palette reads as one compact row:
/// base, text, red, green, yellow, orange, purple, blue, accent.
#[allow(clippy::too_many_arguments)]
const fn dark(
    base: u32,
    text: u32,
    red: u32,
    green: u32,
    yellow: u32,
    orange: u32,
    purple: u32,
    blue: u32,
    accent: u32,
) -> Primitives {
    primitives(Cast::Dark, [base, text, red, green, yellow, orange, purple, blue, accent])
}

/// A light theme's primitives, in [`dark`]'s order.
#[allow(clippy::too_many_arguments)]
const fn light(
    base: u32,
    text: u32,
    red: u32,
    green: u32,
    yellow: u32,
    orange: u32,
    purple: u32,
    blue: u32,
    accent: u32,
) -> Primitives {
    primitives(Cast::Light, [base, text, red, green, yellow, orange, purple, blue, accent])
}

const fn primitives(cast: Cast, c: [u32; 9]) -> Primitives {
    Primitives {
        base: hex(c[0]),
        text: hex(c[1]),
        red: hex(c[2]),
        green: hex(c[3]),
        yellow: hex(c[4]),
        orange: hex(c[5]),
        purple: hex(c[6]),
        blue: hex(c[7]),
        accent: hex(c[8]),
        cast,
    }
}

/// A `Color::Rgb` from a `0xRRGGBB` literal.
const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

#[cfg(test)]
mod tests {
    use super::{NAMES, entry, resolve};
    use crate::roles::{
        Cast, FILL_SEP, FILLS, Fill, INK_SEP, INKS, Ink, LAYERS, MARK_FLOOR, TEXT_FLOOR, TIER_STEP,
        contrast, oklab_distance,
    };
    use ratatui::style::Color;

    fn rgb(hex: u32) -> Color {
        Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }

    #[test]
    fn catppuccin_keeps_its_own_surfaces_and_fills() {
        let p = resolve(Some("catppuccin")).palette;
        assert_eq!(p.fill(Fill::Bar), Color::Rgb(0x31, 0x32, 0x44));
        assert_eq!(p.fill(Fill::Cursor), Color::Rgb(0x58, 0x5b, 0x70));
        assert_eq!(p.fill(Fill::Removed), Color::Rgb(0x45, 0x23, 0x2f));
        assert_eq!(p.fill(Fill::Added), Color::Rgb(0x1f, 0x3a, 0x2a));
        assert_eq!(p.fill(Fill::Selection), Color::Rgb(0x35, 0x3d, 0x7d));
        assert_eq!(p.ink(Ink::Text, Fill::Base), Color::Rgb(0xcd, 0xd6, 0xf4));
        assert_eq!(p.ink(Ink::Comment, Fill::Base), Color::Rgb(0xfa, 0xb3, 0x87));
    }

    #[test]
    fn everforest_diff_rows_use_its_own_palette_fills() {
        let p = resolve(Some("everforest")).palette;
        assert_eq!(p.fill(Fill::Added), Color::Rgb(0x3c, 0x48, 0x41));
        assert_eq!(p.fill(Fill::Removed), Color::Rgb(0x49, 0x3b, 0x40));
        // Everything else still comes from its primitives.
        assert_eq!(p.fill(Fill::Base), Color::Rgb(0x27, 0x2e, 0x33));
        assert_eq!(p.ink(Ink::Text, Fill::Base), Color::Rgb(0xd3, 0xc6, 0xaa));
    }

    #[test]
    fn unknown_and_terminal_fall_back_to_default() {
        assert_eq!(resolve(Some("nope")).name, "catppuccin");
        assert_eq!(resolve(Some("terminal")).name, "catppuccin");
        assert_eq!(resolve(None).name, "catppuccin");
        assert!(!super::is_known("terminal"));
        assert!(NAMES.iter().all(|name| super::is_known(name)));
    }

    #[test]
    fn a_light_theme_steps_its_surfaces_darker() {
        let p = resolve(Some("catppuccin-latte")).palette;
        let lum = |c| contrast(c, Color::Rgb(0, 0, 0));
        assert!(lum(p.fill(Fill::Bar)) < lum(p.fill(Fill::Base)), "the bar is darker than base");
        assert!(lum(p.fill(Fill::Cursor)) < lum(p.fill(Fill::Bar)), "the ramp keeps darkening");
        // The row under the pointer sits between the bar and the cursor, never off the ramp.
        assert!(
            lum(p.fill(Fill::Hover)) < lum(p.fill(Fill::Bar))
                && lum(p.fill(Fill::Hover)) > lum(p.fill(Fill::Cursor)),
            "the hover row is not between the bar and the cursor"
        );
    }

    /// The row under the pointer always reads between the background and the cursor row.
    #[test]
    fn the_hover_row_sits_between_the_page_and_the_cursor() {
        let lum = |c| contrast(c, Color::Rgb(0, 0, 0));
        for name in NAMES {
            let p = resolve(Some(name)).palette;
            let (page, hover, cursor) =
                (lum(p.fill(Fill::Base)), lum(p.fill(Fill::Hover)), lum(p.fill(Fill::Cursor)));
            let (low, high) = if page < cursor { (page, cursor) } else { (cursor, page) };
            assert!(
                hover > low && hover < high,
                "{name}: hover {hover:.3} is not between the page {page:.3} and the cursor {cursor:.3}"
            );
        }
    }

    #[test]
    fn a_color_already_legible_on_the_fill_is_untouched() {
        let p = resolve(Some("catppuccin")).palette;
        let bright = Color::Rgb(0xf0, 0xf0, 0xf0);
        assert_eq!(p.legible(bright, Fill::AddedEmph), bright);
    }

    /// A text tier resolved on the background resolves as that tier on a fill: rendered
    /// markdown's muted and secondary text and its rules keep their order on the cursor row.
    #[test]
    fn a_text_tier_resolves_as_its_tier_on_a_fill() {
        for name in NAMES {
            let p = resolve(Some(name)).palette;
            for ink in [Ink::Text, Ink::TextSecondary, Ink::TextMuted, Ink::Border] {
                let on_base = p.ink(ink, Fill::Base);
                assert_eq!(p.legible(on_base, Fill::Cursor), p.ink(ink, Fill::Cursor), "{name}");
            }
        }
    }

    #[test]
    fn distinct_syntax_colors_stay_distinct_on_floor_hugging_themes() {
        // These themes keep their fills just above the floor for `text`; lifting every color to
        // that floor would paint them all as `text`.
        let (comment, keyword) = (Color::Rgb(0x56, 0x5f, 0x89), Color::Rgb(0x9d, 0x7c, 0xd8));
        for name in ["tokyo-night-day", "solarized", "tokyo-night"] {
            let p = resolve(Some(name)).palette;
            for on in [Fill::RemovedEmph, Fill::AddedEmph, Fill::Cursor] {
                let (a, b) = (p.legible(comment, on), p.legible(keyword, on));
                assert_ne!(a, b, "{name}: two syntax colors merged on {on:?}");
                assert_ne!(a, p.ink(Ink::Text, on), "{name}: the comment lost its hue on {on:?}");
            }
        }
    }

    /// Each theme's accent, comment and merged colors on the background, lifted to 3:1; where two
    /// read alike, the later takes the theme's next hue.
    #[test]
    fn every_theme_paints_its_accent_comment_and_merged() {
        let painted: [(&str, u32, u32, u32); 20] = [
            ("catppuccin", 0x89b4fa, 0xfab387, 0xcba6f7),
            ("catppuccin-frappe", 0x8caaee, 0xef9f76, 0xca9ee6),
            ("catppuccin-macchiato", 0x8aadf4, 0xf5a97f, 0xc6a0f6),
            ("catppuccin-latte", 0x1e66f5, 0xee5c00, 0x8839ef),
            ("tokyo-night", 0x7aa2f7, 0xff9e64, 0xbb9af7),
            ("tokyo-night-day", 0x2e7de9, 0xb15c00, 0x9854f1),
            ("dracula", 0xbd93f9, 0xffb86c, 0x8be9fd),
            ("nord", 0x88c0d0, 0x81a1c1, 0xb48ead),
            ("gruvbox", 0xd79921, 0xfe8019, 0xd3869b),
            ("gruvbox-light", 0x076678, 0xaf3a03, 0x8f3f71),
            ("one-dark", 0x61afef, 0xd19a66, 0xc678dd),
            ("one-light", 0x4078f2, 0xa626a4, 0x986801),
            ("solarized", 0x268bd2, 0xcb4b16, 0x6c71c4),
            ("solarized-light", 0x268bd2, 0xcb4b16, 0x6c71c4),
            ("monokai", 0x66d9ef, 0xfd971f, 0xae81ff),
            ("rose-pine", 0xc4a7e7, 0xebbcba, 0x31748f),
            ("rose-pine-dawn", 0x907aa9, 0x286983, 0xca7773),
            ("ayu", 0x59c2ff, 0xff8f40, 0xd2a6ff),
            ("everforest", 0x7fbbb3, 0xe69875, 0xd699b6),
            ("github-light", 0x0969da, 0xbc4c00, 0x8250df),
        ];
        assert_eq!(painted.map(|row| row.0), NAMES, "every theme, in catalog order");
        for (name, accent, comment, merged) in painted {
            let p = resolve(Some(name)).palette;
            let paints = |ink| p.mark(ink, Fill::Base);
            assert_eq!(paints(Ink::Accent), rgb(accent), "{name} accent");
            assert_eq!(paints(Ink::Comment), rgb(comment), "{name} comment");
            assert_eq!(paints(Ink::Merged), rgb(merged), "{name} merged");
        }
    }

    /// The color guarantees, measured on every theme, role and fill. Collects every failure
    /// before asserting, so one run shows the whole picture.
    #[test]
    fn every_role_reads_on_every_fill() {
        let mut failures = Vec::new();
        for name in NAMES {
            let theme = resolve(Some(name));
            let p = theme.palette;
            let base = p.fill(Fill::Base);
            // The theme's real syntax colors, from a sample through its own syntax theme.
            let syntax: Vec<Color> = crate::highlight::Highlighter::new(theme.syntax)
                .highlight(SAMPLE, Some("rs"))
                .into_iter()
                .flatten()
                .map(|span| Color::Rgb(span.color.0, span.color.1, span.color.2))
                .collect();
            for on in FILLS {
                let bg = p.fill(on);
                // A match and the caret repaint their text in `text`, so no syntax color sits
                // on them, and only `text` paints there.
                let solid = matches!(on, Fill::Highlight | Fill::Caret);
                // Syntax colors keep their plain-background legibility on every fill.
                for &c in syntax.iter().filter(|_| !solid) {
                    let want = contrast(c, base).min(TEXT_FLOOR) - 0.05;
                    let got = contrast(p.legible(c, on), bg);
                    if got < want {
                        failures
                            .push(format!("{name}: syntax {c:?} on {on:?} {got:.2} < {want:.2}"));
                    }
                }
                // Text clears its floor, muted text and every glyph theirs.
                for ink in INKS {
                    let floor = match ink {
                        Ink::TextMuted | Ink::Border => MARK_FLOOR,
                        _ => TEXT_FLOOR,
                    };
                    let c = contrast(p.ink(ink, on), bg);
                    if c < floor - 0.005 {
                        failures.push(format!("{name}: {ink:?} text on {on:?} {c:.2} < {floor}"));
                    }
                    let m = contrast(p.mark(ink, on), bg);
                    if m < MARK_FLOOR - 0.005 {
                        failures.push(format!("{name}: {ink:?} mark on {on:?} {m:.2} < 3"));
                    }
                }
                // The text tiers keep their order and a visible step.
                let tier = |ink| contrast(p.ink(ink, on), bg);
                let (t, s, m) = (tier(Ink::Text), tier(Ink::TextSecondary), tier(Ink::TextMuted));
                let ordered = t >= s * TIER_STEP - 0.01 && s >= m * TIER_STEP - 0.01;
                if !solid && !ordered {
                    failures.push(format!("{name}: tiers on {on:?} {t:.2} / {s:.2} / {m:.2}"));
                }
            }
            // Every fill reads as a layer over every fill in a lower layer.
            for (depth, layer) in LAYERS.iter().enumerate() {
                for &top in *layer {
                    for &under in LAYERS[..depth].iter().flat_map(|l| l.iter()) {
                        let d = oklab_distance(p.fill(top), p.fill(under));
                        if d < FILL_SEP - 0.0005 {
                            failures.push(format!("{name}: {top:?} over {under:?} ΔE {d:.3}"));
                        }
                    }
                }
            }
            // Inks that appear side by side read as different.
            let beside: &[(Ink, &[Ink])] = &[
                (
                    Ink::Accent,
                    &[Ink::Added, Ink::Removed, Ink::Modified, Ink::Comment, Ink::Merged],
                ),
                (Ink::Comment, &[Ink::Added, Ink::Removed, Ink::Modified, Ink::Merged]),
            ];
            for &(a, others) in beside {
                for &b in others {
                    let d = oklab_distance(p.ink(a, Fill::Base), p.ink(b, Fill::Base));
                    if d < INK_SEP - 0.0005 {
                        failures.push(format!("{name}: {a:?} beside {b:?} ΔE {d:.3}"));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
    }

    /// A Rust sample with comments, strings, numbers, keywords and calls: most of a syntax
    /// theme's colors.
    const SAMPLE: &str = "// Totals one order.\nfn total(items: &[Item]) -> u64 {\n    let tax = 0.2; /* rate */\n    items.iter().map(|i| i.price * 2).sum::<u64>() + \"x\".len() as u64\n}\n#[derive(Debug)]\npub struct Item { pub price: u64 }\n";

    /// A match is a solid block of the theme's yellow and the caret one of its accent, each
    /// found at a glance; text on them reads in every theme.
    #[test]
    fn a_match_and_the_caret_are_solid_blocks_text_reads_on() {
        for name in NAMES {
            let p = resolve(Some(name)).palette;
            let (_, primitives, _) = entry(name).unwrap();
            assert_eq!(p.fill(Fill::Highlight), primitives.yellow, "{name}: the match is yellow");
            assert_eq!(p.fill(Fill::Caret), p.mark(Ink::Accent, Fill::Base), "{name}: caret");
            for on in [Fill::Highlight, Fill::Caret] {
                let c = contrast(p.ink(Ink::Text, on), p.fill(on));
                assert!(c >= TEXT_FLOOR, "{name}: text on {on:?} {c:.2} < {TEXT_FLOOR}");
            }
        }
    }

    #[test]
    fn every_named_theme_resolves_to_itself() {
        for name in NAMES {
            assert_eq!(resolve(Some(name)).name, name, "{name} should resolve to its own palette");
        }
    }

    #[test]
    fn appearance_orients_text_against_surface() {
        for name in NAMES {
            let p = resolve(Some(name)).palette;
            let light = entry(name).unwrap().1.cast == Cast::Light;
            // Light theme: dark text on a lighter surface. Dark theme: the reverse.
            let dark = Color::Rgb(0, 0, 0);
            let (text, bar) = (p.ink(Ink::Text, Fill::Base), p.fill(Fill::Bar));
            let text_darker = contrast(text, dark) < contrast(bar, dark);
            assert_eq!(text_darker, light, "{name}: text/surface contrast points the wrong way");
        }
    }
}
