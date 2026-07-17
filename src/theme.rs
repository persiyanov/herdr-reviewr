//! The color model: named palettes, derivation from anchors, and selection.
//!
//! See `specs/theme.md`. A theme is a few anchor colors plus a paired syntax theme;
//! every other slot is derived from the anchors. One theme — `catppuccin` — instead
//! pins its whole palette as a literal, to stay byte-identical to the pre-theming
//! colors. One selection sets both the chrome `Palette` and the syntax theme, so they
//! never desync. The pane background stays the terminal's, so only these fills and the
//! syntax foregrounds are painted.

// This file is a color table; 6-digit `0xRRGGBB` literals read better grouped as one value.
#![allow(clippy::unreadable_literal)]

use ratatui::style::Color;
use two_face::theme::EmbeddedThemeName;

/// The default theme name; the fallback for an unset CLI value.
pub const DEFAULT: &str = "catppuccin";

/// A theme's intrinsic cast, which sets the derivation direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// The syntax theme paired with a palette: a bundled `.tmTheme`'s vendored bytes (for themes
/// `two-face` lacks, and for Catppuccin Mocha kept byte-identical to today's), or a theme
/// from the `two-face` embedded set.
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

/// The resolved colors every UI element paints — one source for chrome and diff fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub surface0: Color,
    pub surface1: Color,
    pub surface2: Color,
    pub overlay0: Color,
    pub overlay1: Color,
    pub subtext0: Color,
    pub text: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub peach: Color,
    pub mauve: Color,
    pub lavender: Color,
    pub del_bg: Color,
    pub ins_bg: Color,
    pub emph_del_bg: Color,
    pub emph_ins_bg: Color,
}

impl Palette {
    /// The cursor-row fill: the strongest-contrast surface (`surface2`) in the focused pane, a
    /// step softer (`surface1`) when not, so which pane holds the cursor reads at a glance.
    /// ("Strongest", not "brightest": light themes step surfaces toward black, not white.)
    pub fn cursor_bg(&self, focused: bool) -> Color {
        if focused { self.surface2 } else { self.surface1 }
    }
}

/// Resolve a theme name to a `Theme`. `None`, an unknown name, or a not-yet-supported
/// one (including `terminal`) falls back to the default and logs; never a half-palette.
pub fn resolve(name: Option<&str>) -> Theme {
    match name {
        None => catppuccin(),
        Some(n) => build(n).unwrap_or_else(|| {
            logln!("unknown theme {n:?}; using {DEFAULT}");
            catppuccin()
        }),
    }
}

/// Whether `name` selects a complete built-in theme. Plugin configuration validates against
/// this same catalog before a snapshot is applied (`specs/config.md`).
pub fn is_known(name: &str) -> bool {
    build(name).is_some()
}

/// The built theme for `name`, or `None` when it is not a known palette. Names match herdr's
/// so the value a user copies from their herdr config resolves to the same palette.
fn build(name: &str) -> Option<Theme> {
    use Appearance::{Dark, Light};
    use EmbeddedThemeName as E;
    Some(match name {
        "catppuccin" => catppuccin(),
        "catppuccin-latte" => catppuccin_latte(),
        "dracula" => derived("dracula", Dark, E::Dracula, DRACULA),
        "nord" => derived("nord", Dark, E::Nord, NORD),
        "gruvbox" => derived("gruvbox", Dark, E::GruvboxDark, GRUVBOX),
        "gruvbox-light" => derived("gruvbox-light", Light, E::GruvboxLight, GRUVBOX_LIGHT),
        "one-dark" => derived("one-dark", Dark, E::TwoDark, ONE_DARK),
        "one-light" => derived("one-light", Light, E::OneHalfLight, ONE_LIGHT),
        "solarized" => derived("solarized", Dark, E::SolarizedDark, SOLARIZED),
        "solarized-light" => derived("solarized-light", Light, E::SolarizedLight, SOLARIZED_LIGHT),
        // Popular themes beyond herdr's set, whose syntax `two-face` already provides.
        "catppuccin-frappe" => derived("catppuccin-frappe", Dark, E::CatppuccinFrappe, FRAPPE),
        "catppuccin-macchiato" => {
            derived("catppuccin-macchiato", Dark, E::CatppuccinMacchiato, MACCHIATO)
        }
        "github-light" => derived("github-light", Light, E::Github, GITHUB_LIGHT),
        "monokai" => derived("monokai", Dark, E::MonokaiExtended, MONOKAI),
        // herdr names whose syntax `two-face` lacks, paired with a vendored `.tmTheme`.
        "tokyo-night" => bundled("tokyo-night", Dark, TOKYO_NIGHT_TM, TOKYO_NIGHT),
        "tokyo-night-day" => bundled("tokyo-night-day", Light, TOKYO_NIGHT_DAY_TM, TOKYO_NIGHT_DAY),
        "rose-pine" => bundled("rose-pine", Dark, ROSE_PINE_TM, ROSE_PINE),
        "rose-pine-dawn" => bundled("rose-pine-dawn", Light, ROSE_PINE_DAWN_TM, ROSE_PINE_DAWN),
        _ => return None,
    })
}

