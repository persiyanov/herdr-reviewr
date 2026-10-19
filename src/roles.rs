//! Color roles: a theme's [`Primitives`] derive every [`Fill`] and every [`Ink`], each ink
//! resolved per fill it sits on, so components never see a primitive.

use std::cell::RefCell;
use std::collections::HashMap;

use palette::color_difference::{EuclideanDistance, Wcag21RelativeContrast};
use palette::convert::IntoColorUnclamped;
use palette::{Clamp, IntoColor, IsWithinBounds, LinSrgb, Oklab, Srgb};
use ratatui::style::Color;

/// A color text or a glyph paints with, by what it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    /// Body text, file names, the footer's status message.
    Text,
    /// Labels, inactive tabs, footer labels, other people's PR comment authors.
    TextSecondary,
    /// Line numbers, placeholders, trails, empty-state hints.
    TextMuted,
    /// Focus, the caret, and everything you can act on.
    Accent,
    /// Your comments.
    Comment,
    Added,
    Removed,
    Modified,
    Success,
    Danger,
    /// Warnings, and checks still running or queued.
    Warning,
    /// The merged PR chip.
    Merged,
    /// Pane borders, rules, separators.
    Border,
}

/// Every ink, in table order.
pub const INKS: [Ink; 13] = [
    Ink::Text,
    Ink::TextSecondary,
    Ink::TextMuted,
    Ink::Accent,
    Ink::Comment,
    Ink::Added,
    Ink::Removed,
    Ink::Modified,
    Ink::Success,
    Ink::Danger,
    Ink::Warning,
    Ink::Merged,
    Ink::Border,
];

/// The inks that resolve as text tiers off body text rather than from a hue.
const TIERS: [Ink; 4] = [Ink::Text, Ink::TextSecondary, Ink::TextMuted, Ink::Border];

/// A layer text sits on. A selection is text you dragged over or a line range you picked for a
/// comment: one fill, one meaning. [`LAYERS`] keeps every stack the UI paints visibly layered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fill {
    /// The terminal's own background, which the theme's `base` stands for. Never painted.
    Base,
    /// Header, footer and fold rows.
    Bar,
    /// Inline code's chip in rendered markdown.
    Code,
    /// The row under the pointer: a hint, weaker than the cursor and stronger than the bar.
    Hover,
    /// The cursor row in an unfocused pane.
    CursorInactive,
    /// The cursor row in the focused pane.
    Cursor,
    /// A text selection, or a line range picked for a comment.
    Selection,
    /// A find or search match: a solid block of the highlight hue.
    Highlight,
    /// The block caret in a text input: a solid block of the accent.
    Caret,
    Added,
    Removed,
    /// Changed words inside an added row.
    AddedEmph,
    /// Changed words inside a removed row.
    RemovedEmph,
}

/// Every fill, in table order.
pub const FILLS: [Fill; 13] = [
    Fill::Base,
    Fill::Bar,
    Fill::Code,
    Fill::Hover,
    Fill::CursorInactive,
    Fill::Cursor,
    Fill::Selection,
    Fill::Highlight,
    Fill::Caret,
    Fill::Added,
    Fill::Removed,
    Fill::AddedEmph,
    Fill::RemovedEmph,
];

/// The fills in layers, derived bottom up; each stays visibly apart from every other layer's,
/// so anything the UI stacks reads as a layer. Fills in one layer never stack.
pub const LAYERS: [&[Fill]; 7] = [
    &[Fill::Base],
    // A row's own surface: the pointer's row replaces the bar's, it never stacks on it.
    &[Fill::Bar, Fill::Code, Fill::Hover],
    &[Fill::Added, Fill::Removed],
    &[Fill::AddedEmph, Fill::RemovedEmph],
    &[Fill::Cursor, Fill::CursorInactive],
    &[Fill::Highlight, Fill::Caret],
    &[Fill::Selection],
];

/// A theme's cast, which sets the direction steps and lifts move in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cast {
    Dark,
    Light,
}

/// The colors a theme supplies. Exact upstream values; nothing here is ever adjusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Primitives {
    pub base: Color,
    pub text: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub orange: Color,
    pub purple: Color,
    pub blue: Color,
    /// The theme's UI accent: herdr's pick for the themes it ships, upstream's otherwise.
    pub accent: Color,
    pub cast: Cast,
}

