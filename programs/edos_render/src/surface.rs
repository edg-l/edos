//! The pixel buffer everything draws into.
//!
//! One rasteriser for the whole shell: a widget, the panel and the window
//! manager all fill rectangles and set text through the same clip and the same
//! bounds arithmetic, so an off-screen or clipped draw behaves identically
//! wherever it comes from.

use crate::graphics::Color;
use crate::text::{self, Style};
use crate::widgets::Rect;

/// A pixel buffer to draw into.
pub struct Surface<'a> {
    pub pixels: &'a mut [u32],
    pub width: u32,
    pub height: u32,
    /// Bounds drawing is confined to, as `(x0, y0, x1, y1)` with the far edges
    /// exclusive. `None` is the whole surface.
    pub clip: Option<(i32, i32, i32, i32)>,
}

impl<'a> Surface<'a> {
    /// A surface covering the whole buffer.
    pub fn new(pixels: &'a mut [u32], width: u32, height: u32) -> Self {
        Self {
            pixels,
            width,
            height,
            clip: None,
        }
    }

    pub(crate) fn blend(&mut self, x: i32, y: i32, color: u32, coverage: u8) {
        if coverage == 0 || x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        if let Some((x0, y0, x1, y1)) = self.clip
            && (x < x0 || x >= x1 || y < y0 || y >= y1)
        {
            return;
        }
        let idx = (y as u32 * self.width + x as u32) as usize;
        let Some(dst) = self.pixels.get_mut(idx) else {
            return;
        };
        if coverage == 255 {
            *dst = color;
            return;
        }
        let bg = Color::from(*dst);
        let fg = Color::from(color);
        let a = coverage as u32;
        let inv = 255 - a;
        let mix = |f: u8, b: u8| ((f as u32 * a + b as u32 * inv) / 255) as u8;
        *dst = Color::from_rgb(
            mix(fg.red(), bg.red()),
            mix(fg.green(), bg.green()),
            mix(fg.blue(), bg.blue()),
        )
        .raw();
    }
}

