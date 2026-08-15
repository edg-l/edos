//! A window's back buffer with the drawing operations hung off it.
//!
//! The free functions in [`super`] all take `(buffer, buffer_width,
//! buffer_height)` as their first three arguments, which is the shape a
//! widget's `draw` is handed. A whole program drawing its own chrome threads
//! that triple through every call, so each graphical program had grown its own
//! wrapper bundling the three -- three copies of the same struct with the same
//! methods, drifting in what they offered. This is that wrapper, once.

use crate::icons;
use crate::text::{self, Style, Surface};

use super::{Rect, draw_rect, draw_rect_outline};

/// A pixel buffer and its dimensions, with the drawing a program does on it.
///
/// Every operation clips to the buffer, so a rectangle or a glyph that runs
/// off an edge is cut rather than wrapping onto the next row.
pub struct Canvas<'a> {
    pub buf: &'a mut [u32],
    pub width: u32,
    pub height: u32,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [u32], width: u32, height: u32) -> Self {
        Self { buf, width, height }
    }

    /// Fill `rect` with a solid colour.
    pub fn fill(&mut self, rect: Rect, color: u32) {
        draw_rect(
            self.buf,
            self.width,
            self.height,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            color,
        );
    }

    /// A one-pixel horizontal rule, which is what separates two panels.
    pub fn hline(&mut self, x: i32, y: i32, width: u32, color: u32) {
        self.fill(Rect::new(x, y, width, 1), color);
    }

    /// A one-pixel border around `rect`, drawn inside it.
    pub fn outline(&mut self, rect: Rect, color: u32) {
        draw_rect_outline(
            self.buf,
            self.width,
            self.height,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            color,
        );
    }

    /// Stamp an icon mask in one colour, with its top-left at (`x`, `y`).
    pub fn icon(&mut self, x: i32, y: i32, mask: &icons::Mask, color: u32) {
        icons::draw(self.buf, self.width, self.height, x, y, mask, color);
    }

    /// Draw a line of text with its top edge at `y`, and report its width.
    pub fn text(&mut self, x: i32, y: i32, string: &str, style: Style) -> u32 {
        let mut surface = Surface::new(self.buf, self.width, self.height);
        text::draw(&mut surface, x, y, string, style);
        text::width(string, style)
    }

    /// Draw text vertically centred in `rect`, starting at `x`.
    pub fn text_in(&mut self, x: i32, rect: Rect, string: &str, style: Style) -> u32 {
        let y = rect.y + (rect.height as i32 - text::line_height(style) as i32) / 2;
        self.text(x, y, string, style)
    }

    /// Draw text ending at `right`, vertically centred in `rect`.
    pub fn text_right(&mut self, right: i32, rect: Rect, string: &str, style: Style) {
        let width = text::width(string, style) as i32;
        self.text_in(right - width, rect, string, style);
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
                if let (Some(&value), Some(slot)) = (pixels.get(src), self.buf.get_mut(dst)) {
                    *slot = value;
                }
            }
        }
    }
}
