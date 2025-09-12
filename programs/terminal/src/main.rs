#![no_std]
#![no_main]

use alloc::{format, string::String, vec::Vec};
use elibc::{
    KeyEvent, get_raw_input,
    graphics::{Color, RasterHeight, Screen, TextMetrics, TextStyle},
    io::get_kernel_logs,
    sys_exit,
};

extern crate alloc;

const MARGIN: u64 = 10;

fn parse_command(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current_arg = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
            }
            q if in_quotes && q == quote_char => {
                in_quotes = false;
            }
            '\\' if in_quotes => {
                // Handle escaped characters within quotes
                if let Some(escaped) = chars.next() {
                    current_arg.push(escaped);
                }
            }
            ' ' | '\t' if !in_quotes => {
                if !current_arg.is_empty() {
                    args.push(current_arg);
                    current_arg = String::new();
                }
            }
            _ => {
                current_arg.push(ch);
            }
        }
    }

    // Add the last argument if it's not empty
    if !current_arg.is_empty() {
        args.push(current_arg);
    }

    args
}

#[derive(Clone, Debug)]
enum LineType {
    Input,  // Lines where user types commands (show prompt)
    Output, // Lines with command output (no prompt)
}

struct Terminal {
    screen: Screen,
    buffer: Vec<String>,
    line_types: Vec<LineType>,
    cursor_x: usize,
    cursor_y: usize,
    max_lines: usize,
    max_cols: usize,
    text_style: TextStyle,
    char_width: u64,
    char_height: u64,
    line_height: u64,
    prompt_text: String,
    prompt_style: TextStyle,
    // Performance optimization fields
    is_dirty: bool,
    enable_kernel_logs: bool,
}

impl Terminal {
    fn new() -> Result<Self, elibc::graphics::GraphicsError> {
        let screen = Screen::get()?;
        let text_style = TextStyle::new(Color::WHITE).with_size(RasterHeight::Size24);
        let prompt_style = TextStyle::new(Color::GREEN).with_size(RasterHeight::Size24);

        let metrics = TextMetrics::for_size(text_style.font_size);

        let max_cols = ((screen.width() as u64 - 2 * MARGIN) / metrics.char_width) as usize;
        let max_lines = ((screen.height() as u64 - 2 * MARGIN) / metrics.line_height) as usize;

        let buffer = alloc::vec![String::new()];
        let line_types = alloc::vec![LineType::Input];

        let prompt_text = String::from("> ");

        Ok(Terminal {
            screen,
            buffer,
            line_types,
            cursor_x: 0, // Start at beginning of input area
            cursor_y: 0,
            max_lines,
            max_cols,
            text_style,
            char_width: metrics.char_width,
            char_height: metrics.char_height,
            line_height: metrics.line_height,
            prompt_text,
            prompt_style,
            is_dirty: true, // Start dirty to force initial render
            enable_kernel_logs: true,
        })
    }

    fn render(&mut self) -> Result<(), elibc::graphics::GraphicsError> {
        // Only render if content has changed
        if !self.is_dirty {
            return Ok(());
        }

        // Clear screen with black background
        self.screen.fill(Color::BLACK)?;

        // Render each line of text
        for (line_idx, line_str) in self.buffer.iter().enumerate() {
            if line_idx >= self.max_lines {
                break; // Don't render lines that would be off-screen
            }

            let y_pos = (line_idx as u64) * self.line_height + MARGIN;

            // Get line type (default to Input if index out of bounds)
            let line_type = self.line_types.get(line_idx).unwrap_or(&LineType::Input);

            match line_type {
                LineType::Input => {
                    // Render prompt and text after it
                    self.screen
                        .draw_text(MARGIN, y_pos, &self.prompt_text, &self.prompt_style)?;

                    if !line_str.is_empty() {
                        let prompt_width = (self.prompt_text.len() as u64) * self.char_width;
                        self.screen.draw_text(
                            prompt_width + MARGIN,
                            y_pos,
                            line_str,
                            &self.text_style,
                        )?;
                    }
                }
                LineType::Output => {
                    // Render only text, no prompt
                    if !line_str.is_empty() {
                        self.screen
                            .draw_text(MARGIN, y_pos, line_str, &self.text_style)?;
                    }
                }
            }
        }

        // Draw cursor
        self.draw_cursor()?;

        // Present the rendered frame
        self.screen.render()?;

        // Clear dirty flag after successful render
        self.is_dirty = false;

        Ok(())
    }

    fn mark_dirty(&mut self) {
        self.is_dirty = true;
    }

    fn draw_cursor(&mut self) -> Result<(), elibc::graphics::GraphicsError> {
        // Check line type to determine cursor positioning
        let line_type = self
            .line_types
            .get(self.cursor_y)
            .unwrap_or(&LineType::Input);

        let cursor_x_pos = match line_type {
            LineType::Input => {
                // Account for prompt offset when positioning cursor
                let prompt_width = (self.prompt_text.len() as u64) * self.char_width;
                prompt_width + (self.cursor_x as u64) * self.char_width + MARGIN
            }
            LineType::Output => {
                // No prompt offset for output lines
                (self.cursor_x as u64) * self.char_width + MARGIN
            }
        };

        let cursor_y_pos = (self.cursor_y as u64) * self.line_height + MARGIN;

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
            self.buffer.push(String::new());
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

        // Handle line wrapping (account for prompt taking up space)
        let effective_max_cols = self.max_cols - self.prompt_text.len();
        if self.cursor_x >= effective_max_cols {
            self.new_line();
        }

        // Mark terminal as dirty since content changed
        self.mark_dirty();
    }