/// Fills a theme sets itself because upstream ships an official value. They pass the same
/// checks as derived ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    fills: [Option<Color>; FILLS.len()],
}

impl Overrides {
    /// Set `fill` as given. Only tinted fills take an override: `base` is the terminal's, and a
    /// solid fill is its hue.
    #[must_use]
    pub fn fill(mut self, fill: Fill, color: Color) -> Self {
        debug_assert!(
            !matches!(fill, Fill::Base | Fill::Highlight | Fill::Caret),
            "{fill:?} is not a tinted fill"
        );
        self.fills[fill as usize] = Some(color);
        self
    }
}

/// Lowest contrast for text, WCAG AA.
pub const TEXT_FLOOR: f64 = 4.5;
/// Lowest contrast for muted text, glyphs and borders.
pub const MARK_FLOOR: f64 = 3.0;
/// The smallest contrast ratio between neighboring text tiers that reads as a step.
pub const TIER_STEP: f64 = 1.2;
/// Body text's target on a fill: room for the secondary tier one step below it at its floor,
/// with a hair of margin for 8-bit rounding.
const TEXT_TARGET: f64 = TEXT_FLOOR * TIER_STEP + 0.05;
/// How far a fill may soften, as a share of its starting strength. Below it a fill stops
/// reading as its own layer, so body text lifts instead.
const MIN_STRENGTH: f64 = 0.6;
/// The strongest a tinted fill may get while making room over the fills beneath it.
const MAX_STRENGTH: f64 = 0.6;
/// The secondary tier's share of body text's contrast.
const SECONDARY_SHARE: f64 = 0.70;
/// The muted tier's share of body text's contrast.
const MUTED_SHARE: f64 = 0.45;
/// A border's share of body text's contrast, floored at [`MARK_FLOOR`].
const BORDER_SHARE: f64 = 0.27;
/// The smallest `OKLab` distance at which a fill reads as distinct from the one beneath it.
pub const FILL_SEP: f64 = 0.03;
/// The smallest `OKLab` distance at which two inks that appear side by side read as different:
/// about two and a half just-noticeable differences.
pub const INK_SEP: f64 = 0.05;

/// Every role a theme paints, resolved: what every UI element paints with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    fills: [Color; FILLS.len()],
    text: [[Color; FILLS.len()]; INKS.len()],
    mark: [[Color; FILLS.len()]; INKS.len()],
}

impl Palette {
    /// The color of `fill`.
    #[must_use]
    pub fn fill(&self, fill: Fill) -> Color {
        self.fills[fill as usize]
    }

    /// The background to paint for `fill`: none for `base`, the terminal's own.
    #[must_use]
    pub fn bg(&self, fill: Fill) -> Option<Color> {
        (fill != Fill::Base).then(|| self.fill(fill))
    }

    /// `ink` painting text on `on`: clears [`TEXT_FLOOR`] there ([`MARK_FLOOR`] for the muted
    /// tier and the border, which are never body text).
    #[must_use]
    pub fn ink(&self, ink: Ink, on: Fill) -> Color {
        self.text[ink as usize][on as usize]
    }

    /// `ink` painting a glyph, a sign or a border on `on`: clears [`MARK_FLOOR`] there. The text
    /// tiers and the border resolve the same either way.
    #[must_use]
    pub fn mark(&self, ink: Ink, on: Fill) -> Color {
        self.mark[ink as usize][on as usize]
    }

    /// A content color on `on`, as legible as on the background (capped at [`TEXT_FLOOR`]), hue
    /// kept; a text tier stays its tier, and terminal defaults pass through.
    #[must_use]
    pub fn legible(&self, fg: Color, on: Fill) -> Color {
        if on == Fill::Base || !matches!(fg, Color::Rgb(..)) {
            return fg;
        }
        if let Some(tier) = TIERS.into_iter().find(|&tier| self.ink(tier, Fill::Base) == fg) {
            return self.ink(tier, on);
        }
        self.lifted(fg, on, contrast(fg, self.fill(Fill::Base)).min(TEXT_FLOOR))
    }

