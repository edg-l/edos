//! Checkbox widget with label.

use super::{
    Rect, Widget, WidgetEvent, WidgetId, char_width, colors, draw_rect, draw_rect_outline,
    draw_text, text_height,
};

/// A toggleable checkbox with a label.
pub struct Checkbox {
    id: WidgetId,
    x: i32,
    y: i32,
    label: String,
    checked: bool,
    hovered: bool,
    focused: bool,
}

const BOX_SIZE: u32 = 16;
const BOX_LABEL_GAP: u32 = 8;

impl Checkbox {
    /// Create a new checkbox.
    pub fn new(id: WidgetId, x: i32, y: i32, label: &str) -> Self {
        Self {
            id,
            x,
            y,
            label: label.to_string(),
            checked: false,
            hovered: false,
            focused: false,
        }
    }

    /// Create a checkbox with initial checked state.
    pub fn with_checked(id: WidgetId, x: i32, y: i32, label: &str, checked: bool) -> Self {
        Self {
            id,
            x,
            y,
            label: label.to_string(),
            checked,
            hovered: false,
            focused: false,
        }
    }

    /// Get the checked state.
    pub fn checked(&self) -> bool {
        self.checked
    }

    /// Set the checked state.
    pub fn set_checked(&mut self, checked: bool) {
        self.checked = checked;
    }

    /// Toggle the checked state.
    pub fn toggle(&mut self) {
        self.checked = !self.checked;
    }

    /// Get the label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Set the label.
    pub fn set_label(&mut self, label: &str) {
        self.label = label.to_string();
    }
}

impl Widget for Checkbox {
    fn id(&self) -> WidgetId {
        self.id
    }

    fn bounds(&self) -> Rect {
        let label_width = (self.label.len() as u32) * char_width();
        let total_width = BOX_SIZE + BOX_LABEL_GAP + label_width;
        Rect::new(self.x, self.y, total_width, BOX_SIZE.max(text_height()))
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn draw(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32) {
        // Draw checkbox box background
        let bg_color = if self.hovered {
            colors::BUTTON_HOVER
        } else {
            colors::INPUT_BG
        };

        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            self.y,
            BOX_SIZE,
            BOX_SIZE,
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
                BOX_SIZE + 4,
                BOX_SIZE + 4,
                colors::FOCUS_RING,
            );
        }

        // Draw border
        let border_color = if self.focused {
            colors::FOCUS_RING
        } else {
            colors::INPUT_BORDER
        };
        draw_rect_outline(buffer, buffer_width, buffer_height,
            self.x, self.y, BOX_SIZE, BOX_SIZE, border_color);

        // Draw check mark if checked
        if self.checked {
            let inset = 3u32;
            draw_rect(
                buffer,
                buffer_width,
                buffer_height,
                self.x + inset as i32,
                self.y + inset as i32,
                BOX_SIZE - inset * 2,
                BOX_SIZE - inset * 2,
                colors::CHECKBOX_CHECK,
            );
        }

        // Draw label
        let label_x = self.x + BOX_SIZE as i32 + BOX_LABEL_GAP as i32;
        let label_y = self.y + (BOX_SIZE as i32 - text_height() as i32) / 2;
        draw_text(
            buffer,
            buffer_width,
            buffer_height,
            label_x,
            label_y,
            &self.label,
            colors::TEXT,
        );
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        self.hovered = self.bounds().contains(x, y);
    }

    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent> {
        if !pressed && self.bounds().contains(x, y) {
            self.toggle();
            Some(WidgetEvent::ValueChanged(if self.checked { 1 } else { 0 }))
        } else {
            None
        }
    }

    fn on_char(&mut self, _ch: char) -> Option<WidgetEvent> {
        None
    }

    fn on_key(&mut self, scancode: u32, pressed: bool) -> Option<WidgetEvent> {
        // Space toggles the checkbox when focused
        if self.focused && pressed && scancode == 57 {
            // 57 = Space
            self.toggle();
            Some(WidgetEvent::ValueChanged(if self.checked { 1 } else { 0 }))
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
