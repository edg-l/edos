//! Single-line text input widget.

use edos_lib::keymap::keycode;

use super::{
    Rect, Widget, WidgetEvent, WidgetId, colors, draw_focus_ring, draw_rect, draw_rect_outline,
    draw_text, text_height, text_width,
};
use crate::metrics::{CONTROL_HEIGHT, TEXT_PAD_X};

/// A single-line text input field.
pub struct TextInput {
    id: WidgetId,
    x: i32,
    y: i32,
    width: u32,
    text: String,
    /// Cursor position measured in characters from the start of the text.
    ///
    /// Characters, not bytes: the cursor is drawn at `cursor_pos * char_width`
    /// and moves one step per arrow key, so a byte index would land inside a
    /// multi-byte character and the next `String::insert` would panic on the
    /// boundary assertion. Byte offsets are derived at the point of use.
    cursor_pos: usize,
    focused: bool,
    enabled: bool,
    placeholder: String,
    cursor_visible: bool,
    cursor_blink_counter: u32,
}

impl TextInput {
    /// Create a new text input.
    pub fn new(id: WidgetId, x: i32, y: i32, width: u32) -> Self {
        Self {
            id,
            x,
            y,
            width,
            text: String::new(),
            cursor_pos: 0,
            focused: false,
            enabled: true,
            placeholder: String::new(),
            cursor_visible: true,
            cursor_blink_counter: 0,
        }
    }

    /// Create a text input with placeholder text.
    pub fn with_placeholder(id: WidgetId, x: i32, y: i32, width: u32, placeholder: &str) -> Self {
        Self {
            id,
            x,
            y,
            width,
            text: String::new(),
            cursor_pos: 0,
            focused: false,
            enabled: true,
            placeholder: placeholder.to_string(),
            cursor_visible: true,
            cursor_blink_counter: 0,
        }
    }

    /// Get the current text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Set the text content.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor_pos = self.char_len();
    }

    /// Length of the text in characters.
    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    /// Byte offset of the character the cursor sits on, or the end of the text.
    fn cursor_byte(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor_pos)
            .map_or(self.text.len(), |(offset, _)| offset)
    }

    /// The character position closest to `offset` pixels from the first glyph.
    ///
    /// Rounds to the nearer edge of the character it lands in, so clicking the
    /// right half of a glyph puts the cursor after it.
    fn char_at_offset(&self, offset: u32) -> usize {
        let mut previous = 0;
        for (index, (at, _)) in self.text.char_indices().enumerate() {
            let advance = text_width(&self.text[..at]);
            if advance > offset {
                let midpoint = previous + (advance - previous) / 2;
                return if offset < midpoint { index - 1 } else { index };
            }
            previous = advance;
        }
        let full = text_width(&self.text);
        let midpoint = previous + (full - previous) / 2;
        if self.char_len() > 0 && offset < midpoint {
            self.char_len() - 1
        } else {
            self.char_len()
        }
    }

    /// Get the placeholder text.
    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    /// Set the placeholder text.
    pub fn set_placeholder(&mut self, placeholder: &str) {
        self.placeholder = placeholder.to_string();
    }

    /// Insert a character at the cursor position.
    fn insert_char(&mut self, ch: char) {
        if self.cursor_pos <= self.char_len() {
            let at = self.cursor_byte();
            self.text.insert(at, ch);
            self.cursor_pos += 1;
        }
    }

    /// Delete the character before the cursor.
    fn delete_before(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
            let at = self.cursor_byte();
            self.text.remove(at);
        }
    }

    /// Delete the character at the cursor.
    fn delete_at(&mut self) {
        if self.cursor_pos < self.char_len() {
            let at = self.cursor_byte();
            self.text.remove(at);
        }
    }

    /// Move cursor left.
    fn move_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
        }
    }

    /// Move cursor right.
    fn move_right(&mut self) {
        if self.cursor_pos < self.char_len() {
            self.cursor_pos += 1;
        }
    }

    /// Move cursor to start.
    fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    /// Move cursor to end.
    fn move_end(&mut self) {
        self.cursor_pos = self.char_len();
    }

    /// Reset cursor blink state (make visible).
    fn reset_cursor_blink(&mut self) {
        self.cursor_visible = true;
        self.cursor_blink_counter = 0;
    }
}

impl Widget for TextInput {
    fn id(&self) -> WidgetId {
        self.id
    }

    fn bounds(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, CONTROL_HEIGHT)
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn draw(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32) {
        // Draw background
        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            self.y,
            self.width,
            CONTROL_HEIGHT,
            colors::INPUT_BG,
        );

        // Draw focus ring if focused
        if self.focused {
            draw_focus_ring(
                buffer,
                buffer_width,
                buffer_height,
                self.x,
                self.y,
                self.width,
                CONTROL_HEIGHT,
            );
        }

