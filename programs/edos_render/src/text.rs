//! Drawing text into a pixel buffer.
//!
//! One blitter for the whole shell, so the window manager's titles, the
//! panel's labels and every widget agree about where a glyph sits and how wide
//! a string is. It draws through [`crate::font`] when outline faces are
//! installed, and falls back to the bitmap face otherwise.

use crate::font::{self, Family, Weight};
use crate::surface::Surface;

/// How a run of text is set: face, weight, size, colour.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub family: Family,
    pub weight: Weight,
    pub px: u32,
    pub color: u32,
}

impl Style {
    /// Interface text: proportional, regular, body size.
    pub const fn new(color: u32) -> Self {
        Self {
            family: Family::Sans,
            weight: Weight::Regular,
            px: font::size::BODY,
            color,
        }
    }

    pub const fn mono(color: u32) -> Self {
        Self {
            family: Family::Mono,
            weight: Weight::Regular,
            px: font::size::BODY,
            color,
        }
    }

    pub const fn with_weight(mut self, weight: Weight) -> Self {
        self.weight = weight;
        self
    }

    pub const fn with_px(mut self, px: u32) -> Self {
        self.px = px;
        self
    }
}

/// The bitmap face the shell used before outlines, kept as the fallback so a
/// missing `/share/fonts` degrades the type instead of blanking the screen.
mod bitmap {
    use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};

    pub const HEIGHT: RasterHeight = RasterHeight::Size16;

    pub fn advance() -> u32 {
        get_raster_width(FontWeight::Regular, HEIGHT) as u32
    }

    pub fn draw(
        surface: &mut super::Surface<'_>,
        x: i32,
        y: i32,
        text: &str,
        color: u32,
        letter: i32,
    ) {
        let advance = advance() as i32;
        let mut pen = x;
        for ch in text.chars() {
            if let Some(raster) = get_raster(ch, FontWeight::Regular, HEIGHT) {
                for (row, line) in raster.raster().iter().enumerate() {
                    for (col, &intensity) in line.iter().enumerate() {
                        surface.blend(pen + col as i32, y + row as i32, color, intensity);
                    }
                }
            }
            pen += advance + letter;
        }
    }
}

/// Draw `text` with its first line's top edge at `y`.
///
/// `y` is the top of the line rather than the baseline, because that is what
/// every caller already had: a widget knows the box it is filling, not the
/// typographic grid.
pub fn draw(surface: &mut Surface<'_>, x: i32, y: i32, text: &str, style: Style) {
    draw_tracked(surface, x, y, text, style, 0);
}

/// Draw `text` with `letter` pixels added to every character's advance, which
/// is what CSS `letter-spacing` asks for.
///
/// The tracking is part of the pen rather than applied by drawing each
/// character separately, so the sub-pixel advances still accumulate the way an
/// untracked run's do and [`width_tracked`] stays the width that gets drawn.
pub fn draw_tracked(
    surface: &mut Surface<'_>,
    x: i32,
    y: i32,
    text: &str,
    style: Style,
    letter: i32,
) {
    if !font::available() {
        bitmap::draw(surface, x, y, text, style.color, letter);
        return;
    }

    let (ascent, _) = font::line_metrics(style.family, style.weight, style.px);
    let baseline = y + ascent as i32;
    let mut pen = x as f32;

    for ch in text.chars() {
        let Some(glyph) = font::glyph(style.family, style.weight, style.px, ch) else {
            continue;
        };
        let gx = pen as i32 + glyph.left;
        let gy = baseline - glyph.top;
        for row in 0..glyph.height {
            for col in 0..glyph.width {
                let coverage = glyph.coverage[row * glyph.width + col];
                surface.blend(gx + col as i32, gy + row as i32, style.color, coverage);
            }
        }
        pen += glyph.advance + letter as f32;
    }
}

/// Width of `text` when set in `style`, in pixels.
pub fn width(text: &str, style: Style) -> u32 {
    if !font::available() {
        return text.chars().count() as u32 * bitmap::advance();
    }
    font::measure(style.family, style.weight, style.px, text)
}