    /// A content color that is read as text — a markdown heading, inline code — painted on
    /// `on`: it clears [`TEXT_FLOOR`] there, its hue kept.
    #[must_use]
    pub fn readable(&self, fg: Color, on: Fill) -> Color {
        if !matches!(fg, Color::Rgb(..)) {
            return fg;
        }
        self.lifted(fg, on, TEXT_FLOOR)
    }

    /// `fg` lifted on `on` to `target` like every colored role ([`hued`]), memoized: a frame
    /// repaints the same few colors on the same few fills thousands of times.
    fn lifted(&self, fg: Color, on: Fill, target: f64) -> Color {
        thread_local! {
            static MEMO: RefCell<HashMap<(Color, Color, u64), Color>> =
                RefCell::new(HashMap::new());
        }
        let bg = self.fill(on);
        let key = (fg, bg, target.to_bits());
        MEMO.with(|memo| {
            let mut memo = memo.borrow_mut();
            if let Some(&known) = memo.get(&key) {
                return known;
            }
            // Bounded: a theme switch leaves the old palette's entries behind.
            if memo.len() > 4096 {
                memo.clear();
            }
            let lifted = hued(fg, bg, target);
            memo.insert(key, lifted);
            lifted
        })
    }

    /// Recede a color halfway to `base` behind a modal; terminal defaults pass through.
    #[must_use]
    pub fn scrim(&self, color: Color) -> Color {
        match color {
            Color::Rgb(..) => blend(color, self.fill(Fill::Base), 0.5),
            other => other,
        }
    }

    /// Derive every role from `p`, taking `o`'s fills as given.
    #[must_use]
    pub fn derive(p: Primitives, o: Overrides) -> Self {
        let hues = Hues::resolve(&p);
        let fills = derive_fills(&p, &hues, &o);
        let mut text = [[p.base; FILLS.len()]; INKS.len()];
        let mut mark = [[p.base; FILLS.len()]; INKS.len()];
        for fill in FILLS {
            let bg = fills[fill as usize];
            let tiers = Tiers::on(&p, bg);
            let colored = |hue: Color| (hued(hue, bg, TEXT_FLOOR), hued(hue, bg, MARK_FLOOR));
            for ink in INKS {
                let (t, m) = match ink {
                    Ink::Text => (tiers.text, tiers.text),
                    Ink::TextSecondary => (tiers.secondary, tiers.secondary),
                    Ink::TextMuted => (tiers.muted, tiers.muted),
                    Ink::Border => (tiers.border, tiers.border),
                    Ink::Accent => colored(hues.accent),
                    Ink::Comment => colored(hues.comment),
                    Ink::Merged => colored(hues.merged),
                    Ink::Added | Ink::Success => colored(hues.added),
                    Ink::Removed | Ink::Danger => colored(hues.removed),
                    Ink::Modified | Ink::Warning => colored(hues.modified),
                };
                text[ink as usize][fill as usize] = t;
                mark[ink as usize][fill as usize] = m;
            }
        }
        Palette { fills, text, mark }
    }
}

/// The hue each colored ink takes, after side-by-side distinctness picked among candidates.
struct Hues {
    accent: Color,
    comment: Color,
    merged: Color,
    added: Color,
    removed: Color,
    modified: Color,
}

impl Hues {
    /// Diff and status hues fixed, then `comment`, `accent`, `merged`, each the first candidate
    /// [`INK_SEP`] from every role beside it; `comment` also avoids the focus accent.
    fn resolve(p: &Primitives) -> Self {
        let (added, removed, modified) = (p.green, p.red, p.yellow);
        let pick = |candidates: &[Color], taken: &[Color]| first_distinct(p, candidates, taken);
        let comment = pick(&[p.orange, p.blue, p.purple], &[added, removed, modified, p.accent]);
        let accent =
            pick(&[p.accent, p.blue, p.purple, p.orange], &[added, removed, modified, comment]);
        let merged = pick(&[p.purple, p.blue, p.orange], &[accent, comment]);
        Hues { accent, comment, merged, added, removed, modified }
    }
}