/// A derived theme: its palette is computed from `anchors`, paired with a `two-face` syntax theme.
fn derived(
    name: &'static str,
    appearance: Appearance,
    syntax: EmbeddedThemeName,
    anchors: Anchors,
) -> Theme {
    Theme { name, palette: derive(anchors, appearance), syntax: SyntaxChoice::Embedded(syntax) }
}

/// The anchor colors a derived theme lists; the rest of its palette is computed from these.
#[derive(Clone, Copy, Debug)]
struct Anchors {
    base: Color,
    text: Color,
    red: Color,
    green: Color,
    yellow: Color,
    peach: Color,
    mauve: Color,
    lavender: Color,
}

/// Catppuccin Mocha: pinned to its canonical values so it renders identically to the
/// pre-theming palette.
fn catppuccin() -> Theme {
    Theme {
        name: "catppuccin",
        palette: Palette {
            surface0: Color::Rgb(0x31, 0x32, 0x44),
            surface1: Color::Rgb(0x45, 0x47, 0x5a),
            surface2: Color::Rgb(0x58, 0x5b, 0x70),
            overlay0: Color::Rgb(0x6c, 0x70, 0x86),
            overlay1: Color::Rgb(0x7f, 0x84, 0x9c),
            subtext0: Color::Rgb(0xa6, 0xad, 0xc8),
            text: Color::Rgb(0xcd, 0xd6, 0xf4),
            red: Color::Rgb(0xf3, 0x8b, 0xa8),
            green: Color::Rgb(0xa6, 0xe3, 0xa1),
            yellow: Color::Rgb(0xf9, 0xe2, 0xaf),
            peach: Color::Rgb(0xfa, 0xb3, 0x87),
            mauve: Color::Rgb(0xcb, 0xa6, 0xf7),
            lavender: Color::Rgb(0xb4, 0xbe, 0xfe),
            del_bg: Color::Rgb(0x45, 0x23, 0x2f),
            ins_bg: Color::Rgb(0x1f, 0x3a, 0x2a),
            emph_del_bg: Color::Rgb(0x6e, 0x34, 0x46),
            emph_ins_bg: Color::Rgb(0x30, 0x55, 0x3f),
        },
        syntax: SyntaxChoice::Bundled(MOCHA_TM),
    }
}

/// A theme whose palette is derived from `anchors`, paired with a bundled `.tmTheme`'s bytes.
fn bundled(
    name: &'static str,
    appearance: Appearance,
    syntax: &'static [u8],
    anchors: Anchors,
) -> Theme {
    Theme { name, palette: derive(anchors, appearance), syntax: SyntaxChoice::Bundled(syntax) }
}

/// Vendored `.tmTheme` assets for the syntax themes `two-face` does not carry (and Mocha,
/// kept as the byte-identical source of today's highlighting). Licenses listed in the
/// README's License section.
const MOCHA_TM: &[u8] = include_bytes!("../assets/Catppuccin Mocha.tmTheme");
const TOKYO_NIGHT_TM: &[u8] = include_bytes!("../assets/tokyo-night.tmTheme");
const TOKYO_NIGHT_DAY_TM: &[u8] = include_bytes!("../assets/tokyo-night-day.tmTheme");
const ROSE_PINE_TM: &[u8] = include_bytes!("../assets/rose-pine.tmTheme");
const ROSE_PINE_DAWN_TM: &[u8] = include_bytes!("../assets/rose-pine-dawn.tmTheme");

