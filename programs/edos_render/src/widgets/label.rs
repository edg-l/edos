//! Simple text label widget.

use super::{Rect, Widget, WidgetEvent, colors};
use crate::surface::Surface;
use crate::text::Style;

/// A simple text display widget.
pub struct Label {
    x: i32,
    y: i32,
    text: String,
    color: u32,
}

impl Label {
    /// Create a new label.
    pub fn new(x: i32, y: i32, text: &str) -> Self {
        Self {
            x,
            y,
            text: text.to_string(),
            color: colors::TEXT,
        }
    }

    /// Create a new label with custom color.
    pub fn with_color(x: i32, y: i32, text: &str, color: u32) -> Self {
        Self {
            x,
            y,
            text: text.to_string(),
            color,
        }
    }

    /// Set the label text.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
    }

    /// Get the label text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Set the text color.
    pub fn set_color(&mut self, color: u32) {
        self.color = color;
    }
}

impl Widget for Label {
    fn bounds(&self) -> Rect {
        let width = super::text_width(&self.text);
        Rect::new(self.x, self.y, width.max(1), super::text_height())
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn draw(&self, surface: &mut Surface<'_>) {
        surface.text(self.x, self.y, &self.text, Style::new(self.color));
    }

    fn on_mouse_move(&mut self, _x: i32, _y: i32) {
        // Labels don't respond to mouse
    }

    fn on_mouse_button(&mut self, _x: i32, _y: i32, _pressed: bool) -> Option<WidgetEvent> {
        // Labels don't respond to clicks
        None
    }

    fn on_char(&mut self, _ch: char) -> Option<WidgetEvent> {
        None
    }

    fn on_key(&mut self, _scancode: u32, _pressed: bool) -> Option<WidgetEvent> {
        None
    }
}
