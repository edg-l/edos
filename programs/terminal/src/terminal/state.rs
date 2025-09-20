use alloc::{
    collections::vec_deque::VecDeque,
    format,
    string::{String, ToString},
};
use elibc::{
    KeyEvent,
    graphics::{Color, GraphicsError, RasterHeight, Screen, TextMetrics, TextStyle},
    io::{getcwd, open},
};

pub(crate) const MARGIN: u64 = 10;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Program {
    pub(crate) pid: u64,
}

pub(crate) struct TerminalState {
    pub(super) screen: Screen,
    pub(super) buffer: VecDeque<String>,
    pub(super) input_line: String,
    pub(super) cursor_x: usize,
    pub(super) max_output_lines: usize,
    pub(super) max_cols: usize,
    pub(super) text_style: TextStyle,
    pub(super) prompt_style: TextStyle,
    pub(super) char_width: u64,
    pub(super) char_height: u64,
    pub(super) line_height: u64,
    pub(super) prompt_text: String,
    dirty: bool,
    show_kernel_logs: bool,
    current_dir: String,
    running_program: Option<Program>,
    tty_fd: u64,
}

impl TerminalState {
    pub(crate) fn new() -> Result<Self, GraphicsError> {
        let screen = Screen::get()?;
        let text_style = TextStyle::new(Color::WHITE).with_size(RasterHeight::Size24);
        let prompt_style = TextStyle::new(Color::GREEN).with_size(RasterHeight::Size24);
        let metrics = TextMetrics::for_size(text_style.font_size);

        let horizontal_space = screen.width() as u64 - 2 * MARGIN;
        let max_cols = core::cmp::max(1, (horizontal_space / metrics.char_width) as usize);
        let vertical_space = screen.height() as u64 - 2 * MARGIN - metrics.line_height;
        let max_output_lines = core::cmp::max(1, (vertical_space / metrics.line_height) as usize);

        let mut buffer = VecDeque::new();
        buffer.push_back(String::new());

        let current_dir = getcwd().unwrap_or_else(|_| "/".to_string());
        let prompt_text = format!("{} > ", current_dir);

        let tty_fd = open("/dev/tty0", 0).map_err(GraphicsError::from)?;

        Ok(Self {
            screen,
            buffer,
            input_line: String::new(),
            cursor_x: 0,
            max_output_lines,
            max_cols,
            text_style,
            prompt_style,
            char_width: metrics.char_width,
            char_height: metrics.char_height,
            line_height: metrics.line_height,
            prompt_text,
            dirty: true,
            show_kernel_logs: false,
            current_dir,
            running_program: None,
            tty_fd,
        })
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(crate) fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub(crate) fn logs_enabled(&self) -> bool {
        self.show_kernel_logs
    }

    pub(crate) fn set_logs_enabled(&mut self, enabled: bool) {
        self.show_kernel_logs = enabled;
    }

    pub(crate) fn running_program(&self) -> Option<Program> {
        self.running_program
    }

    pub(crate) fn set_running_program(&mut self, program: Option<Program>) {
        self.running_program = program;
        self.mark_dirty();
    }

    pub(crate) fn tty_fd(&self) -> u64 {
        self.tty_fd
    }

    pub(crate) fn clear_output(&mut self) {
        self.buffer.clear();
        self.buffer.push_back(String::new());
        self.mark_dirty();
    }

    pub(crate) fn write_str(&mut self, text: &str) {
        let mut segments = text.split('\n').peekable();
        while let Some(segment) = segments.next() {
            self.write_fragment(segment);
            if segments.peek().is_some() {
                self.start_new_output_line();
            }
        }
        self.mark_dirty();
    }

    pub(crate) fn write_line(&mut self, text: &str) {
        self.write_str(text);
        self.start_new_output_line();
    }

    pub(crate) fn update_prompt(&mut self) {
        self.current_dir = getcwd().unwrap_or_else(|_| "/".to_string());
        self.prompt_text = format!("{} > ", self.current_dir);
        self.mark_dirty();
    }

    pub(crate) fn finalize_input(&mut self) -> Option<String> {
        let line = core::mem::take(&mut self.input_line);
        self.cursor_x = 0;

        let echoed = format!("{}{}", self.prompt_text, &line);
        self.write_line(&echoed);

        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        Some(line)
    }

    pub(crate) fn insert_input_char(&mut self, ch: char) {
        self.input_line.insert(self.cursor_x, ch);
        self.cursor_x += 1;
        self.mark_dirty();
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor_x > 0 {
            self.cursor_x -= 1;
            if self.cursor_x < self.input_line.len() {
                self.input_line.remove(self.cursor_x);
            }
            self.mark_dirty();
        }
    }

    pub(crate) fn handle_key_event(&mut self, event: KeyEvent) -> Option<String> {
        match event {
            KeyEvent::Unicode(ch) => match ch {
                '\n' | '\r' => self.finalize_input(),
                '\u{08}' | '\u{7F}' => {
                    self.backspace();
                    None
                }
                ch if (' '..='~').contains(&ch) => {
                    self.insert_input_char(ch);
                    None
                }
                _ => None,
            },
            KeyEvent::RawScancode(scancode) => match scancode {
                0x0E => {
                    self.backspace();
                    None
                }
                0x1C => self.finalize_input(),
                _ => None,
            },
        }
    }

    fn write_fragment(&mut self, fragment: &str) {
        if self.buffer.is_empty() {
            self.buffer.push_back(String::new());
        }

        let chars = fragment.chars();
        for ch in chars {
            self.push_output_char(ch);
        }
    }

    fn push_output_char(&mut self, ch: char) {
        if self.buffer.is_empty() {
            self.buffer.push_back(String::new());
        }

        if let Some(last) = self.buffer.back_mut() {
            last.push(ch);
            if last.len() >= self.max_cols {
                self.start_new_output_line();
            }
        }
    }

    fn start_new_output_line(&mut self) {
        self.buffer.push_back(String::new());
        self.trim_excess_lines();
        self.mark_dirty();
    }

    fn trim_excess_lines(&mut self) {
        while self.buffer.len() > self.max_output_lines {
            self.buffer.pop_front();
        }
        if self.buffer.is_empty() {
            self.buffer.push_back(String::new());
        }
    }
}
