#![no_std]
#![no_main]

use alloc::{string::String, vec::Vec};
use elibc::{
    KeyEvent, get_raw_input,
    graphics::{Color, RasterHeight, Screen, TextMetrics, TextStyle},
};

extern crate alloc;

struct Terminal {
    screen: Screen,
    buffer: Vec<Vec<char>>,
    cursor_x: usize,
    cursor_y: usize,
    max_lines: usize,
    max_cols: usize,
    text_style: TextStyle,
    char_width: u64,
    char_height: u64,
    line_height: u64,
}

impl Terminal {
    fn new() -> Result<Self, elibc::graphics::GraphicsError> {
        let screen = Screen::get()?;
        let text_style = TextStyle::new(Color::WHITE).with_size(RasterHeight::Size24);

        let metrics = TextMetrics::for_size(text_style.font_size);

        let max_cols = (screen.width() as u64 / metrics.char_width) as usize;
        let max_lines = (screen.height() as u64 / metrics.line_height) as usize;

        let mut buffer = Vec::new();
        buffer.push(Vec::new()); // Start with one empty line

        Ok(Terminal {
            screen,
            buffer,
            cursor_x: 0,
            cursor_y: 0,
            max_lines,
            max_cols,
            text_style,
            char_width: metrics.char_width,
            char_height: metrics.char_height,
            line_height: metrics.line_height,
        })
    }

    fn render(&self) -> Result<(), elibc::graphics::GraphicsError> {
        // Clear screen with black background
        self.screen.fill(Color::BLACK)?;

        // Render each line of text
        for (line_idx, line) in self.buffer.iter().enumerate() {
            if line_idx >= self.max_lines {
                break; // Don't render lines that would be off-screen
            }

            let y_pos = (line_idx as u64) * self.line_height;

            // Convert line to string and render it
            let line_str: String = line.iter().collect();
            if !line_str.is_empty() {
                self.screen
                    .draw_text(0, y_pos, &line_str, &self.text_style)?;
            }
        }

        // Draw cursor
        self.draw_cursor()?;

        // Present the rendered frame
        self.screen.render()?;
        Ok(())
    }

    fn draw_cursor(&self) -> Result<(), elibc::graphics::GraphicsError> {
        let cursor_x_pos = (self.cursor_x as u64) * self.char_width;
        let cursor_y_pos = (self.cursor_y as u64) * self.line_height;

        // Draw a simple vertical line as cursor
        self.screen.draw_rect(
            cursor_x_pos,
            cursor_y_pos,
            2, // 2 pixel wide cursor
            self.char_height,
            Color::WHITE,
        )?;

        Ok(())
    }

    fn insert_char(&mut self, ch: char) {
        // Ensure we have enough lines in the buffer
        while self.buffer.len() <= self.cursor_y {
            self.buffer.push(Vec::new());
        }

        // Insert character at cursor position
        if self.cursor_x < self.buffer[self.cursor_y].len() {
            self.buffer[self.cursor_y].insert(self.cursor_x, ch);
        } else {
            // Extend line if cursor is beyond current line length
            while self.buffer[self.cursor_y].len() < self.cursor_x {
                self.buffer[self.cursor_y].push(' ');
            }
            self.buffer[self.cursor_y].push(ch);
        }

        // Move cursor right
        self.cursor_x += 1;

        // Handle line wrapping
        if self.cursor_x >= self.max_cols {
            self.new_line();
        }
    }

    fn new_line(&mut self) {
        self.cursor_y += 1;
        self.cursor_x = 0;

        // Add new line to buffer if needed
        if self.cursor_y >= self.buffer.len() {
            self.buffer.push(Vec::new());
        }

        // Handle scrolling if we exceed max lines
        if self.cursor_y >= self.max_lines {
            self.scroll_up();
        }
    }

    fn backspace(&mut self) {
        if self.cursor_x > 0 {
            // Remove character before cursor on current line
            self.cursor_x -= 1;
            if self.cursor_x < self.buffer[self.cursor_y].len() {
                self.buffer[self.cursor_y].remove(self.cursor_x);
            }
        } else if self.cursor_y > 0 {
            // Move to end of previous line and join lines
            let current_line = self.buffer[self.cursor_y].clone();
            self.buffer.remove(self.cursor_y);
            self.cursor_y -= 1;
            self.cursor_x = self.buffer[self.cursor_y].len();
            self.buffer[self.cursor_y].extend(current_line);
        }
    }

    fn scroll_up(&mut self) {
        // Remove first line and adjust cursor position
        if !self.buffer.is_empty() {
            self.buffer.remove(0);
            if self.cursor_y > 0 {
                self.cursor_y -= 1;
            }
        }
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event {
            KeyEvent::Unicode(ch) => {
                match ch {
                    '\n' | '\r' => {
                        // Enter key - new line
                        self.new_line();
                    }
                    '\u{08}' | '\u{7F}' => {
                        // Backspace or DEL key
                        self.backspace();
                    }
                    ch if ch >= ' ' && ch <= '~' => {
                        // Printable ASCII characters
                        self.insert_char(ch);
                    }
                    _ => {
                        // Ignore other unicode characters for now
                    }
                }
            }
            KeyEvent::RawScancode(scancode) => {
                // Handle special keys based on scancode
                match scancode {
                    0x0E => {
                        // Backspace scancode (make code)
                        self.backspace();
                    }
                    0x1C => {
                        // Enter scancode (make code)
                        self.new_line();
                    }
                    _ => {
                        // Ignore other scancodes for now
                    }
                }
            }
        }
    }
}

// This will be called by elibc's _start function
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut terminal = match Terminal::new() {
        Ok(term) => term,
        Err(_) => return 1,
    };

    // Initial render
    if terminal.render().is_err() {
        return 1;
    }

    // Main terminal loop
    loop {
        // Get raw input with a small timeout to avoid blocking indefinitely
        if let Some(key_event) = get_raw_input(10) {
            // Process the key event
            terminal.handle_key_event(key_event);

            // Re-render the terminal immediately for instant feedback
            if terminal.render().is_err() {
                return 1;
            }
        }
        // Continue looping even if no input (timeout occurred)
    }
}