/// Width of `text` set in `style` with `letter` pixels of tracking, which the
/// last character carries too: the advance is the character's, not the gap to
/// the next one.
pub fn width_tracked(text: &str, style: Style, letter: i32) -> u32 {
    let count = text.chars().count() as i32;
    (width(text, style) as i32 + letter * count).max(0) as u32
}

/// What marks a string that was cut to fit.
pub const ELLIPSIS: &str = "…";

/// Advance of one character set in `style`, unrounded.
///
/// Unrounded because [`width`] rounds the whole string once: accumulating
/// per-character *pixels* overstates a prefix by up to a pixel per character,
/// and a caller comparing that running total against a column would cut text
/// that fits.
fn advance(ch: char, style: Style) -> f32 {
    if !font::available() {
        return bitmap::advance() as f32;
    }
    font::glyph(style.family, style.weight, style.px, ch)
        .map(|glyph| glyph.advance)
        .unwrap_or(0.0)
}

/// Each character's byte offset and the pen position it starts at — which is
/// the width of everything before it, and so [`width_tracked`] of that prefix
/// once rounded.
///
/// This is the one pass every "how much of this fits" question wants. Asking
/// [`width`] about each candidate prefix instead re-measures the whole prefix
/// per character, which is quadratic in metric lookups on strings a text field
/// remeasures per keystroke and a line breaker per relayout.
fn pens(text: &str, style: Style, letter: i32) -> impl Iterator<Item = (usize, f32)> + '_ {
    let mut pen = 0.0f32;
    text.char_indices().map(move |(at, ch)| {
        let before = pen;
        pen += advance(ch, style) + letter as f32;
        (at, before)
    })
}

/// `text` cut to `available` pixels, with an [`ELLIPSIS`] where it was cut.
pub fn elide(text: &str, available: u32, style: Style) -> String {
    if width(text, style) <= available {
        return text.to_string();
    }
    let room = available.saturating_sub(width(ELLIPSIS, style));
    let mut kept = String::new();
    let mut used = 0.0f32;
    for ch in text.chars() {
        used += advance(ch, style);
        if used.ceil() as u32 > room {
            break;
        }
        kept.push(ch);
    }
    kept.push_str(ELLIPSIS);
    kept
}

/// The longest *proper* prefix of `text` that fits in `room` pixels when set
/// in `style` with `letter` pixels of tracking, as a byte offset, or `None`
/// when not even its first character does.
pub fn fit_prefix(text: &str, style: Style, room: u32, letter: i32) -> Option<usize> {
    let mut fits = None;
    for (at, pen) in pens(text, style, letter).skip(1) {
        if pen.ceil() as u32 > room {
            break;
        }
        fits = Some(at);
    }
    fits
}

/// The character position closest to `offset` pixels from the first glyph.
///
/// Rounds to the nearer edge of the character it lands in, so clicking the
/// right half of a glyph puts the caret after it. The count of characters is
/// returned for an `offset` past the end.
pub fn char_at_width(text: &str, style: Style, offset: u32) -> usize {
    // The pen before the first character is zero, so the `index - 1` below is
    // never reached at index 0.
    let mut previous = 0;
    for (index, (_, pen)) in pens(text, style, 0).enumerate() {
        let advance = pen.ceil() as u32;
        if advance > offset {
            let midpoint = previous + (advance - previous) / 2;
            return if offset < midpoint { index - 1 } else { index };
        }
        previous = advance;
    }
    let count = text.chars().count();
    let full = width(text, style);
    let midpoint = previous + (full - previous) / 2;
    if count > 0 && offset < midpoint {
        count - 1
    } else {
        count
    }
}

/// Height of one line set in `style`, in pixels.
pub fn line_height(style: Style) -> u32 {
    if !font::available() {
        return crate::metrics::TEXT_CELL_HEIGHT;
    }
    font::line_metrics(style.family, style.weight, style.px).1
}
