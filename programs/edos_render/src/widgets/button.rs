//! Clickable button widget.

use super::{FocusState, Rect, Widget, WidgetEvent, colors, text_height, text_width};
use crate::metrics::{CONTROL_HEIGHT, CONTROL_PAD_X};
use crate::surface::Surface;
use crate::text::Style;
use edos_lib::keymap::keycode;

/// A clickable button with a label.
pub struct Button {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    label: String,
    hovered: bool,
    pressed: bool,
    focus: FocusState,
}

impl Button {
    /// Create a new button that auto-sizes based on text.
    pub fn new(x: i32, y: i32, label: &str) -> Self {
        let width = text_width(label) + CONTROL_PAD_X * 2;

        Self {
            x,
            y,
            width,
            height: CONTROL_HEIGHT,
            label: label.to_string(),
            hovered: false,
            pressed: false,
            focus: FocusState::default(),
        }
    }

    /// Create a new button with explicit size.
    pub fn with_size(x: i32, y: i32, width: u32, height: u32, label: &str) -> Self {
        Self {
            x,
            y,
            width,
            height,
            label: label.to_string(),
            hovered: false,
            pressed: false,
            focus: FocusState::default(),
        }
    }

    pub fn set_label(&mut self, label: &str) {
        self.label = label.to_string();
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

impl Widget for Button {
    fn bounds(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn draw(&self, surface: &mut Surface<'_>) {
        // Choose background color based on state
        if !self.focus.enabled {
            surface.rect(
                self.x,
                self.y,
                self.width,
                self.height,
                colors::CONTROL_DISABLED,
            );
            surface.rect_outline(
                self.x,
                self.y,
                self.width,
                self.height,
                colors::CONTROL_DISABLED,
            );
            let label_w = text_width(&self.label);
            surface.text(
                self.x + (self.width as i32 - label_w as i32) / 2,
                self.y + (self.height as i32 - text_height() as i32) / 2,
                &self.label,
                Style::new(colors::TEXT_DISABLED),
            );
            return;
        }

        let bg_color = if self.pressed {
            colors::BUTTON_PRESSED
        } else if self.hovered {
            colors::BUTTON_HOVER
        } else {
            colors::BUTTON_NORMAL
        };

        // Draw button background
        surface.rect(self.x, self.y, self.width, self.height, bg_color);

        // Draw focus ring if focused
        if self.focus.focused {
            surface.focus_ring(self.x, self.y, self.width, self.height);
        }

        // Draw border (subtle, matches dark theme)
        let border_color = if self.pressed || self.hovered {
            colors::BORDER_HOVER
        } else {
            colors::INPUT_BORDER
        };
        surface.rect_outline(self.x, self.y, self.width, self.height, border_color);

        // Center the text
        let label_w = text_width(&self.label);
        let text_x = self.x + (self.width as i32 - label_w as i32) / 2;
        let text_y = self.y + (self.height as i32 - text_height() as i32) / 2;

        surface.text(text_x, text_y, &self.label, Style::new(colors::TEXT));
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        if !self.focus.enabled {
            return;
        }
        self.hovered = self.bounds().contains(x, y);
        // Release press if mouse moves outside while pressed
        if !self.hovered {
            self.pressed = false;
        }
    }

    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent> {
        if !self.focus.enabled {
            return None;
        }
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
        // Space or Enter activates the button when focused.
        if self.focus.enabled
            && self.focus.focused
            && pressed
            && matches!(
                scancode,
                keycode::RETURN | keycode::NUMPAD_ENTER | keycode::SPACEBAR
            )
        {
            Some(WidgetEvent::Clicked)
        } else {
            None
        }
    }

    fn focus_state(&self) -> Option<&FocusState> {
        Some(&self.focus)
    }

    fn focus_state_mut(&mut self) -> Option<&mut FocusState> {
        Some(&mut self.focus)
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.focus.set_enabled(enabled);
        if !enabled {
            self.hovered = false;
            self.pressed = false;
        }
    }
}
