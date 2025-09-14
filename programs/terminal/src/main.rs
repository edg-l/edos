#![no_std]
#![no_main]

use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use elibc::{
    KeyEvent, get_raw_input,
    graphics::{Color, RasterHeight, Screen, TextMetrics, TextStyle},
    io::{
        FileType, chdir, get_kernel_logs, getcwd, list_dir, open, open_flags, read_from_fd,
        read_to_end, write_all_fd,
    },
    pipe,
    process::sys_waitpid,
    spawn, sys_close,
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

struct Terminal {
    screen: Screen,
    // Output lines only (printed program output and echoed commands)
    buffer: Vec<String>,
    // Input state (rendered in a reserved bottom margin)
    input_line: String,
    cursor_x: usize,
    // Layout
    max_output_lines: usize,
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
    current_dir: String,
    running_program: Option<Program>,
}

#[derive(Debug, Clone, Copy)]
struct Program {
    pub pid: u64,
    pub read_fd: u64,
}

impl Terminal {
    fn new() -> Result<Self, elibc::graphics::GraphicsError> {
        let screen = Screen::get()?;
        let text_style = TextStyle::new(Color::WHITE).with_size(RasterHeight::Size24);
        let prompt_style = TextStyle::new(Color::GREEN).with_size(RasterHeight::Size24);

        let metrics = TextMetrics::for_size(text_style.font_size);

        // Columns across full width minus side margins
        let max_cols = ((screen.width() as u64 - 2 * MARGIN) / metrics.char_width) as usize;
        // Reserve one text line at the bottom for the input area
        let max_output_lines = ((screen.height() as u64 - 2 * MARGIN - metrics.line_height)
            / metrics.line_height) as usize;

        let buffer = alloc::vec![String::new()];

        // Get current working directory
        let current_dir = getcwd().unwrap_or_else(|_| "/".to_string());
        let prompt_text = format!("{} > ", current_dir);

        Ok(Terminal {
            screen,
            buffer,
            input_line: String::new(),
            cursor_x: 0, // Input cursor X within input_line
            max_output_lines,
            max_cols,
            text_style,
            char_width: metrics.char_width,
            char_height: metrics.char_height,
            line_height: metrics.line_height,
            prompt_text,
            prompt_style,
            is_dirty: true, // Start dirty to force initial render
            enable_kernel_logs: true,
            current_dir,
            running_program: None,
        })
    }

    fn render(&mut self) -> Result<(), elibc::graphics::GraphicsError> {
        // Only render if content has changed
        if !self.is_dirty {
            return Ok(());
        }

        // Clear screen with black background
        self.screen.fill(Color::BLACK)?;

        // Render output area (top to just above the input line)
        for (line_idx, line_str) in self.buffer.iter().enumerate() {
            if line_idx >= self.max_output_lines {
                break; // Don't render lines that would be off-screen
            }

            let y_pos = (line_idx as u64) * self.line_height + MARGIN;

            if !line_str.is_empty() {
                self.screen
                    .draw_text(MARGIN, y_pos, line_str, &self.text_style)?;
            }
        }

        // Render input line in reserved bottom margin
        let input_y = (self.screen.height() as u64) - self.line_height - MARGIN;
        // Prompt
        self.screen
            .draw_text(MARGIN, input_y, &self.prompt_text, &self.prompt_style)?;
        // Input text
        if !self.input_line.is_empty() {
            let prompt_width = (self.prompt_text.len() as u64) * self.char_width;
            self.screen.draw_text(
                prompt_width + MARGIN,
                input_y,
                &self.input_line,
                &self.text_style,
            )?;
        }

        // Draw input cursor
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

    fn update_prompt(&mut self) {
        self.current_dir = getcwd().unwrap_or_else(|_| "/".to_string());
        self.prompt_text = format!("{} > ", self.current_dir);
        self.mark_dirty();
    }

    fn draw_cursor(&mut self) -> Result<(), elibc::graphics::GraphicsError> {
        // Cursor is always on the bottom input line
        let prompt_width = (self.prompt_text.len() as u64) * self.char_width;
        let cursor_x_pos = prompt_width + (self.cursor_x as u64) * self.char_width + MARGIN;
        let cursor_y_pos = (self.screen.height() as u64) - self.line_height - MARGIN;

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
        // Insert character at input cursor position
        if self.cursor_x < self.input_line.len() {
            self.input_line.insert(self.cursor_x, ch);
        } else {
            // Extend line if cursor is beyond current input length
            while self.input_line.len() < self.cursor_x {
                self.input_line.push(' ');
            }
            self.input_line.push(ch);
        }

        // Move input cursor right
        self.cursor_x += 1;

        // Mark terminal as dirty since content changed
        self.mark_dirty();
    }

    fn add_output_newline(&mut self) {
        self.buffer.push(String::new());
        // Handle scrolling if we exceed max output lines
        while self.buffer.len() > self.max_output_lines {
            self.buffer.remove(0);
        }
        self.mark_dirty();
    }

    fn backspace(&mut self) {
        if self.cursor_x > 0 {
            // Remove character before cursor in the input line
            self.cursor_x -= 1;
            if self.cursor_x < self.input_line.len() {
                self.input_line.remove(self.cursor_x);
            }
        }

        // Mark terminal as dirty since content changed
        self.mark_dirty();
    }

    fn print_text(&mut self, text: &str) {
        let lines: Vec<&str> = text.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            // Ensure there's at least one output line to write to
            if self.buffer.is_empty() {
                self.buffer.push(String::new());
            }

            // Add text to last output line with wrapping
            for ch in line.chars() {
                self.insert_char_at_end(ch);
            }

            // If this isn't the last line, create a new Output line
            if i < lines.len() - 1 {
                self.add_output_newline();
            }
        }

        // Mark terminal as dirty since content was added
        self.mark_dirty();
    }

    fn insert_char_at_end(&mut self, ch: char) {
        // Ensure we have at least one output line
        if self.buffer.is_empty() {
            self.buffer.push(String::new());
        }

        // Add character to end of the last output line
        let last_idx = self.buffer.len() - 1;
        self.buffer[last_idx].push(ch);

        // Handle line wrapping for output lines
        if self.buffer[last_idx].len() >= self.max_cols {
            self.add_output_newline();
        }
        // Note: We don't mark dirty here as callers will mark dirty
    }

    fn try_spawn_program(&mut self, command: &str, _args: &[String]) {
        // Create a pipe to capture the program's stdout
        let (read_fd, write_fd) = match pipe() {
            Some(fds) => fds,
            None => {
                self.print_text(&format!("Failed to create pipe for {command}\n"));
                return;
            }
        };

        // Try absolute path first, then relative in current directory
        let paths_to_try = vec![
            format!("/{}", command),         // Try as absolute path
            format!("./{}", command),        // Try in current directory
            format!("/bin/{}", command),     // Try in /bin
            format!("/usr/bin/{}", command), // Try in /usr/bin
        ];

        for path in &paths_to_try {
            // Attempt to spawn the program with stdout redirected to our pipe
            let child_pid = spawn(path, 0, write_fd, 2); // stdin=0, stdout=pipe, stderr=2

            if child_pid != u64::MAX {
                // self.print_text(&format!("Started {} (PID: {})\n", command, child_pid));
                let _ = sys_close(write_fd);
                self.running_program = Some(Program {
                    pid: child_pid,
                    read_fd,
                });
                return;
            }
        }

        self.print_text(&format!("Command not found: {}\n", command));

        // Clean up pipe if we couldn't spawn
        let _ = sys_close(read_fd);
        let _ = sys_close(write_fd);
    }

    fn read_and_display_output(&mut self, read_fd: u64) {
        let mut buffer = [0u8; 1024];

        loop {
            match read_from_fd(read_fd, &mut buffer) {
                Ok(0) => break, // EOF
                Ok(bytes_read) => {
                    if let Ok(text) = core::str::from_utf8(&buffer[..bytes_read]) {
                        self.print_text(text);
                    }
                }
                Err(e) => {
                    self.print_text(&format!("Read error: {e:?}\n"));
                    break;
                } // Read error
            }
        }
    }

    fn on_command(&mut self, command: &str, args: &[String]) {
        match command {
            "logs" => {
                if args.is_empty() || !["on", "off"].contains(&args[0].as_str()) {
                    self.print_text("Usage: logs [on|off]\n");
                }

                self.enable_kernel_logs = args[0] == "on";
            }
            "help" => {
                self.print_text("Commands:\n");
                self.print_text("- help\n");
                self.print_text("- logs\n");
                self.print_text("- pwd\n");
                self.print_text("- cd [path]\n");
                self.print_text("- ls [path]\n");
                self.print_text("- cat <path>\n");
                self.print_text("- write <path> <content>\n");
            }
            "pwd" => match getcwd() {
                Ok(cwd) => self.print_text(&format!("{}\n", cwd)),
                Err(_) => self.print_text("pwd: error getting current directory\n"),
            },
            "cd" => {
                let target_path = if args.is_empty() { "/" } else { &args[0] };

                match chdir(target_path) {
                    Ok(()) => {
                        self.update_prompt();
                    }
                    Err(_) => {
                        self.print_text(&format!(
                            "cd: {}: No such file or directory\n",
                            target_path
                        ));
                    }
                }
            }
            "ls" => {
                let path = if args.is_empty() { "." } else { &args[0] };

                match list_dir(path) {
                    Ok(entries) => {
                        if entries.is_empty() {
                            self.print_text("[empty directory]\n");
                        } else {
                            for entry in entries {
                                let type_char = match entry.file_type {
                                    FileType::File => ' ',
                                    FileType::Directory => '/',
                                    FileType::Symlink => '@',
                                    FileType::Special => '*',
                                };

                                let size_str = if entry.file_type == FileType::Directory {
                                    "".to_string()
                                } else {
                                    format!("{:9}", entry.size)
                                };

                                self.print_text(&format!(
                                    "{}{} {}\n",
                                    entry.name, type_char, size_str
                                ));
                            }
                        }
                    }
                    Err(_) => {
                        self.print_text(&format!(
                            "ls: cannot access '{}': No such file or directory\n",
                            path
                        ));
                    }
                }
            }
            "cat" => {
                if args.len() != 1 {
                    self.print_text("Usage: cat <path>\n");
                } else {
                    let path = &args[0];
                    match open(path, 0) {
                        Ok(fd) => {
                            match read_to_end(fd, Some(16 * 1024)) {
                                Ok(data) => {
                                    let text =
                                        core::str::from_utf8(&data).unwrap_or("[non-utf8 data]\n");
                                    self.print_text(text);
                                    self.print_text("\n");
                                }
                                Err(_) => self.print_text("cat: read error\n"),
                            }
                            let _ = sys_close(fd);
                        }
                        Err(_) => self.print_text("cat: open failed\n"),
                    }
                }
            }
            "write" => {
                if args.len() < 2 {
                    self.print_text("Usage: write <path> <content>\n");
                } else {
                    let path = &args[0];
                    let content = args[1..].join(" ");
                    match open(path, open_flags::O_APPEND | open_flags::O_CREAT) {
                        Ok(fd) => {
                            match write_all_fd(fd, content.as_bytes()) {
                                Ok(()) => self.print_text("[ok]\n"),
                                Err(_) => self.print_text("write: error\n"),
                            }
                            let _ = sys_close(fd);
                        }
                        Err(_) => self.print_text("write: open failed\n"),
                    }
                }
            }
            _ => {
                // Try to spawn the command as an external program
                self.try_spawn_program(command, args);
            }
        }
    }

    fn process_current_line(&mut self) {
        // Echo the command into the output buffer (typical terminal behavior)
        let echoed = format!("{}{}", self.prompt_text, self.input_line);
        if !echoed.is_empty() {
            // If the current last output line is empty, write there, otherwise create a new one
            if self.buffer.is_empty() || !self.buffer.last().unwrap().is_empty() {
                self.add_output_newline();
            }
            if self.buffer.is_empty() {
                self.buffer.push(String::new());
            }
            let last_idx = self.buffer.len() - 1;
            self.buffer[last_idx].push_str(&echoed);
            // Start subsequent output on a new line
            self.add_output_newline();
        }

        // Parse and process the command if it's not empty
        let trimmed = self.input_line.trim();
        if !trimmed.is_empty() {
            let args = parse_command(trimmed);
            if let Some(command) = args.first() {
                let command_args = &args[1..];
                self.on_command(command, command_args);
            }
        }

        // Clear input for the next command
        self.input_line.clear();
        self.cursor_x = 0;
        self.mark_dirty();
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

        if let Some(program) = terminal.running_program {
            // Read output from the pipe and display it
            terminal.read_and_display_output(program.read_fd);
            if !sys_waitpid(program.pid, false) {
                // Close our copy of the write end since child has it
                let _ = sys_close(program.read_fd);
                terminal.running_program = None;
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