/// Catppuccin Latte: a light theme, derived from its anchors to exercise the derivation
/// path (and paired with `two-face`'s Latte syntax theme).
fn catppuccin_latte() -> Theme {
    derived(
        "catppuccin-latte",
        Appearance::Light,
        EmbeddedThemeName::CatppuccinLatte,
        CATPPUCCIN_LATTE,
    )
}

const CATPPUCCIN_LATTE: Anchors =
    anchors(0xeff1f5, 0x4c4f69, 0xd20f39, 0x40a02b, 0xdf8e1d, 0xfe640b, 0x8839ef, 0x7287fd);

/// Canonical anchors for the derived themes. base, text, then the six accents
/// (red, green, yellow, peach, mauve, lavender); surfaces and diff fills are derived.
const DRACULA: Anchors =
    anchors(0x282a36, 0xf8f8f2, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xffb86c, 0xbd93f9, 0x8be9fd);
const NORD: Anchors =
    anchors(0x2e3440, 0xd8dee9, 0xbf616a, 0xa3be8c, 0xebcb8b, 0xd08770, 0xb48ead, 0x81a1c1);
const GRUVBOX: Anchors =
    anchors(0x282828, 0xebdbb2, 0xfb4934, 0xb8bb26, 0xfabd2f, 0xfe8019, 0xd3869b, 0x83a598);
const GRUVBOX_LIGHT: Anchors =
    anchors(0xfbf1c7, 0x3c3836, 0x9d0006, 0x79740e, 0xb57614, 0xaf3a03, 0x8f3f71, 0x076678);
const ONE_DARK: Anchors =
    anchors(0x282c34, 0xabb2bf, 0xe06c75, 0x98c379, 0xe5c07b, 0xd19a66, 0xc678dd, 0x61afef);
const ONE_LIGHT: Anchors =
    anchors(0xfafafa, 0x383a42, 0xe45649, 0x50a14f, 0xc18401, 0x986801, 0xa626a4, 0x4078f2);
const SOLARIZED: Anchors =
    anchors(0x002b36, 0x93a1a1, 0xdc322f, 0x859900, 0xb58900, 0xcb4b16, 0x6c71c4, 0x268bd2);
const SOLARIZED_LIGHT: Anchors =
    anchors(0xfdf6e3, 0x586e75, 0xdc322f, 0x859900, 0xb58900, 0xcb4b16, 0x6c71c4, 0x268bd2);
const FRAPPE: Anchors =
    anchors(0x303446, 0xc6d0f5, 0xe78284, 0xa6d189, 0xe5c890, 0xef9f76, 0xca9ee6, 0xbabbf1);
const MACCHIATO: Anchors =
    anchors(0x24273a, 0xcad3f5, 0xed8796, 0xa6da95, 0xeed49f, 0xf5a97f, 0xc6a0f6, 0xb7bdf8);
const GITHUB_LIGHT: Anchors =
    anchors(0xffffff, 0x1f2328, 0xcf222e, 0x1a7f37, 0x9a6700, 0xbc4c00, 0x8250df, 0x0969da);
const MONOKAI: Anchors =
    anchors(0x272822, 0xf8f8f2, 0xf92672, 0xa6e22e, 0xe6db74, 0xfd971f, 0xae81ff, 0x66d9ef);
const TOKYO_NIGHT: Anchors =
    anchors(0x1a1b26, 0xc0caf5, 0xf7768e, 0x9ece6a, 0xe0af68, 0xff9e64, 0xbb9af7, 0x7aa2f7);
const TOKYO_NIGHT_DAY: Anchors =
    anchors(0xe1e2e7, 0x3760bf, 0xf52a65, 0x587539, 0x8c6c3e, 0xb15c00, 0x9854f1, 0x2e7de9);
const ROSE_PINE: Anchors =
    anchors(0x191724, 0xe0def4, 0xeb6f92, 0x9ccfd8, 0xf6c177, 0xebbcba, 0xc4a7e7, 0x31748f);
const ROSE_PINE_DAWN: Anchors =
    anchors(0xfaf4ed, 0x575279, 0xb4637a, 0x56949f, 0xea9d34, 0xd7827e, 0x907aa9, 0x286983);

