//! Checkbox widget with label.

use super::{
    Rect, Widget, WidgetEvent, WidgetId, char_width, colors, draw_focus_ring, draw_rect,
    draw_rect_outline, draw_text, text_height,
};
use crate::metrics::{CHECKBOX_BOX, CHECKBOX_INSET, CONTROL_HEIGHT, LABEL_GAP};

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

    /// Top of the box, centred in the control row so a checkbox lines up with
    /// the taller controls beside it.
    fn box_y(&self) -> i32 {
        self.y + (CONTROL_HEIGHT as i32 - CHECKBOX_BOX as i32) / 2
    }
}

impl Widget for Checkbox {
    fn id(&self) -> WidgetId {
        self.id
    }

    fn bounds(&self) -> Rect {
        let label_width = (self.label.chars().count() as u32) * char_width();
        let total_width = CHECKBOX_BOX + LABEL_GAP + label_width;
        Rect::new(self.x, self.y, total_width, CONTROL_HEIGHT)
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
            self.box_y(),
            CHECKBOX_BOX,
            CHECKBOX_BOX,
            bg_color,
        );

        // Draw focus ring if focused
        if self.focused {
            draw_focus_ring(
                buffer,
                buffer_width,
                buffer_height,
                self.x,
                self.box_y(),
                CHECKBOX_BOX,
                CHECKBOX_BOX,
            );
        }

        // Draw border
        let border_color = if self.focused {
            colors::FOCUS_RING
        } else if self.hovered {
            colors::BORDER_HOVER
        } else {
            colors::INPUT_BORDER
        };
        draw_rect_outline(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            self.box_y(),
            CHECKBOX_BOX,
            CHECKBOX_BOX,
            border_color,
        );

        // Draw check mark if checked
        if self.checked {
            draw_rect(
                buffer,
                buffer_width,
                buffer_height,
                self.x + CHECKBOX_INSET as i32,
                self.box_y() + CHECKBOX_INSET as i32,
                CHECKBOX_BOX - CHECKBOX_INSET * 2,
                CHECKBOX_BOX - CHECKBOX_INSET * 2,
                colors::CHECKBOX_CHECK,
            );
        }

        // Draw label
        let label_x = self.x + CHECKBOX_BOX as i32 + LABEL_GAP as i32;
        let label_y = self.y + (CONTROL_HEIGHT as i32 - text_height() as i32) / 2;
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