    fn new_line(&mut self) {
        self.new_line_with_type(LineType::Input);
    }

    fn new_line_with_type(&mut self, line_type: LineType) {
        self.cursor_y += 1;
        self.cursor_x = 0; // Reset cursor to beginning of line

        // Add new line to buffer if needed
        if self.cursor_y >= self.buffer.len() {
            self.buffer.push(String::new());
            self.line_types.push(line_type);
        } else {
            // Update existing line type
            if self.cursor_y < self.line_types.len() {
                self.line_types[self.cursor_y] = line_type;
            } else {
                self.line_types.push(line_type);
            }
        }

        // Handle scrolling if we exceed max lines
        if self.cursor_y >= self.max_lines {
            self.scroll_up();
        }

        // Mark terminal as dirty since layout changed
        self.mark_dirty();
    }

    fn backspace(&mut self) {
        if self.cursor_x > 0 {
            // Remove character before cursor on current line
            self.cursor_x -= 1;
            if self.cursor_x < self.buffer[self.cursor_y].len() {
                self.buffer[self.cursor_y].remove(self.cursor_x);
            }
        } /*else if self.cursor_y > 0 {
        // Move to end of previous line and join lines
        let current_line = self.buffer[self.cursor_y].clone();
        self.buffer.remove(self.cursor_y);
        self.cursor_y -= 1;
        self.cursor_x = self.buffer[self.cursor_y].len();
        self.buffer[self.cursor_y].push_str(&current_line);
        }*/

        // Mark terminal as dirty since content changed
        self.mark_dirty();
    }

    fn scroll_up(&mut self) {
        // Remove first line and adjust cursor position
        if !self.buffer.is_empty() {
            self.buffer.remove(0);
            if !self.line_types.is_empty() {
                self.line_types.remove(0);
            }
            if self.cursor_y > 0 {
                self.cursor_y -= 1;
            }
        }

        // Mark terminal as dirty since content scrolled
        self.mark_dirty();
    }

    fn print_text(&mut self, text: &str) {
        // Mark current line as Output (where we'll start printing)
        if self.cursor_y < self.line_types.len() {
            self.line_types[self.cursor_y] = LineType::Output;
        }

        let lines: Vec<&str> = text.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            // Add text to current line
            for ch in line.chars() {
                self.insert_char_at_end(ch);
            }

            // If this isn't the last line, create a new Output line
            if i < lines.len() - 1 {
                self.new_line_with_type(LineType::Output);
            }
        }

        // Mark terminal as dirty since content was added
        self.mark_dirty();
    }

    fn insert_char_at_end(&mut self, ch: char) {
        // Ensure we have enough lines in the buffer
        while self.buffer.len() <= self.cursor_y {
            self.buffer.push(String::new());
        }

        // Add character to end of current line
        self.buffer[self.cursor_y].push(ch);
        self.cursor_x += 1;

        // Handle line wrapping (account for prompt taking up space for Input lines)
        let line_type = self
            .line_types
            .get(self.cursor_y)
            .unwrap_or(&LineType::Input);
        let effective_max_cols = match line_type {
            LineType::Input => self.max_cols - self.prompt_text.len(),
            LineType::Output => self.max_cols,
        };

        if self.cursor_x >= effective_max_cols {
            // For Output lines, wrap with Output type; for Input lines, wrap with Input type
            self.new_line_with_type(line_type.clone());
        }

        // Note: We don't mark dirty here as this is called from other functions that will mark dirty
    }

    fn on_command(&mut self, command: &str, args: &[String]) {
        match command {
            "logs" => {
                if args.is_empty() || !["on", "off"].contains(&args[0].as_str()) {
                    self.print_text("Usage: logs [on|off]");
                }

                self.enable_kernel_logs = args[0] == "on";
            }
            "help" => {
                self.print_text("Commands:\n");
                self.print_text("- help\n");
                self.print_text("- logs");
            }
            _ => {
                self.print_text(&format!("Unknown command {command}"));
            }
        }
    }

    fn process_current_line(&mut self) {
        // Get current line content
        let current_line: String = if self.cursor_y < self.buffer.len() {
            self.buffer[self.cursor_y].clone()
        } else {
            String::new()
        };

        // Create a new line for the output
        self.new_line_with_type(LineType::Output);

        // Parse and process the command if it's not empty
        let trimmed = current_line.trim();
        if !trimmed.is_empty() {
            let args = parse_command(trimmed);
            if let Some(command) = args.first() {
                let command_args = &args[1..];
                self.on_command(command, command_args);
            }
        }

        // Create a new Input line for the next command
        self.new_line_with_type(LineType::Input);
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event {
            KeyEvent::Unicode(ch) => {
                match ch {
                    '\n' | '\r' => {
                        // Enter key - process command
                        self.process_current_line();
                    }
                    '\u{08}' | '\u{7F}' => {
                        // Backspace or DEL key
                        self.backspace();
                    }
                    ch if (' '..='~').contains(&ch) => {
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
                        self.process_current_line();
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

    let mut key_buffer = Vec::new();

    loop {
        // Get raw input with a small timeout to avoid blocking indefinitely

        if terminal.enable_kernel_logs {
            let logs = get_kernel_logs();

            if !logs.is_empty() {
                for log in logs {
                    terminal.print_text(&log);
                }
            }
        }

        get_raw_input(20, &mut key_buffer, 16);

        if !key_buffer.is_empty() {
            // Process the key event

            for key_event in &key_buffer {
                terminal.handle_key_event(*key_event);
            }

            key_buffer.clear();

            // Re-render the terminal immediately for instant feedback
            if terminal.render().is_err() {
                return 1;
            }
        }
    }
}