/// Build `Anchors` from `0xRRGGBB` hex literals, so a palette reads as one compact row.
/// One argument per anchor slot — the count is the palette's shape, not accidental.
#[allow(clippy::too_many_arguments)]
const fn anchors(
    base: u32,
    text: u32,
    red: u32,
    green: u32,
    yellow: u32,
    peach: u32,
    mauve: u32,
    lavender: u32,
) -> Anchors {
    Anchors {
        base: hex(base),
        text: hex(text),
        red: hex(red),
        green: hex(green),
        yellow: hex(yellow),
        peach: hex(peach),
        mauve: hex(mauve),
        lavender: hex(lavender),
    }
}

/// A `Color::Rgb` from a `0xRRGGBB` literal.
const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Build a full palette from anchors: surfaces step `base` toward the contrast pole
/// (lighter for a dark theme, darker for a light one); diff fills tint `base` with the
/// add/remove accent, kept legible against `text`.
fn derive(a: Anchors, appearance: Appearance) -> Palette {
    let pole = match appearance {
        Appearance::Dark => WHITE,
        Appearance::Light => BLACK,
    };
    let surface = |t: f64| blend(a.base, pole, t);
    Palette {
        surface0: surface(0.045),
        surface1: surface(0.09),
        surface2: surface(0.14),
        overlay0: surface(0.26),
        overlay1: surface(0.34),
        subtext0: blend(a.text, a.base, 0.18),
        text: a.text,
        red: a.red,
        green: a.green,
        yellow: a.yellow,
        peach: a.peach,
        mauve: a.mauve,
        lavender: a.lavender,
        del_bg: readable_tint(a.red, a.base, a.text, appearance, false),
        ins_bg: readable_tint(a.green, a.base, a.text, appearance, false),
        emph_del_bg: readable_tint(a.red, a.base, a.text, appearance, true),
        emph_ins_bg: readable_tint(a.green, a.base, a.text, appearance, true),
    }
}

const WHITE: Color = Color::Rgb(0xff, 0xff, 0xff);
const BLACK: Color = Color::Rgb(0x00, 0x00, 0x00);

/// The lowest contrast a diff fill keeps against the row's text, so code on a fill stays
/// legible on any base.
const MIN_FILL_CONTRAST: f64 = 4.5;

/// A diff-row fill: tint `base` with `accent`, stepping the tint down from its start strength
/// until the row's `fg` clears [`MIN_FILL_CONTRAST`]. `strong` is the brighter word-emphasis
/// fill. When even a faint tint can't clear the floor (a light theme with light text), the
/// bare `base` wins — legibility over a visible tint.
fn readable_tint(
    accent: Color,
    base: Color,
    fg: Color,
    appearance: Appearance,
    strong: bool,
) -> Color {
    let start = match (appearance, strong) {
        (Appearance::Dark, false) => 0.20,
        (Appearance::Dark, true) => 0.38,
        (Appearance::Light, false) => 0.12,
        (Appearance::Light, true) => 0.22,
    };
    let mut t = start;
    while t > 0.0 {
        let fill = blend(base, accent, t);
        if contrast(fg, fill) >= MIN_FILL_CONTRAST {
            return fill;
        }
        t -= 0.02;
    }
    base
}

/// Linear per-channel blend: `t` of the way from `from` to `to` (0.0 = `from`, 1.0 = `to`).
fn blend(from: Color, to: Color, t: f64) -> Color {
    let (fr, fg, fb) = channels(from);
    let (tr, tg, tb) = channels(to);
    let mix = |lhs: u8, rhs: u8| (f64::from(lhs) * (1.0 - t) + f64::from(rhs) * t).round() as u8;
    Color::Rgb(mix(fr, tr), mix(fg, tg), mix(fb, tb))
}

/// The WCAG contrast ratio between two colors (1.0 .. 21.0).
fn contrast(fg: Color, bg: Color) -> f64 {
    let (lf, lb) = (luminance(fg), luminance(bg));
    let (hi, lo) = if lf >= lb { (lf, lb) } else { (lb, lf) };
    (hi + 0.05) / (lo + 0.05)
}