        // Draw border
        let border_color = if !self.enabled {
            colors::CONTROL_DISABLED
        } else if self.focused {
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
            CONTROL_HEIGHT,
            border_color,
        );

        let text_x = self.x + TEXT_PAD_X as i32;
        let text_y = self.y + (CONTROL_HEIGHT as i32 - text_height() as i32) / 2;

        // Draw text or placeholder
        if self.text.is_empty() && !self.placeholder.is_empty() {
            draw_text(
                buffer,
                buffer_width,
                buffer_height,
                text_x,
                text_y,
                &self.placeholder,
                colors::TEXT_PLACEHOLDER,
            );
        } else {
            draw_text(
                buffer,
                buffer_width,
                buffer_height,
                text_x,
                text_y,
                &self.text,
                if self.enabled {
                    colors::TEXT
                } else {
                    colors::TEXT_DISABLED
                },
            );
        }

        // Draw cursor if focused and visible
        if self.focused && self.cursor_visible {
            let cursor_x = text_x + text_width(&self.text[..self.cursor_byte()]) as i32;
            draw_rect(
                buffer,
                buffer_width,
                buffer_height,
                cursor_x,
                text_y,
                2,
                text_height(),
                colors::TEXT,
            );
        }
    }

    fn on_mouse_move(&mut self, _x: i32, _y: i32) {
        // Text input doesn't need mouse move handling beyond focus
    }

    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent> {
        if !self.enabled {
            return None;
        }
        if !pressed && self.bounds().contains(x, y) {
            // Click to position cursor. The field is set in the proportional
            // face, so the character under the pointer is found by measuring
            // prefixes rather than by dividing by a cell width.
            let text_x = self.x + TEXT_PAD_X as i32;
            let target = (x - text_x).max(0) as u32;
            self.cursor_pos = self.char_at_offset(target);
            self.reset_cursor_blink();
        }
        None
    }

    fn on_char(&mut self, ch: char) -> Option<WidgetEvent> {
        if !self.enabled {
            return None;
        }
        if !self.focused {
            return None;
        }

        // Handle backspace character (sent by keyboard driver as Unicode)
        if ch == '\u{8}' {
            self.delete_before();
            self.reset_cursor_blink();
            return Some(WidgetEvent::TextChanged(self.text.clone()));
        }

        // Only handle printable characters
        if ch.is_control() {
            return None;
        }

        self.insert_char(ch);
        self.reset_cursor_blink();
        Some(WidgetEvent::TextChanged(self.text.clone()))
    }

    fn on_key(&mut self, scancode: u32, pressed: bool) -> Option<WidgetEvent> {
        if !self.enabled {
            return None;
        }
        if !self.focused || !pressed {
            return None;
        }

        self.reset_cursor_blink();

        match scancode {
            keycode::BACKSPACE => {
                self.delete_before();
                Some(WidgetEvent::TextChanged(self.text.clone()))
            }
            keycode::DELETE => {
                self.delete_at();
                Some(WidgetEvent::TextChanged(self.text.clone()))
            }
            keycode::RETURN | keycode::NUMPAD_ENTER => Some(WidgetEvent::Submit(self.text.clone())),
            keycode::ARROW_LEFT => {
                self.move_left();
                None
            }
            keycode::ARROW_RIGHT => {
                self.move_right();
                None
            }
            keycode::HOME => {
                self.move_home();
                None
            }
            keycode::END => {
                self.move_end();
                None
            }
            _ => None,
        }
    }

    /// The whole field. There is no selection here, so a copy takes everything
    /// the field holds rather than nothing.
    fn clipboard_copy(&self) -> Option<String> {
        if !self.enabled || self.text.is_empty() {
            return None;
        }
        Some(self.text.clone())
    }

    fn clipboard_cut(&mut self) -> Option<WidgetEvent> {
        if !self.enabled || self.text.is_empty() {
            return None;
        }
        self.text.clear();
        self.cursor_pos = 0;
        self.reset_cursor_blink();
        Some(WidgetEvent::TextChanged(self.text.clone()))
    }

    fn clipboard_paste(&mut self, text: &str) -> Option<WidgetEvent> {
        if !self.enabled {
            return None;
        }
        // A field is one line, so a newline in the pasted text ends it rather
        // than putting a character the field cannot draw into the middle of it.
        for ch in text.chars().take_while(|ch| *ch != '\n' && *ch != '\r') {
            if !ch.is_control() {
                self.insert_char(ch);
            }
        }
        self.reset_cursor_blink();
        Some(WidgetEvent::TextChanged(self.text.clone()))
    }

    fn focusable(&self) -> bool {
        // A disabled control is skipped by Tab: focusing something that cannot
        // act is a dead end the user has to Tab out of again.
        self.enabled
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.focused = false;
        }
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.reset_cursor_blink();
        }
    }
}
