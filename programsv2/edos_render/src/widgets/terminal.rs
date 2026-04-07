//! Terminal widget for displaying text with cursor and scrolling support.

use std::collections::VecDeque;

use super::{Rect, Widget, WidgetEvent, WidgetId, char_width, draw_rect, draw_text, text_height};

#[derive(Debug, Clone, Copy, PartialEq)]
enum EscState {
    Normal,
    Escape, // saw ESC (0x1B)
    Csi,    // saw ESC [
}

/// Default terminal colors
pub mod terminal_colors {
    use crate::theme::Theme;
    pub const BACKGROUND: u32 = Theme::DEFAULT.terminal_bg.raw();
    pub const FOREGROUND: u32 = Theme::DEFAULT.terminal_fg.raw();
    pub const CURSOR: u32 = Theme::DEFAULT.terminal_cursor.raw();
    pub const SELECTION: u32 = Theme::DEFAULT.terminal_selection.raw();
}

/// Standard 16 ANSI colors (Ayu Dark palette).
const ANSI_COLORS: [u32; 16] = [
    0xFF0A0E14, // 0: Black
    0xFFF07178, // 1: Red
    0xFFAAD94C, // 2: Green
    0xFFE6B450, // 3: Yellow
    0xFF59C2FF, // 4: Blue
    0xFFD2A6FF, // 5: Magenta
    0xFF95E6CB, // 6: Cyan
    0xFFBFBDB6, // 7: White
    0xFF565B66, // 8: Bright Black
    0xFFFF6B6B, // 9: Bright Red
    0xFFC2E78C, // 10: Bright Green
    0xFFFFB454, // 11: Bright Yellow
    0xFF73D0FF, // 12: Bright Blue
    0xFFDFBFFF, // 13: Bright Magenta
    0xFFB8E4C9, // 14: Bright Cyan
    0xFFFFFFFF, // 15: Bright White
];

/// A single terminal cell with character and color attributes.
#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    fg: u32,
    bg: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: terminal_colors::FOREGROUND,
            bg: terminal_colors::BACKGROUND,
        }
    }
}

/// A terminal widget that displays a text buffer with cursor.
#[allow(dead_code)]
pub struct Terminal {
    id: WidgetId,
    x: i32,
    y: i32,
    width: u32,
    height: u32,

    // Text buffer (rows x cols of cells)
    buffer: VecDeque<Vec<Cell>>,
    cols: usize,
    rows: usize,

    // Cursor position
    cursor_row: usize,
    cursor_col: usize,
    cursor_visible: bool,
    cursor_blink_counter: u32,

    // Scroll position
    scroll_offset: usize,
    history: VecDeque<Vec<Cell>>,
    max_history: usize,

    // State
    focused: bool,
    bg_color: u32,
    fg_color: u32,

    // Current pen attributes (applied to new cells)
    current_fg: u32,
    current_bg: u32,
    bold: bool,

    // Modifier key state
    modifiers: edos_lib::keymap::Modifiers,

    // Input buffer for characters to send
    input_buffer: Vec<char>,

    // ANSI escape sequence parser state
    esc_state: EscState,
    esc_buf: [u8; 32],
    esc_len: usize,
}

impl Terminal {
    /// Create a new terminal with the given character dimensions.
    pub fn new(id: WidgetId, x: i32, y: i32, cols: usize, rows: usize) -> Self {
        let char_w = char_width();
        let char_h = text_height();
        let width = (cols as u32) * char_w;
        let height = (rows as u32) * char_h;

        // Initialize buffer with empty rows
        let buffer: VecDeque<Vec<Cell>> = (0..rows).map(|_| vec![Cell::default(); cols]).collect();

        Self {
            id,
            x,
            y,
            width,
            height,
            buffer,
            cols,
            rows,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            cursor_blink_counter: 0,
            scroll_offset: 0,
            history: VecDeque::new(),
            max_history: 1000,
            focused: false,
            bg_color: terminal_colors::BACKGROUND,
            fg_color: terminal_colors::FOREGROUND,
            current_fg: terminal_colors::FOREGROUND,
            current_bg: terminal_colors::BACKGROUND,
            bold: false,
            modifiers: edos_lib::keymap::Modifiers::default(),
            input_buffer: Vec::new(),
            esc_state: EscState::Normal,
            esc_buf: [0; 32],
            esc_len: 0,
        }
    }