/// The first candidate at least [`INK_SEP`] from every `taken` color, compared as each paints
/// on `base` as text; when none is, the one farthest from its nearest taken color.
fn first_distinct(p: &Primitives, candidates: &[Color], taken: &[Color]) -> Color {
    let paints = |c: Color| hued(c, p.base, TEXT_FLOOR);
    let nearest = |c: Color| {
        taken.iter().map(|&t| oklab_distance(paints(c), paints(t))).fold(f64::MAX, f64::min)
    };
    candidates.iter().copied().find(|&c| nearest(c) >= INK_SEP).unwrap_or_else(|| {
        candidates.iter().copied().max_by(|&a, &b| nearest(a).total_cmp(&nearest(b))).unwrap()
    })
}

/// The three text tiers and the border, resolved on one background.
struct Tiers {
    text: Color,
    secondary: Color,
    muted: Color,
    border: Color,
}

impl Tiers {
    fn on(p: &Primitives, bg: Color) -> Self {
        // Text keeps the theme's side unless the theme's background reads better on this fill.
        let side = if contrast(p.text, bg) >= contrast(p.base, bg) { p.text } else { p.base };
        let text = lift(side, bg, pole_on(bg), TEXT_TARGET);
        let tc = contrast(text, bg);
        let toward_bg = |target: f64| fade(text, bg, target);
        Tiers {
            text,
            secondary: toward_bg((tc * SECONDARY_SHARE).max(TEXT_FLOOR)),
            muted: toward_bg((tc * MUTED_SHARE).max(MARK_FLOOR)),
            border: toward_bg((tc * BORDER_SHARE).max(MARK_FLOOR)),
        }
    }
}

/// How a fill comes from the theme.
enum Recipe {
    /// Painted as this color: found at a glance, never softened.
    Solid(Color),
    /// `base` blended `start` of the way toward `toward`, then softened and strengthened.
    Tint { toward: Color, start: f64 },
}

/// Every fill, bottom up: a tint softens toward `base` until body text reads (to [`MIN_STRENGTH`]),
/// then strengthens until it reads as a layer over every fill below; overrides as given.
fn derive_fills(p: &Primitives, hues: &Hues, o: &Overrides) -> [Color; FILLS.len()] {
    let mut fills = [p.base; FILLS.len()];
    for (depth, layer) in LAYERS.iter().enumerate() {
        for &fill in *layer {
            let below = || LAYERS[..depth].iter().flat_map(|l| l.iter());
            let distinct = |c: Color| {
                below().all(|&under| oklab_distance(c, fills[under as usize]) >= FILL_SEP)
            };
            let color = match (o.fills[fill as usize], recipe(p, hues, fill)) {
                (Some(given), _) => given,
                (None, Recipe::Solid(color)) => color,
                (None, Recipe::Tint { toward, start }) => {
                    let at = |t: f64| blend(p.base, toward, t);
                    let mut t = start;
                    while t - 0.01 >= start * MIN_STRENGTH && contrast(p.text, at(t)) < TEXT_TARGET
                    {
                        t -= 0.01;
                    }
                    while !distinct(at(t)) && t + 0.01 <= MAX_STRENGTH {
                        t += 0.01;
                    }
                    at(t)
                }
            };
            fills[fill as usize] = color;
        }
    }
    fills
}

/// How each fill comes from the theme's hues.
fn recipe(p: &Primitives, hues: &Hues, fill: Fill) -> Recipe {
    let dark = p.cast == Cast::Dark;
    let tint = |toward: Color, start: f64| Recipe::Tint { toward, start };
    match fill {
        Fill::Base => Recipe::Solid(p.base),
        Fill::Highlight => Recipe::Solid(hues.modified),
        Fill::Caret => Recipe::Solid(hues.accent),
        Fill::Bar | Fill::Code => tint(pole(p.cast), 0.045),
        // Half the cursor's strength: a row under the pointer reads as a hint, not a cursor.
        Fill::Hover => tint(pole(p.cast), 0.07),
        Fill::CursorInactive => tint(pole(p.cast), 0.09),
        Fill::Cursor => tint(pole(p.cast), 0.14),
        Fill::Selection => tint(saturated(hues.accent), if dark { 0.38 } else { 0.22 }),
        Fill::Added => tint(hues.added, if dark { 0.20 } else { 0.12 }),
        Fill::Removed => tint(hues.removed, if dark { 0.20 } else { 0.12 }),
        Fill::AddedEmph => tint(hues.added, if dark { 0.38 } else { 0.22 }),
        Fill::RemovedEmph => tint(hues.removed, if dark { 0.38 } else { 0.22 }),
    }
}