impl Surface<'_> {
    /// Restrict drawing to `(x, y, width, height)`, intersected with any clip
    /// already in force.
    pub fn clip_to(&mut self, x: i32, y: i32, width: u32, height: u32) {
        let want = (
            x,
            y,
            (x as i64 + width as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            (y as i64 + height as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        );
        self.clip = Some(match self.clip {
            Some((x0, y0, x1, y1)) => (
                x0.max(want.0),
                y0.max(want.1),
                x1.min(want.2),
                y1.min(want.3),
            ),
            None => want,
        });
    }

    /// Fill a rectangle.
    pub fn rect(&mut self, x: i32, y: i32, width: u32, height: u32, color: u32) {
        if self.width == 0 {
            return;
        }
        // The far edges are computed in i64. A rect entirely left of or above
        // the surface has a negative one, and casting that to u32 before the
        // clamp turns it into a value far larger than the surface -- which is
        // how a fully off-screen rect came to fill whole rows.
        let mut x0 = x.max(0) as i64;
        let mut y0 = y.max(0) as i64;
        let mut x1 = (x as i64 + width as i64).min(self.width as i64);
        // Rows the buffer actually holds, which is what bounds the writes; a
        // surface claiming more rows than its slice carries is clamped here
        // rather than caught per pixel further down.
        let rows = (self.pixels.len() / self.width as usize) as i64;
        let mut y1 = (y as i64 + height as i64).min(self.height as i64).min(rows);
        if let Some((cx0, cy0, cx1, cy1)) = self.clip {
            x0 = x0.max(cx0 as i64);
            y0 = y0.max(cy0 as i64);
            x1 = x1.min(cx1 as i64);
            y1 = y1.min(cy1 as i64);
        }
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for py in y0..y1 {
            let row = (py * self.width as i64) as usize;
            self.pixels[row + x0 as usize..row + x1 as usize].fill(color);
        }
    }

    /// Draw a one-pixel rectangle outline.
    pub fn rect_outline(&mut self, x: i32, y: i32, width: u32, height: u32, color: u32) {
        self.rect(x, y, width, 1, color);
        self.rect(x, y + height as i32 - 1, width, 1, color);
        self.rect(x, y, 1, height, color);
        self.rect(x + width as i32 - 1, y, 1, height, color);
    }

    /// Draw the keyboard focus ring around a control, offset from its edge by
    /// [`crate::metrics::FOCUS_RING_GAP`] so the ring never touches the border.
    pub fn focus_ring(&mut self, x: i32, y: i32, width: u32, height: u32) {
        let gap = crate::metrics::FOCUS_RING_GAP;
        self.rect_outline(
            x - gap as i32,
            y - gap as i32,
            width + gap * 2,
            height + gap * 2,
            crate::widgets::colors::FOCUS_RING,
        );
    }

    /// Fill a rectangle with a vertical gradient from `top` to `bottom`.
    pub fn gradient_v(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        top: Color,
        bottom: Color,
    ) {
        for row in 0..height {
            let t = ((row as u64 * 255) / (height as u64 - 1).max(1)) as u8;
            let color = crate::theme::lerp_color(top, bottom, t);
            self.rect(x, y + row as i32, width, 1, color.raw());
        }
    }

    /// Fill `rect` with a solid colour.
    pub fn fill(&mut self, rect: Rect, color: u32) {
        self.rect(rect.x, rect.y, rect.width, rect.height, color);
    }

    /// A one-pixel horizontal rule, which is what separates two panels.
    pub fn hline(&mut self, x: i32, y: i32, width: u32, color: u32) {
        self.rect(x, y, width, 1, color);
    }

    /// A one-pixel border around `rect`, drawn inside it.
    pub fn outline(&mut self, rect: Rect, color: u32) {
        self.rect_outline(rect.x, rect.y, rect.width, rect.height, color);
    }

    /// Draw a line of text with its top edge at `y`, and report its width.
    ///
    /// `y` is the top of the line rather than the baseline, because that is
    /// what every caller already had: a widget knows the box it is filling,
    /// not the typographic grid.
    pub fn text(&mut self, x: i32, y: i32, s: &str, style: Style) -> u32 {
        text::draw(self, x, y, s, style);
        text::width(s, style)
    }

    /// Draw text vertically centred in `rect`, starting at `x`.
    pub fn text_in(&mut self, x: i32, rect: Rect, s: &str, style: Style) -> u32 {
        let y = rect.y + (rect.height as i32 - text::line_height(style) as i32) / 2;
        self.text(x, y, s, style)
    }

    /// Draw text ending at `right`, vertically centred in `rect`.
    pub fn text_right(&mut self, right: i32, rect: Rect, s: &str, style: Style) {
        let width = text::width(s, style) as i32;
        self.text_in(right - width, rect, s, style);
    }

    /// Draw an icon mask with its top-left corner at (`x`, `y`).
    pub fn icon(&mut self, x: i32, y: i32, mask: &crate::icons::Mask, color: u32) {
        crate::icons::draw(self, x, y, mask, color);
    }

    /// Draw `pixels`, a `w` x `h` opaque image, with its top-left at
    /// (`x`, `y`).
    pub fn blit(&mut self, x: i32, y: i32, w: u32, h: u32, pixels: &[u32]) {
        for row in 0..h {
            let py = y + row as i32;
            if py < 0 || py >= self.height as i32 {
                continue;
            }
            for col in 0..w {
                let px = x + col as i32;
                if px < 0 || px >= self.width as i32 {
                    continue;
                }
                let src = (row * w + col) as usize;
                let dst = (py as u32 * self.width + px as u32) as usize;
                if let (Some(&value), Some(slot)) = (pixels.get(src), self.pixels.get_mut(dst)) {
                    *slot = value;
                }
            }
        }
    }
}