    /// Create a terminal with explicit pixel dimensions (calculates cols/rows).
    pub fn with_size(id: WidgetId, x: i32, y: i32, width: u32, height: u32) -> Self {
        let char_w = char_width();
        let char_h = text_height();
        let cols = (width / char_w) as usize;
        let rows = (height / char_h) as usize;

        let mut term = Self::new(id, x, y, cols.max(1), rows.max(1));
        term.width = width;
        term.height = height;
        term
    }

    /// Get the number of columns.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Get the number of rows.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Get cursor position as (row, col).
    pub fn cursor_position(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    /// Set cursor position.
    pub fn set_cursor(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.cursor_col = col.min(self.cols.saturating_sub(1));
    }

    /// Write a character at the current cursor position.
    pub fn write_char(&mut self, ch: char) {
        match self.esc_state {
            EscState::Escape => {
                if ch == '[' {
                    self.esc_state = EscState::Csi;
                    self.esc_len = 0;
                } else {
                    // Unknown escape sequence, discard
                    self.esc_state = EscState::Normal;
                }
                return;
            }
            EscState::Csi => {
                let b = ch as u8;
                if b >= 0x30 && b <= 0x3F {
                    // Parameter byte (digits, semicolons)
                    if self.esc_len < self.esc_buf.len() {
                        self.esc_buf[self.esc_len] = b;
                        self.esc_len += 1;
                    }
                } else if b >= 0x40 && b <= 0x7E {
                    // Final byte - execute the command
                    self.execute_csi(ch);
                    self.esc_state = EscState::Normal;
                } else {
                    // Intermediate byte - accumulate
                    if self.esc_len < self.esc_buf.len() {
                        self.esc_buf[self.esc_len] = b;
                        self.esc_len += 1;
                    }
                }
                return;
            }
            EscState::Normal => {} // fall through to normal char handling
        }

        match ch {
            '\n' => {
                self.newline();
            }
            '\r' => {
                self.cursor_col = 0;
            }
            '\x08' => {
                // Backspace: move cursor left (standard terminal behavior)
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                }
            }
            '\x7F' => {
                // DEL: move cursor left and erase
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                    self.buffer[self.cursor_row][self.cursor_col] = Cell::default();
                }
            }
            '\t' => {
                // Tab - move to next 8-character boundary
                let next_tab = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next_tab.min(self.cols - 1);
            }
            '\x1B' => {
                self.esc_state = EscState::Escape;
            }
            _ => {
                if ch.is_control() {
                    return; // Ignore other control characters
                }

                // Write the character with current pen attributes
                if self.cursor_row < self.rows && self.cursor_col < self.cols {
                    self.buffer[self.cursor_row][self.cursor_col] = Cell {
                        ch,
                        fg: self.current_fg,
                        bg: self.current_bg,
                    };
                    self.cursor_col += 1;

                    // Wrap to next line if needed
                    if self.cursor_col >= self.cols {
                        self.cursor_col = 0;
                        self.newline();
                    }
                }
            }
        }

