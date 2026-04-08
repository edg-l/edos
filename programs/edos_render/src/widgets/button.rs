//! Clickable button widget.

use super::{
    Rect, Widget, WidgetEvent, WidgetId, char_width, colors, draw_rect, draw_rect_outline,
    draw_text, text_height,
};

/// A clickable button with a label.
pub struct Button {
    id: WidgetId,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    label: String,
    hovered: bool,
    pressed: bool,
    focused: bool,
}

impl Button {
    /// Create a new button that auto-sizes based on text.
    pub fn new(id: WidgetId, x: i32, y: i32, label: &str) -> Self {
        let padding = 16;
        let width = (label.len() as u32) * char_width() + padding * 2;
        let height = text_height() + padding;

        Self {
            id,
            x,
            y,
            width,
            height,
            label: label.to_string(),
            hovered: false,
            pressed: false,
            focused: false,
        }
    }

    /// Create a new button with explicit size.
    pub fn with_size(id: WidgetId, x: i32, y: i32, width: u32, height: u32, label: &str) -> Self {
        Self {
            id,
            x,
            y,
            width,
            height,
            label: label.to_string(),
            hovered: false,
            pressed: false,
            focused: false,
        }
    }

    /// Set the button label.
    pub fn set_label(&mut self, label: &str) {
        self.label = label.to_string();
    }

    /// Get the button label.
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl Widget for Button {
    fn id(&self) -> WidgetId {
        self.id
    }

    fn bounds(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn draw(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32) {
        // Choose background color based on state
        let bg_color = if self.pressed {
            colors::BUTTON_PRESSED
        } else if self.hovered {
            colors::BUTTON_HOVER
        } else {
            colors::BUTTON_NORMAL
        };

        // Draw button background
        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            self.y,
            self.width,
            self.height,
            bg_color,
        );

        // Draw focus ring if focused
        if self.focused {
            draw_rect_outline(
                buffer,
                buffer_width,
                buffer_height,
                self.x - 2,
                self.y - 2,
                self.width + 4,
                self.height + 4,
                colors::FOCUS_RING,
            );
        }

        // Draw border (subtle, matches dark theme)
        let border_color = if self.pressed {
            colors::FOCUS_RING
        } else if self.hovered {
            colors::FOCUS_RING
        } else {
            colors::INPUT_BORDER
        };
        draw_rect_outline(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            self.y,
            self.width,
            self.height,
            border_color,
        );

        // Center the text
        let text_width = (self.label.len() as u32) * char_width();
        let text_x = self.x + (self.width as i32 - text_width as i32) / 2;
        let text_y = self.y + (self.height as i32 - text_height() as i32) / 2;

        draw_text(
            buffer,
            buffer_width,
            buffer_height,
            text_x,
            text_y,
            &self.label,
            colors::TEXT,
        );
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        self.hovered = self.bounds().contains(x, y);
        // Release press if mouse moves outside while pressed
        if !self.hovered {
            self.pressed = false;
        }
    }

    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent> {
        let inside = self.bounds().contains(x, y);

        if pressed && inside {
            self.pressed = true;
            None
        } else if !pressed && self.pressed && inside {
            self.pressed = false;
            Some(WidgetEvent::Clicked)
        } else {
            self.pressed = false;
            None
        }
    }

    fn on_char(&mut self, _ch: char) -> Option<WidgetEvent> {
        None
    }

    fn on_key(&mut self, scancode: u32, pressed: bool) -> Option<WidgetEvent> {
        // Space or Enter activates the button when focused
        if self.focused && pressed && (scancode == 28 || scancode == 57) {
            // 28 = Enter, 57 = Space
            Some(WidgetEvent::Clicked)
        } else {
            None
        }
    }

    fn focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
