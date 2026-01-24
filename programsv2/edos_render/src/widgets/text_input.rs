//! Single-line text input widget.

use super::{
    char_width, colors, draw_rect, draw_rect_outline, draw_text, text_height, Rect, Widget,
    WidgetEvent, WidgetId,
};

/// A single-line text input field.
pub struct TextInput {
    id: WidgetId,
    x: i32,
    y: i32,
    width: u32,
    text: String,
    cursor_pos: usize,
    focused: bool,
    placeholder: String,
    cursor_visible: bool,
    cursor_blink_counter: u32,
}

const INPUT_HEIGHT: u32 = 24;
const PADDING: u32 = 4;

// Scancodes
const SCANCODE_BACKSPACE: u32 = 14;
const SCANCODE_ENTER: u32 = 28;
const SCANCODE_DELETE: u32 = 83;
const SCANCODE_LEFT: u32 = 75;
const SCANCODE_RIGHT: u32 = 77;
const SCANCODE_HOME: u32 = 71;
const SCANCODE_END: u32 = 79;

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
        self.cursor_pos = self.text.len();
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
        if self.cursor_pos <= self.text.len() {
            self.text.insert(self.cursor_pos, ch);
            self.cursor_pos += 1;
        }
    }

    /// Delete the character before the cursor.
    fn delete_before(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
            self.text.remove(self.cursor_pos);
        }
    }

    /// Delete the character at the cursor.
    fn delete_at(&mut self) {
        if self.cursor_pos < self.text.len() {
            self.text.remove(self.cursor_pos);
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
        if self.cursor_pos < self.text.len() {
            self.cursor_pos += 1;
        }
    }

    /// Move cursor to start.
    fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    /// Move cursor to end.
    fn move_end(&mut self) {
        self.cursor_pos = self.text.len();
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
        Rect::new(self.x, self.y, self.width, INPUT_HEIGHT)
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
            INPUT_HEIGHT,
            colors::INPUT_BG,
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
                INPUT_HEIGHT + 4,
                colors::FOCUS_RING,
            );
        }

        // Draw border
        draw_rect_outline(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            self.y,
            self.width,
            INPUT_HEIGHT,
            colors::INPUT_BORDER,
        );

        let text_x = self.x + PADDING as i32;
        let text_y = self.y + (INPUT_HEIGHT as i32 - text_height() as i32) / 2;

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
                colors::TEXT,
            );
        }

        // Draw cursor if focused and visible
        if self.focused && self.cursor_visible {
            let cursor_x = text_x + (self.cursor_pos as u32 * char_width()) as i32;
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
        if !pressed && self.bounds().contains(x, y) {
            // Click to position cursor
            let text_x = self.x + PADDING as i32;
            let relative_x = (x - text_x).max(0) as u32;
            let char_pos = (relative_x / char_width()) as usize;
            self.cursor_pos = char_pos.min(self.text.len());
            self.reset_cursor_blink();
        }
        None
    }

    fn on_char(&mut self, ch: char) -> Option<WidgetEvent> {
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
        if !self.focused || !pressed {
            return None;
        }

        self.reset_cursor_blink();

        match scancode {
            SCANCODE_BACKSPACE => {
                self.delete_before();
                Some(WidgetEvent::TextChanged(self.text.clone()))
            }
            SCANCODE_DELETE => {
                self.delete_at();
                Some(WidgetEvent::TextChanged(self.text.clone()))
            }
            SCANCODE_ENTER => Some(WidgetEvent::Submit(self.text.clone())),
            SCANCODE_LEFT => {
                self.move_left();
                None
            }
            SCANCODE_RIGHT => {
                self.move_right();
                None
            }
            SCANCODE_HOME => {
                self.move_home();
                None
            }
            SCANCODE_END => {
                self.move_end();
                None
            }
            _ => None,
        }
    }

    fn focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.reset_cursor_blink();
        }
    }
}