        // New output snaps back to live view
        self.scroll_offset = 0;
    }

    /// Execute a CSI (Control Sequence Introducer) command.
    fn execute_csi(&mut self, final_byte: char) {
        let params = self.parse_csi_params();

        match final_byte {
            'H' | 'f' => {
                // Cursor position: CSI row ; col H
                let row = params.get(0).copied().unwrap_or(1).max(1) - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) - 1;
                self.cursor_row = row.min(self.rows - 1);
                self.cursor_col = col.min(self.cols - 1);
            }
            'A' => {
                // Cursor up
                let n = params.get(0).copied().unwrap_or(1).max(1);
                self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            'B' => {
                // Cursor down
                let n = params.get(0).copied().unwrap_or(1).max(1);
                self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
            }
            'C' => {
                // Cursor forward (right)
                let n = params.get(0).copied().unwrap_or(1).max(1);
                self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
            }
            'D' => {
                // Cursor back (left)
                let n = params.get(0).copied().unwrap_or(1).max(1);
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            'J' => {
                // Erase in display
                let n = params.get(0).copied().unwrap_or(0);
                match n {
                    0 => {
                        // Erase from cursor to end of screen
                        for c in self.cursor_col..self.cols {
                            self.buffer[self.cursor_row][c] = Cell::default();
                        }
                        for row in (self.cursor_row + 1)..self.rows {
                            for c in 0..self.cols {
                                self.buffer[row][c] = Cell::default();
                            }
                        }
                    }
                    1 => {
                        // Erase from start to cursor
                        for row in 0..self.cursor_row {
                            for c in 0..self.cols {
                                self.buffer[row][c] = Cell::default();
                            }
                        }
                        for c in 0..=self.cursor_col.min(self.cols - 1) {
                            self.buffer[self.cursor_row][c] = Cell::default();
                        }
                    }
                    2 => {
                        // Erase entire screen
                        self.clear();
                    }
                    _ => {}
                }
            }
            'K' => {
                // Erase in line
                let n = params.get(0).copied().unwrap_or(0);
                match n {
                    0 => {
                        // Erase from cursor to end of line
                        for c in self.cursor_col..self.cols {
                            self.buffer[self.cursor_row][c] = Cell::default();
                        }
                    }
                    1 => {
                        // Erase from start to cursor
                        for c in 0..=self.cursor_col.min(self.cols - 1) {
                            self.buffer[self.cursor_row][c] = Cell::default();
                        }
                    }
                    2 => {
                        // Erase entire line
                        for c in 0..self.cols {
                            self.buffer[self.cursor_row][c] = Cell::default();
                        }
                    }
                    _ => {}
                }
            }
            'm' => {
                // SGR (Select Graphic Rendition) - color/style attributes
                let params = if params.is_empty() {
                    vec![0]
                } else {
                    params.clone()
                };
                for &p in &params {
                    match p {
                        0 => {
                            self.current_fg = terminal_colors::FOREGROUND;
                            self.current_bg = terminal_colors::BACKGROUND;
                            self.bold = false;
                        }
                        1 => {
                            self.bold = true;
                        }
                        22 => {
                            self.bold = false;
                        }
                        30..=37 => {
                            let idx = (p - 30) + if self.bold { 8 } else { 0 };
                            self.current_fg = ANSI_COLORS[idx];
                        }
                        39 => {
                            self.current_fg = terminal_colors::FOREGROUND;
                        }
                        40..=47 => {
                            self.current_bg = ANSI_COLORS[p - 40];
                        }
                        49 => {
                            self.current_bg = terminal_colors::BACKGROUND;
                        }
                        90..=97 => {
                            self.current_fg = ANSI_COLORS[p - 90 + 8];
                        }
                        100..=107 => {
                            self.current_bg = ANSI_COLORS[p - 100 + 8];
                        }
                        _ => {} // Unknown SGR param, ignore
                    }
                }
            }
            _ => {
                // Unknown CSI command, ignore
            }
        }
    }

    /// Parse the accumulated CSI parameter bytes into a list of numeric values.
    fn parse_csi_params(&self) -> Vec<usize> {
        let param_str = core::str::from_utf8(&self.esc_buf[..self.esc_len]).unwrap_or("");
        if param_str.is_empty() {
            return Vec::new();
        }
        param_str
            .split(';')
            .map(|s| s.parse::<usize>().unwrap_or(0))
            .collect()
    }

    /// Resize the terminal to fit the given pixel dimensions.
    pub fn resize_to_pixels(&mut self, pixel_width: u32, pixel_height: u32) {
        let char_w = char_width();
        let char_h = text_height();
        let new_cols = (pixel_width / char_w) as usize;
        let new_rows = (pixel_height / char_h) as usize;
        if new_cols == 0 || new_rows == 0 || (new_cols == self.cols && new_rows == self.rows) {
            return;
        }

        // Build new buffer, preserving existing content where it fits
        let mut new_buffer = VecDeque::new();
        for r in 0..new_rows {
            let mut row = vec![Cell::default(); new_cols];
            if r < self.buffer.len() {
                let old_row = &self.buffer[r];
                let copy_cols = old_row.len().min(new_cols);
                row[..copy_cols].copy_from_slice(&old_row[..copy_cols]);
            }
            new_buffer.push_back(row);
        }

        self.buffer = new_buffer;
        self.cols = new_cols;
        self.rows = new_rows;
        self.cursor_row = self.cursor_row.min(new_rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(new_cols.saturating_sub(1));
        self.width = pixel_width;
        self.height = pixel_height;
        self.scroll_offset = 0;
    }

    /// Write a string to the terminal.
    pub fn write_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.write_char(ch);
        }
    }

    /// Move to a new line, scrolling if necessary.
    fn newline(&mut self) {
        self.cursor_col = 0;
        self.cursor_row += 1;

        if self.cursor_row >= self.rows {
            self.scroll_up();
            self.cursor_row = self.rows - 1;
        }
    }

    /// Scroll the buffer up by one line.
    fn scroll_up(&mut self) {
        if let Some(line) = self.buffer.pop_front() {
            // Move top line to history
            if self.history.len() >= self.max_history {
                self.history.pop_front();
            }
            self.history.push_back(line);

            // Add new empty line at bottom
            self.buffer.push_back(vec![Cell::default(); self.cols]);
        }
    }

    /// Clear the entire terminal.
    pub fn clear(&mut self) {
        for row in &mut self.buffer {
            for cell in row.iter_mut() {
                *cell = Cell::default();
            }
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Clear from cursor to end of line.
    pub fn clear_to_eol(&mut self) {
        if self.cursor_row < self.rows {
            for col in self.cursor_col..self.cols {
                self.buffer[self.cursor_row][col] = Cell::default();
            }
        }
    }

    /// Clear from cursor to end of screen.
    pub fn clear_to_eos(&mut self) {
        self.clear_to_eol();
        for row in (self.cursor_row + 1)..self.rows {
            for col in 0..self.cols {
                self.buffer[row][col] = Cell::default();
            }
        }
    }

    /// Get any pending input characters and clear the buffer.
    pub fn take_input(&mut self) -> Vec<char> {
        std::mem::take(&mut self.input_buffer)
    }

    /// Check if there is pending input.
    pub fn has_input(&self) -> bool {
        !self.input_buffer.is_empty()
    }

    /// Get the text content of a specific row.
    #[allow(private_interfaces, dead_code)]
    pub(crate) fn get_row(&self, row: usize) -> Option<&[Cell]> {
        self.buffer.get(row).map(|v| v.as_slice())
    }

    /// Set background color.
    pub fn set_background(&mut self, color: u32) {
        self.bg_color = color;
    }

    /// Set foreground color.
    pub fn set_foreground(&mut self, color: u32) {
        self.fg_color = color;
    }

    /// Scroll the terminal view by the given delta (positive = up into history).
    pub fn scroll(&mut self, delta: i32) {
        if delta > 0 {
            self.scroll_offset = (self.scroll_offset + delta as usize).min(self.history.len());
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub((-delta) as usize);
        }
    }

    /// Update cursor blink state (call periodically).
    pub fn tick(&mut self) {
        self.cursor_blink_counter += 1;
        if self.cursor_blink_counter >= 30 {
            // Toggle every ~500ms at 60fps
            self.cursor_visible = !self.cursor_visible;
            self.cursor_blink_counter = 0;
        }
    }
}

impl Widget for Terminal {
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
        // Draw background
        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            self.y,
            self.width,
            self.height,
            self.bg_color,
        );

        let char_w = char_width() as i32;
        let char_h = text_height() as i32;

        // Draw each row of cells, accounting for scroll offset into history
        let history_len = self.history.len();
        let total_lines = history_len + self.buffer.len();
        let viewport_bottom = total_lines.saturating_sub(self.scroll_offset);
        let viewport_top = viewport_bottom.saturating_sub(self.rows);

        for display_row in 0..self.rows {
            let line_idx = viewport_top + display_row;
            let row_y = self.y + (display_row as i32) * char_h;

            let row_data: Option<&Vec<Cell>> = if line_idx < history_len {
                self.history.get(line_idx)
            } else {
                self.buffer.get(line_idx - history_len)
            };

            if let Some(row) = row_data {
                for (col_idx, cell) in row.iter().enumerate() {
                    let cell_x = self.x + (col_idx as i32) * char_w;

                    // Draw cell background if it differs from terminal background
                    if cell.bg != self.bg_color {
                        draw_rect(
                            buffer,
                            buffer_width,
                            buffer_height,
                            cell_x,
                            row_y,
                            char_w as u32,
                            char_h as u32,
                            cell.bg,
                        );
                    }

                    // Draw character if not a space
                    if cell.ch != ' ' {
                        let ch_str = cell.ch.to_string();
                        draw_text(
                            buffer,
                            buffer_width,
                            buffer_height,
                            cell_x,
                            row_y,
                            &ch_str,
                            cell.fg,
                        );
                    }
                }
            }
        }

        // Draw cursor if focused, visible, and not scrolled back
        if self.scroll_offset == 0 && self.focused && self.cursor_visible {
            let cursor_x = self.x + (self.cursor_col as i32) * char_w;
            let cursor_y = self.y + (self.cursor_row as i32) * char_h;

            draw_rect(
                buffer,
                buffer_width,
                buffer_height,
                cursor_x,
                cursor_y,
                char_w as u32,
                char_h as u32,
                terminal_colors::CURSOR,
            );

            // Draw the character under cursor in inverted color if there is one
            if self.cursor_row < self.rows && self.cursor_col < self.cols {
                let ch = self.buffer[self.cursor_row][self.cursor_col].ch;
                if ch != ' ' {
                    let ch_str = ch.to_string();
                    draw_text(
                        buffer,
                        buffer_width,
                        buffer_height,
                        cursor_x,
                        cursor_y,
                        &ch_str,
                        self.bg_color,
                    );
                }
            }
        }
    }

    fn on_mouse_move(&mut self, _x: i32, _y: i32) {
        // Could implement text selection here
    }

    fn on_mouse_button(&mut self, _x: i32, _y: i32, _pressed: bool) -> Option<WidgetEvent> {
        None
    }

    fn on_key(&mut self, scancode: u32, pressed: bool) -> Option<WidgetEvent> {
        use edos_lib::keymap::{keycode, map_keycode, update_modifiers};

        if !self.focused {
            return None;
        }

        // Update modifier state (shift, altgr, ctrl, caps lock)
        if update_modifiers(&mut self.modifiers, scancode, pressed) {
            return None;
        }

        if !pressed {
            return None;
        }

        // Reset cursor blink
        self.cursor_visible = true;
        self.cursor_blink_counter = 0;

        // Handle special keys (escape sequences, scrollback)
        match scancode {
            keycode::ARROW_UP => self.input_buffer.extend("\x1B[A".chars()),
            keycode::ARROW_DOWN => self.input_buffer.extend("\x1B[B".chars()),
            keycode::ARROW_RIGHT => self.input_buffer.extend("\x1B[C".chars()),
            keycode::ARROW_LEFT => self.input_buffer.extend("\x1B[D".chars()),
            keycode::HOME => self.input_buffer.extend("\x1B[H".chars()),
            keycode::END => self.input_buffer.extend("\x1B[F".chars()),
            keycode::PAGE_UP => {
                if self.modifiers.shift {
                    self.scroll_offset =
                        (self.scroll_offset + self.rows / 2).min(self.history.len());
                } else {
                    self.input_buffer.extend("\x1B[5~".chars());
                }
            }
            keycode::PAGE_DOWN => {
                if self.modifiers.shift {
                    self.scroll_offset = self.scroll_offset.saturating_sub(self.rows / 2);
                } else {
                    self.input_buffer.extend("\x1B[6~".chars());
                }
            }
            keycode::DELETE => self.input_buffer.extend("\x1B[3~".chars()),
            _ => {
                // Try to decode keycode to a character via the layout
                if let Some(ch) = map_keycode(scancode, &self.modifiers) {
                    self.input_buffer.push(ch);
                }
            }
        }

        None
    }

    fn on_char(&mut self, _ch: char) -> Option<WidgetEvent> {
        // Character decoding is handled in on_key via the keymap.
        None
    }

    fn focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.cursor_visible = true;
            self.cursor_blink_counter = 0;
        }
    }
}