/// WCAG relative luminance, with sRGB linearization.
fn luminance(color: Color) -> f64 {
    let (r, g, b) = channels(color);
    let lin = |channel: u8| {
        let srgb = f64::from(channel) / 255.0;
        if srgb <= 0.03928 { srgb / 12.92 } else { ((srgb + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// The RGB channels of a color; anchors are always `Rgb`, so the fallback never fires.
fn channels(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Appearance, CATPPUCCIN_LATTE, MIN_FILL_CONTRAST, Palette, contrast, derive, resolve,
    };
    use ratatui::style::Color;

    #[test]
    fn contrast_black_white_is_max() {
        let r = contrast(Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255));
        assert!((r - 21.0).abs() < 0.01, "black vs white is ~21:1, got {r}");
    }

    #[test]
    fn catppuccin_is_the_unchanged_mocha_palette() {
        let p = resolve(Some("catppuccin")).palette;
        assert_eq!(p.surface0, Color::Rgb(0x31, 0x32, 0x44));
        assert_eq!(p.text, Color::Rgb(0xcd, 0xd6, 0xf4));
        assert_eq!(p.del_bg, Color::Rgb(0x45, 0x23, 0x2f));
        assert_eq!(p.ins_bg, Color::Rgb(0x1f, 0x3a, 0x2a));
    }

    #[test]
    fn unknown_and_terminal_fall_back_to_default() {
        assert_eq!(resolve(Some("nope")).name, "catppuccin");
        assert_eq!(resolve(Some("terminal")).name, "catppuccin");
        assert_eq!(resolve(None).name, "catppuccin");
    }

    #[test]
    fn latte_is_a_selectable_light_theme() {
        assert_eq!(resolve(Some("catppuccin-latte")).name, "catppuccin-latte");
    }

    #[test]
    fn light_derivation_keeps_diff_fills_legible() {
        // Exercise the shipped catppuccin-latte anchors, so a real retune that breaks the
        // contrast floor or the surface ramp is caught here.
        let anchors = CATPPUCCIN_LATTE;
        let p: Palette = derive(anchors, Appearance::Light);
        // Text stays readable on every derived fill, on a light base.
        for fill in [p.del_bg, p.ins_bg, p.emph_del_bg, p.emph_ins_bg] {
            assert!(
                contrast(p.text, fill) >= MIN_FILL_CONTRAST,
                "fill {fill:?} drops below the legibility floor",
            );
        }
        // A light theme steps its surfaces darker than the base, deepening along the ramp, so
        // the fills read against the light canvas.
        let base_lum = super::luminance(anchors.base);
        assert!(super::luminance(p.surface0) < base_lum, "surface0 is darker than the base");
        assert!(
            super::luminance(p.surface2) < super::luminance(p.surface0),
            "the surface ramp keeps darkening",
        );
    }

    /// Every named theme and its appearance (`true` = light).
    const NAMED: &[(&str, bool)] = &[
        ("catppuccin", false),
        ("catppuccin-latte", true),
        ("dracula", false),
        ("nord", false),
        ("gruvbox", false),
        ("gruvbox-light", true),
        ("one-dark", false),
        ("one-light", true),
        ("solarized", false),
        ("solarized-light", true),
        ("catppuccin-frappe", false),
        ("catppuccin-macchiato", false),
        ("github-light", true),
        ("monokai", false),
        ("tokyo-night", false),
        ("tokyo-night-day", true),
        ("rose-pine", false),
        ("rose-pine-dawn", true),
    ];

    #[test]
    fn every_named_theme_resolves_to_itself() {
        for &(name, _) in NAMED {
            assert_eq!(resolve(Some(name)).name, name, "{name} should resolve to its own palette");
        }
    }

    #[test]
    fn every_theme_keeps_diff_fills_legible() {
        for &(name, _) in NAMED {
            let p = resolve(Some(name)).palette;
            for fill in [p.del_bg, p.ins_bg, p.emph_del_bg, p.emph_ins_bg] {
                assert!(
                    contrast(p.text, fill) >= MIN_FILL_CONTRAST,
                    "{name}: fill {fill:?} drops below the legibility floor",
                );
            }
        }
    }

    #[test]
    fn appearance_orients_text_against_surface() {
        for &(name, light) in NAMED {
            let p = resolve(Some(name)).palette;
            // Light theme: dark text on a lighter surface. Dark theme: the reverse.
            let text_darker = super::luminance(p.text) < super::luminance(p.surface0);
            assert_eq!(text_darker, light, "{name}: text/surface contrast points the wrong way");
        }
    }
}