/// The contrast pole a theme steps toward: white on a dark theme, black on a light one.
fn pole(cast: Cast) -> Color {
    match cast {
        Cast::Dark => Color::Rgb(0xff, 0xff, 0xff),
        Cast::Light => Color::Rgb(0x00, 0x00, 0x00),
    }
}

/// The pole that reads best on `bg`: black on a bright fill, white on a dark one.
fn pole_on(bg: Color) -> Color {
    let (black, white) = (Color::Rgb(0, 0, 0), Color::Rgb(0xff, 0xff, 0xff));
    if contrast(black, bg) >= contrast(white, bg) { black } else { white }
}

/// A colored role's `hue` on `bg`: its official color wherever it clears `floor`, else moved
/// in lightness toward the pole that reads there, hue kept.
fn hued(hue: Color, bg: Color, floor: f64) -> Color {
    lift_lightness(hue, bg, pole_on(bg) != Color::Rgb(0, 0, 0), floor)
}

/// `fg` blended toward `toward` just far enough to clear `min` on `bg`; `fg` itself when it
/// already does, `toward` when nothing short of it does.
fn lift(fg: Color, bg: Color, toward: Color, min: f64) -> Color {
    if contrast(fg, bg) >= min {
        return fg;
    }
    if contrast(toward, bg) < min {
        return toward;
    }
    // One crossing from failing to passing along the blend, so bisect for the nearest pass.
    let (mut short, mut far) = (0.0_f64, 1.0_f64);
    for _ in 0..16 {
        let mid = f64::midpoint(short, far);
        if contrast(blend(fg, toward, mid), bg) >= min { far = mid } else { short = mid }
    }
    blend(fg, toward, far)
}

/// `fg` blended toward `bg` as far as it can go while keeping `target` contrast on it.
fn fade(fg: Color, bg: Color, target: f64) -> Color {
    if contrast(fg, bg) <= target {
        return fg;
    }
    // Contrast falls monotonically along the blend, so bisect for the last blend that keeps it.
    let (mut keep, mut lose) = (0.0_f64, 1.0_f64);
    for _ in 0..24 {
        let mid = f64::midpoint(keep, lose);
        if contrast(blend(fg, bg, mid), bg) >= target { keep = mid } else { lose = mid }
    }
    blend(fg, bg, keep)
}

/// `fg` moved in `OKLab` lightness only, just far enough to clear `min` on `bg`, hue kept: a pole
/// blend would wash a syntax token toward gray.
fn lift_lightness(fg: Color, bg: Color, lighter: bool, min: f64) -> Color {
    if contrast(fg, bg) >= min {
        return fg;
    }
    let lab = oklab(fg);
    let end = if lighter { 1.0 } else { 0.0 };
    let at = |lightness: f64| in_gamut(Oklab::new(lightness, lab.a, lab.b));
    if contrast(at(end), bg) < min {
        return at(end);
    }
    let (mut short, mut far) = (lab.l, end);
    for _ in 0..20 {
        let mid = f64::midpoint(short, far);
        if contrast(at(mid), bg) >= min { far = mid } else { short = mid }
    }
    at(far)
}

/// `lab` with the most of its chroma sRGB can show at its lightness.
fn in_gamut(lab: Oklab<f64>) -> Color {
    // Unclamped: the clamping conversion would report every color as in gamut.
    let with =
        |k: f64| -> LinSrgb<f64> { Oklab::new(lab.l, lab.a * k, lab.b * k).into_color_unclamped() };
    let k = if with(1.0).is_within_bounds() {
        1.0
    } else {
        let (mut keep, mut lose) = (0.0_f64, 1.0_f64);
        for _ in 0..20 {
            let mid = f64::midpoint(keep, lose);
            if with(mid).is_within_bounds() { keep = mid } else { lose = mid }
        }
        keep
    };
    let rgb: Srgb<u8> = Srgb::<f64>::from_linear(with(k)).clamp().into_format();
    Color::Rgb(rgb.red, rgb.green, rgb.blue)
}

/// Halfway between a hue and its colorful core, so a pastel tints `base` into a clear hue.
fn saturated(c: Color) -> Color {
    let (r, g, b) = channels(c);
    let lo = r.min(g).min(b);
    let span = r.max(g).max(b) - lo;
    if span == 0 {
        return c;
    }
    let core = |ch: u8| (f64::from(ch - lo) * 255.0 / f64::from(span)).round() as u8;
    blend(c, Color::Rgb(core(r), core(g), core(b)), 0.5)
}

/// Linear per-channel blend: `t` of the way from `from` to `to`.
fn blend(from: Color, to: Color, t: f64) -> Color {
    let (fr, fg, fb) = channels(from);
    let (tr, tg, tb) = channels(to);
    let mix = |lhs: u8, rhs: u8| (f64::from(lhs) * (1.0 - t) + f64::from(rhs) * t).round() as u8;
    Color::Rgb(mix(fr, tr), mix(fg, tg), mix(fb, tb))
}

/// The WCAG 2.1 contrast ratio between two colors (1.0 .. 21.0).
#[must_use]
pub fn contrast(fg: Color, bg: Color) -> f64 {
    srgb(fg).relative_contrast(srgb(bg))
}

/// Euclidean distance in `OKLab`: how different two colors look.
#[must_use]
pub fn oklab_distance(a: Color, b: Color) -> f64 {
    oklab(a).distance(oklab(b))
}

fn oklab(color: Color) -> Oklab<f64> {
    srgb(color).into_linear().into_color()
}

fn srgb(color: Color) -> Srgb<f64> {
    let (r, g, b) = channels(color);
    Srgb::new(r, g, b).into_format()
}

fn channels(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::{Cast, Color, Primitives, contrast, first_distinct, lift_lightness, oklab};

    #[test]
    fn contrast_black_white_is_max() {
        let c = contrast(Color::Rgb(0, 0, 0), Color::Rgb(0xff, 0xff, 0xff));
        assert!((c - 21.0).abs() < 1e-6, "{c}");
    }

    /// Catppuccin's red, lifted to 4.5:1 on its light cursor row, stays red: same hue, most of
    /// its chroma. A blend toward body text reached the contrast by turning it pink-gray.
    #[test]
    fn a_lifted_syntax_color_keeps_its_hue() {
        let (red, cursor) = (Color::Rgb(0xf3, 0x8b, 0xa8), Color::Rgb(0x58, 0x5b, 0x70));
        let lifted = lift_lightness(red, cursor, true, 4.5);
        assert!(contrast(lifted, cursor) >= 4.5, "{lifted:?} reads on the cursor row");
        let polar = |c: Color| {
            let lab = oklab(c);
            (lab.b.atan2(lab.a).to_degrees(), lab.a.hypot(lab.b))
        };
        let ((hue, chroma), (lifted_hue, lifted_chroma)) = (polar(red), polar(lifted));
        assert!((hue - lifted_hue).abs() < 8.0, "hue {hue:.1}° became {lifted_hue:.1}°");
        assert!(lifted_chroma >= chroma * 0.5, "chroma {chroma:.3} fell to {lifted_chroma:.3}");
    }

    /// With no candidate far enough from every taken hue, the pick is the one farthest from its
    /// nearest.
    #[test]
    fn with_no_distinct_candidate_the_farthest_wins() {
        let gray = Color::Rgb(0x80, 0x80, 0x80);
        let p = Primitives {
            base: Color::Rgb(0x10, 0x10, 0x10),
            text: Color::Rgb(0xe0, 0xe0, 0xe0),
            red: gray,
            green: gray,
            yellow: gray,
            orange: gray,
            purple: gray,
            blue: gray,
            accent: gray,
            cast: Cast::Dark,
        };
        let (near, nearer) = (Color::Rgb(0x84, 0x80, 0x80), Color::Rgb(0x81, 0x80, 0x80));
        assert_eq!(first_distinct(&p, &[nearer, near], &[gray]), near);
    }
}
