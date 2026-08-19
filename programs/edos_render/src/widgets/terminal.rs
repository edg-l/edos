//! Terminal widget for displaying text with cursor and scrolling support.

use std::collections::VecDeque;

use edos_lib::clipboard::{self, Buffer};

use super::{
    Rect, Widget, WidgetEvent, WidgetId, char_width, draw_rect, draw_text_styled, text_height,
};
use crate::text::Style;

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

/// Smallest gap kept between the widget edge and the character grid. The grid
/// is centred inside the widget, so whatever the pixel-to-cell division leaves
/// over is split around it and the real gap is never smaller than this.
pub const MIN_PADDING_X: u32 = 6;
pub const MIN_PADDING_Y: u32 = 4;

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

/// A cell as it will actually be rasterised, with selection folded into the
/// background. Comparing these is what decides whether a row is drawn again.
///
/// Selection lives outside the cell grid, so a comparison against [`Cell`]
/// would call a row unchanged while its highlight came and went.
#[derive(Clone, Copy, PartialEq)]
struct RenderCell {
    ch: char,
    fg: u32,
    bg: u32,
}

/// Where the cursor block is and whether it is being drawn at all.
#[derive(Clone, Copy, PartialEq, Default)]
struct CursorMark {
    row: usize,
    col: usize,
    drawn: bool,
}

/// What one of the window's shm buffers already shows.
///
/// The window alternates two buffers, so the one about to be drawn holds the
/// frame from *two* frames ago rather than the one on screen. Each therefore
/// needs its own record: comparing against the buffer's own last contents is
/// what makes a partial repaint correct whichever buffer is current, instead of
/// correct on alternate frames.
#[derive(Default)]
struct Painted {
    rows: Vec<Vec<RenderCell>>,
    cursor: CursorMark,
    /// Widget size when this record was made. A resize hands the window two
    /// fresh buffers and moves the grid inside them, so the padding around it
    /// has to be laid down again rather than compared row by row.
    size: (u32, u32),
    /// Where the viewport sat when this record was made, so the distance the
    /// picture has travelled since is a subtraction.
    top_line: i64,
}

impl Painted {
    /// Account for the buffer's pixels having been moved `shift` rows up
    /// (positive) or down (negative).
    ///
    /// The rows that scrolled in from outside hold whatever the move dragged
    /// along, so they are emptied: an empty row matches no real row, which is
    /// what makes the comparison that follows rasterise exactly them. That is
    /// also the safety net -- if `shift` were ever wrong, every row it does not
    /// account for simply compares unequal and is drawn again.
    fn shift_rows(&mut self, shift: i32) {
        let rows = self.rows.len();
        if rows == 0 {
            return;
        }
        let exposed: &mut [Vec<RenderCell>] = if shift > 0 {
            let n = (shift as usize).min(rows);
            self.rows.rotate_left(n);
            &mut self.rows[rows - n..]
        } else {
            let n = ((-shift) as usize).min(rows);
            self.rows.rotate_right(n);
            &mut self.rows[..n]
        };
        for row in exposed {
            row.clear();
        }
        // The cursor block moved with the pixels under it.
        self.cursor.row =
            (self.cursor.row as i64 - shift as i64).clamp(0, rows as i64 - 1) as usize;
    }
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
    /// DECTCEM (CSI ? 25 h / l): the program can hide the cursor entirely,
    /// independently of the blink phase in `cursor_visible`.
    cursor_enabled: bool,

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
    /// SGR 7: pen colours are swapped as each cell is written, so a cell keeps
    /// the colours it was drawn with and nothing has to be re-inverted later.
    reverse: bool,

    // Modifier key state
    modifiers: edos_lib::keymap::Modifiers,

    // Input buffer for characters to send
    input_buffer: Vec<char>,

    // Text selection state
    selection_start: Option<(usize, usize)>, // (absolute_line_idx, col)
    selection_end: Option<(usize, usize)>,   // (absolute_line_idx, col)
    selecting: bool,                         // true while mouse button held during drag
    last_click_cell: Option<(usize, usize)>, // for double-click detection
    last_click_tick: u32,                    // frame counter at last click
    tick_counter: u32,                       // monotonic frame counter

    // ANSI escape sequence parser state
    esc_state: EscState,
    esc_buf: [u8; 32],
    esc_len: usize,

    /// Lines that have left the top of the buffer into history. Counted rather
    /// than derived from `history.len()`, which stops growing once the history
    /// is full and would then report a scroll as standing still.
    scrolled: u64,

    /// What each of the window's two shm buffers already shows.
    painted: [Painted; 2],
    /// The viewport as it would be rasterised now, reused across frames so a
    /// repaint allocates nothing.
    current: Vec<Vec<RenderCell>>,
}

impl Terminal {
    /// Create a new terminal with the given character dimensions.
    pub fn new(id: WidgetId, x: i32, y: i32, cols: usize, rows: usize) -> Self {
        let char_w = char_width();
        let char_h = text_height();
        let width = (cols as u32) * char_w + 2 * MIN_PADDING_X;
        let height = (rows as u32) * char_h + 2 * MIN_PADDING_Y;

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
            cursor_enabled: true,
            scroll_offset: 0,
            history: VecDeque::new(),
            max_history: 1000,
            focused: false,
            bg_color: terminal_colors::BACKGROUND,
            fg_color: terminal_colors::FOREGROUND,
            current_fg: terminal_colors::FOREGROUND,
            current_bg: terminal_colors::BACKGROUND,
            bold: false,
            reverse: false,
            modifiers: edos_lib::keymap::Modifiers::default(),
            input_buffer: Vec::new(),
            selection_start: None,
            selection_end: None,
            selecting: false,
            last_click_cell: None,
            last_click_tick: 0,
            tick_counter: 0,
            esc_state: EscState::Normal,
            esc_buf: [0; 32],
            esc_len: 0,
            scrolled: 0,
            painted: [Painted::default(), Painted::default()],
            current: Vec::new(),
        }
    }

    /// Create a terminal with explicit pixel dimensions (calculates cols/rows).
    pub fn with_size(id: WidgetId, x: i32, y: i32, width: u32, height: u32) -> Self {
        let (cols, rows) = Self::grid_for_pixels(width, height);

        let mut term = Self::new(id, x, y, cols.max(1), rows.max(1));
        term.width = width;
        term.height = height;
        term
    }

    /// Character grid that fits inside `width` x `height` once the padding is
    /// taken out.
    fn grid_for_pixels(width: u32, height: u32) -> (usize, usize) {
        let cols = width.saturating_sub(2 * MIN_PADDING_X) / char_width();
        let rows = height.saturating_sub(2 * MIN_PADDING_Y) / text_height();
        (cols as usize, rows as usize)
    }

    /// Top-left pixel of the character grid, centred inside the widget.
    fn content_origin(&self) -> (i32, i32) {
        let grid_w = (self.cols as u32) * char_width();
        let grid_h = (self.rows as u32) * text_height();
        (
            self.x + (self.width.saturating_sub(grid_w) / 2) as i32,
            self.y + (self.height.saturating_sub(grid_h) / 2) as i32,
        )
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
                if (0x30..=0x3F).contains(&b) {
                    // Parameter byte (digits, semicolons)
                    if self.esc_len < self.esc_buf.len() {
                        self.esc_buf[self.esc_len] = b;
                        self.esc_len += 1;
                    }
                } else if (0x40..=0x7E).contains(&b) {
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
                    let (fg, bg) = if self.reverse {
                        (self.current_bg, self.current_fg)
                    } else {
                        (self.current_fg, self.current_bg)
                    };
                    self.buffer[self.cursor_row][self.cursor_col] = Cell { ch, fg, bg };
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
                let row = params.first().copied().unwrap_or(1).max(1) - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) - 1;
                self.cursor_row = row.min(self.rows - 1);
                self.cursor_col = col.min(self.cols - 1);
            }
            'A' => {
                // Cursor up
                let n = params.first().copied().unwrap_or(1).max(1);
                self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            'B' => {
                // Cursor down
                let n = params.first().copied().unwrap_or(1).max(1);
                self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
            }
            'C' => {
                // Cursor forward (right)
                let n = params.first().copied().unwrap_or(1).max(1);
                self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
            }
            'D' => {
                // Cursor back (left)
                let n = params.first().copied().unwrap_or(1).max(1);
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            'J' => {
                // Erase in display
                let n = params.first().copied().unwrap_or(0);
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
                let n = params.first().copied().unwrap_or(0);
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
                            self.reverse = false;
                        }
                        1 => {
                            self.bold = true;
                        }
                        7 => {
                            self.reverse = true;
                        }
                        22 => {
                            self.bold = false;
                        }
                        27 => {
                            self.reverse = false;
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
            'h' | 'l'
                // DEC private mode set/reset. DECTCEM (?25) is the only mode
                // the widget implements; anything else is ignored.
                if self.csi_is_private() && params.first() == Some(&25) => {
                    self.cursor_enabled = final_byte == 'h';
                    if self.cursor_enabled {
                        self.cursor_visible = true;
                        self.cursor_blink_counter = 0;
                    }
                }
            _ => {
                // Unknown CSI command, ignore
            }
        }
    }

    /// Whether the accumulated CSI carries the DEC private-parameter prefix `?`.
    fn csi_is_private(&self) -> bool {
        self.esc_len > 0 && self.esc_buf[0] == b'?'
    }

    /// Parse the accumulated CSI parameter bytes into a list of numeric values.
    /// A leading private-parameter prefix is not a parameter and is skipped.
    fn parse_csi_params(&self) -> Vec<usize> {
        let start = usize::from(self.csi_is_private());
        let param_str = core::str::from_utf8(&self.esc_buf[start..self.esc_len]).unwrap_or("");
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
        let (new_cols, new_rows) = Self::grid_for_pixels(pixel_width, pixel_height);
        if new_cols == 0 || new_rows == 0 {
            return;
        }
        if new_cols == self.cols && new_rows == self.rows {
            self.width = pixel_width;
            self.height = pixel_height;
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
        self.clear_selection();
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
            self.scrolled += 1;
            // Move top line to history
            if self.history.len() >= self.max_history {
                self.history.pop_front();
                // A line was evicted from the front of history; adjust selection indices.
                // Only history-resident endpoints shift; buffer-resident ones stay put
                // because history.len() decreased by 1, keeping their absolute index correct.
                let history_len = self.history.len(); // post-pop length
                let start_gone = self.selection_start.map(|(l, _)| l == 0).unwrap_or(false);
                let end_gone = self.selection_end.map(|(l, _)| l == 0).unwrap_or(false);
                if start_gone || end_gone {
                    self.clear_selection();
                } else {
                    for opt in [&mut self.selection_start, &mut self.selection_end] {
                        if let Some((line_idx, _)) = opt
                            && *line_idx < history_len {
                                *line_idx -= 1;
                            }
                    }
                }
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

    /// Convert pixel coordinates to (absolute_line_index, col).
    fn pixel_to_cell(&self, px: i32, py: i32) -> (usize, usize) {
        let char_w = char_width() as i32;
        let char_h = text_height() as i32;

        let (origin_x, origin_y) = self.content_origin();
        let col = ((px - origin_x) / char_w).clamp(0, (self.cols as i32) - 1) as usize;
        let display_row = ((py - origin_y) / char_h).clamp(0, (self.rows as i32) - 1) as usize;

        let total_lines = self.history.len() + self.buffer.len();
        let viewport_bottom = total_lines.saturating_sub(self.scroll_offset);
        let viewport_top = viewport_bottom.saturating_sub(self.rows);
        let abs_line = viewport_top + display_row;

        (abs_line, col)
    }

    /// Returns selection (start, end) sorted so start <= end, or None if no selection.
    fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let start = self.selection_start?;
        let end = self.selection_end?;

        if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
            Some((start, end))
        } else {
            Some((end, start))
        }
    }

    /// The inclusive column span selected on `abs_line`, or None.
    ///
    /// Per line rather than per cell: the range is the same for every cell in
    /// a row, and recomputing it a thousand times a frame cost more than
    /// rasterising the glyphs did.
    fn selected_cols(
        &self,
        selection: Option<((usize, usize), (usize, usize))>,
        abs_line: usize,
    ) -> Option<(usize, usize)> {
        let ((start_line, start_col), (end_line, end_col)) = selection?;

        // Empty selection (click without drag)
        if start_line == end_line && start_col == end_col {
            return None;
        }

        if abs_line < start_line || abs_line > end_line {
            return None;
        }

        Some(if start_line == end_line {
            (start_col, end_col)
        } else if abs_line == start_line {
            (start_col, usize::MAX)
        } else if abs_line == end_line {
            (0, end_col)
        } else {
            (0, usize::MAX)
        })
    }

    /// Find the word boundaries around the given cell. Returns (start_col, end_col) inclusive.
    fn word_bounds_at(&self, abs_line: usize, col: usize) -> (usize, usize) {
        let row = if abs_line < self.history.len() {
            self.history.get(abs_line)
        } else {
            self.buffer.get(abs_line - self.history.len())
        };
        let row = match row {
            Some(r) if !r.is_empty() => r,
            _ => return (col, col),
        };
        let col = col.min(row.len() - 1);
        let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '-' || c == '.';
        let ch = row[col].ch;
        if !is_word(ch) {
            return (col, col);
        }
        let mut start = col;
        while start > 0 && is_word(row[start - 1].ch) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < row.len() && is_word(row[end + 1].ch) {
            end += 1;
        }
        (start, end)
    }

    /// Clear the current selection.
    pub fn clear_selection(&mut self) {
        self.selection_start = None;
        self.selection_end = None;
        self.selecting = false;
    }

    /// Get the text covered by the current selection, or None if no selection.
    pub fn get_selected_text(&self) -> Option<String> {
        let ((start_line, start_col), (end_line, end_col)) = self.selection_range()?;

        // Empty selection (click without drag)
        if start_line == end_line && start_col == end_col {
            return None;
        }

        let get_row = |abs_line: usize| -> Option<&Vec<Cell>> {
            if abs_line < self.history.len() {
                self.history.get(abs_line)
            } else {
                self.buffer.get(abs_line - self.history.len())
            }
        };

        let mut lines: Vec<String> = Vec::new();

        for abs_line in start_line..=end_line {
            let Some(row) = get_row(abs_line) else {
                continue;
            };

            if row.is_empty() {
                lines.push(String::new());
                continue;
            }

            let (col_start, col_end) = if start_line == end_line {
                (start_col, end_col)
            } else if abs_line == start_line {
                (start_col, row.len().saturating_sub(1))
            } else if abs_line == end_line {
                (0, end_col)
            } else {
                (0, row.len().saturating_sub(1))
            };

            let col_end_clamped = col_end.min(row.len().saturating_sub(1));
            let text: String = if col_start <= col_end_clamped {
                row[col_start..=col_end_clamped]
                    .iter()
                    .map(|c| c.ch)
                    .collect()
            } else {
                String::new()
            };

            lines.push(text.trim_end().to_string());
        }

        let result = lines.join("\n");
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    /// Copy the current selection to the clipboard.
    pub fn copy_selection(&self) {
        if let Some(text) = self.get_selected_text() {
            let _ = clipboard::set(Buffer::Clipboard, &text);
        }
    }

    /// Publish the current selection as the primary selection, which is what a
    /// middle click pastes. Called when a selection is finished rather than
    /// when one is asked for, since selecting *is* the act that fills it.
    fn publish_selection(&self) {
        if let Some(text) = self.get_selected_text() {
            let _ = clipboard::set(Buffer::Primary, &text);
        }
    }

    /// Paste a clipboard buffer into the input buffer, as though it had been
    /// typed: a newline in the text submits the line, because that is what the
    /// Return key puts in this same buffer. A carriage return is folded into
    /// one, so text copied from a CRLF source does not submit twice.
    pub fn paste(&mut self, buffer: Buffer) {
        let text = clipboard::get(buffer);
        let mut prev_cr = false;
        for ch in text.chars() {
            match ch {
                '\r' => {
                    self.input_buffer.push('\n');
                    prev_cr = true;
                }
                '\n' => {
                    if !prev_cr {
                        self.input_buffer.push('\n');
                    }
                    prev_cr = false;
                }
                _ => {
                    self.input_buffer.push(ch);
                    prev_cr = false;
                }
            }
        }
    }

    /// Paste the clipboard into the input buffer.
    pub fn paste_clipboard(&mut self) {
        self.paste(Buffer::Clipboard);
    }

    /// Paste the primary selection, which is what the middle button does
    /// everywhere else. Not part of [`Widget`], because the trait's mouse
    /// handler carries no button and the owner is what knows which one it saw.
    pub fn paste_primary(&mut self) {
        self.paste(Buffer::Primary);
    }

    /// Whether a selection drag is in progress.
    ///
    /// Pointer motion only changes the picture while this is true. A window
    /// being dragged by its title bar delivers motion to the terminal for the
    /// whole drag, and repainting on that made the cost of moving a window
    /// scale with how much text it held.
    pub fn is_selecting(&self) -> bool {
        self.selecting
    }

    /// Update cursor blink state (call periodically).
    ///
    /// Returns whether anything on screen changed, so an owner can skip a
    /// repaint it does not need. A terminal that redraws unconditionally
    /// reports damage every frame and makes the compositor retransmit an
    /// identical window; over a remote display that is the whole cost.
    pub fn tick(&mut self) -> bool {
        self.tick_counter = self.tick_counter.wrapping_add(1);
        self.cursor_blink_counter += 1;
        if self.cursor_blink_counter >= 30 {
            // Toggle every ~500ms at 60fps
            self.cursor_visible = !self.cursor_visible;
            self.cursor_blink_counter = 0;
            return true;
        }
        false
    }

    /// The absolute line index shown at the top of the viewport.
    fn viewport_top(&self) -> usize {
        let total_lines = self.history.len() + self.buffer.len();
        total_lines
            .saturating_sub(self.scroll_offset)
            .saturating_sub(self.rows)
    }

    /// Fill `out` with one display row as it will be rasterised.
    fn render_row_into(&self, display_row: usize, out: &mut Vec<RenderCell>) {
        out.clear();
        let line_idx = self.viewport_top() + display_row;
        let history_len = self.history.len();
        let row = if line_idx < history_len {
            self.history.get(line_idx)
        } else {
            self.buffer.get(line_idx - history_len)
        };
        let Some(row) = row else {
            return;
        };
        out.reserve(row.len());
        let selected = self.selected_cols(self.selection_range(), line_idx);
        for (col, cell) in row.iter().enumerate() {
            let bg = match selected {
                Some((first, last)) if col >= first && col <= last => terminal_colors::SELECTION,
                _ => cell.bg,
            };
            out.push(RenderCell {
                ch: cell.ch,
                fg: cell.fg,
                bg,
            });
        }
    }

    /// Rasterise one display row, over its own ground so a partial repaint
    /// erases whatever the row held before.
    fn draw_row(
        &self,
        buffer: &mut [u32],
        buffer_width: u32,
        buffer_height: u32,
        display_row: usize,
        cells: &[RenderCell],
    ) {
        let char_w = char_width() as i32;
        let char_h = text_height() as i32;
        let (origin_x, origin_y) = self.content_origin();
        let row_y = origin_y + (display_row as i32) * char_h;

        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            row_y,
            self.width,
            char_h as u32,
            self.bg_color,
        );

        for (col, cell) in cells.iter().enumerate() {
            let cell_x = origin_x + (col as i32) * char_w;
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
            if cell.ch != ' ' {
                // Mono, always: this is a character grid, and a proportional
                // face would put every cell's glyph at a different offset
                // inside its cell.
                draw_text_styled(
                    buffer,
                    buffer_width,
                    buffer_height,
                    cell_x,
                    row_y,
                    &cell.ch.to_string(),
                    Style::mono(cell.fg),
                );
            }
        }
    }

    /// Fill the bands above and below the character grid.
    ///
    /// Every row lays down its own ground, so a full repaint covers the grid
    /// between them; filling the whole widget as well would write those pixels
    /// twice, and at a screen a frame that is the largest single cost here.
    fn fill_padding(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32) {
        let (_, origin_y) = self.content_origin();
        let grid_bottom = origin_y + (self.rows as i32) * (text_height() as i32);
        let widget_bottom = self.y + self.height as i32;

        for (y, height) in [
            (self.y, origin_y - self.y),
            (grid_bottom, widget_bottom - grid_bottom),
        ] {
            if height > 0 {
                draw_rect(
                    buffer,
                    buffer_width,
                    buffer_height,
                    self.x,
                    y,
                    self.width,
                    height as u32,
                    self.bg_color,
                );
            }
        }
    }

    /// How far the viewport has travelled down the output stream, in lines.
    ///
    /// Output moves it forward and scrollback moves it back, so the difference
    /// between two of these is exactly how many rows the picture shifted.
    fn top_line(&self) -> i64 {
        self.scrolled as i64 - self.scroll_offset as i64
    }

    /// Move the grid's pixels `shift` rows up (positive) or down (negative).
    ///
    /// Scrolling a terminal changes every row, and rasterising them all is the
    /// expensive way to say that row `r` now shows what row `r + shift` already
    /// showed. Those pixels are in the buffer; moving them costs one memmove
    /// and leaves only the newly exposed rows to draw.
    fn scroll_pixels(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32, shift: i32) {
        let char_h = text_height() as i32;
        let (_, origin_y) = self.content_origin();
        let stride = buffer_width as usize;

        // The band a row's ground covers, so what moves is exactly what
        // `draw_row` would have painted.
        let left = self.x.max(0) as usize;
        let right = ((self.x + self.width as i32).max(0) as usize).min(stride);
        if right <= left {
            return;
        }
        let span = right - left;

        let distance = shift.unsigned_abs() as i32 * char_h;
        let top = origin_y.max(0);
        let bottom = (origin_y + self.rows as i32 * char_h).min(buffer_height as i32);
        if bottom - top <= distance {
            return;
        }

        let mut move_row = |y: i32| {
            let src_y = if shift > 0 {
                y + distance
            } else {
                y - distance
            };
            if src_y < 0 || src_y >= buffer_height as i32 {
                return;
            }
            let src = src_y as usize * stride + left;
            let dst = y as usize * stride + left;
            if src + span <= buffer.len() && dst + span <= buffer.len() {
                buffer.copy_within(src..src + span, dst);
            }
        };

        // Up means each destination reads from below it, so writing top-down
        // never overwrites a source still to be read; down is the mirror.
        if shift > 0 {
            for y in top..bottom - distance {
                move_row(y);
            }
        } else {
            for y in (top + distance..bottom).rev() {
                move_row(y);
            }
        }
    }

    /// Note what a buffer now holds. The rows it held become next frame's
    /// scratch, so neither the record nor the comparison allocates again.
    fn record(&mut self, slot: usize, rows: &mut Vec<Vec<RenderCell>>, cursor: CursorMark) {
        std::mem::swap(&mut self.painted[slot].rows, rows);
        self.painted[slot].cursor = cursor;
        self.painted[slot].size = (self.width, self.height);
        self.painted[slot].top_line = self.top_line();
    }

    /// Where the cursor block is, and whether this frame draws it.
    fn cursor_mark(&self) -> CursorMark {
        CursorMark {
            row: self.cursor_row,
            col: self.cursor_col,
            drawn: self.scroll_offset == 0
                && self.focused
                && self.cursor_enabled
                && self.cursor_visible,
        }
    }

    /// Draw the cursor block and the character under it.
    fn draw_cursor_block(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32) {
        if !self.cursor_mark().drawn {
            return;
        }
        let char_w = char_width() as i32;
        let char_h = text_height() as i32;
        let (origin_x, origin_y) = self.content_origin();
        let cursor_x = origin_x + (self.cursor_col as i32) * char_w;
        let cursor_y = origin_y + (self.cursor_row as i32) * char_h;

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

        if self.cursor_row < self.rows && self.cursor_col < self.cols {
            let ch = self.buffer[self.cursor_row][self.cursor_col].ch;
            if ch != ' ' {
                draw_text_styled(
                    buffer,
                    buffer_width,
                    buffer_height,
                    cursor_x,
                    cursor_y,
                    &ch.to_string(),
                    Style::mono(self.bg_color),
                );
            }
        }
    }

    /// Draw only the rows that differ from what this buffer already shows.
    ///
    /// `slot` names which of the window's two shm buffers `buffer` is; see
    /// [`Painted`] for why each needs its own record.
    ///
    /// Returns the rectangle that changed, in the same coordinates as
    /// [`Widget::bounds`], or `None` when the buffer already shows the current
    /// picture. That rectangle is what the owner reports as damage, and it is
    /// what stops a one-character change costing a whole-window transfer.
    pub fn draw_changed(
        &mut self,
        slot: usize,
        buffer: &mut [u32],
        buffer_width: u32,
        buffer_height: u32,
    ) -> Option<Rect> {
        let slot = slot & 1;
        let char_h = text_height() as i32;
        let (_, origin_y) = self.content_origin();

        let mut current = std::mem::take(&mut self.current);
        current.resize(self.rows, Vec::new());
        for display_row in 0..self.rows {
            let mut row = std::mem::take(&mut current[display_row]);
            self.render_row_into(display_row, &mut row);
            current[display_row] = row;
        }
        let cursor = self.cursor_mark();

        // Two different questions, and answering only the first is what makes a
        // partial repaint wrong on alternate frames.
        //
        // What has to be *drawn* is what this buffer does not already hold, and
        // it holds the frame from two frames ago. What has to be *reported* is
        // what the buffer currently on screen does not hold, since that is the
        // picture a viewer is looking at. A cursor blink makes the difference
        // plain: two toggles later this buffer is already correct and needs no
        // drawing, while the screen still shows the opposite phase and needs
        // the swap.
        let stale = |record: &Painted, size: (u32, u32), rows: usize| {
            record.rows.len() != rows || record.size != size
        };
        let size = (self.width, self.height);
        let redraw_all = stale(&self.painted[slot], size, self.rows);
        let report_all = stale(&self.painted[slot ^ 1], size, self.rows);

        // A scroll moves rows the buffer already holds. Move the pixels and
        // shift the record with them, and the comparison below is left with
        // only the rows that scrolled in.
        if !redraw_all {
            let shift = self.top_line() - self.painted[slot].top_line;
            if shift != 0 && shift.unsigned_abs() < self.rows as u64 {
                self.scroll_pixels(buffer, buffer_width, buffer_height, shift as i32);
                self.painted[slot].shift_rows(shift as i32);
            }
        }

        let mut first = usize::MAX;
        let mut last = 0usize;

        if redraw_all {
            self.fill_padding(buffer, buffer_width, buffer_height);
            for display_row in 0..self.rows {
                self.draw_row(
                    buffer,
                    buffer_width,
                    buffer_height,
                    display_row,
                    &current[display_row],
                );
            }
        }

        let differs = |record: &Painted, display_row: usize| {
            let moved_cursor = cursor != record.cursor
                && (cursor.row == display_row || record.cursor.row == display_row);
            moved_cursor || current[display_row] != record.rows[display_row]
        };

        for display_row in 0..self.rows {
            if !redraw_all && differs(&self.painted[slot], display_row) {
                self.draw_row(
                    buffer,
                    buffer_width,
                    buffer_height,
                    display_row,
                    &current[display_row],
                );
            }
            if report_all || differs(&self.painted[slot ^ 1], display_row) {
                first = first.min(display_row);
                last = last.max(display_row);
            }
        }

        if redraw_all {
            self.draw_cursor_block(buffer, buffer_width, buffer_height);
        }

        if first == usize::MAX {
            // This buffer may have been brought up to date even though the
            // screen already shows the right picture; record that and present
            // nothing.
            self.record(slot, &mut current, cursor);
            self.current = current;
            return None;
        }

        if !redraw_all {
            self.draw_cursor_block(buffer, buffer_width, buffer_height);
        }

        self.record(slot, &mut current, cursor);
        self.current = current;

        if report_all {
            return Some(Rect::new(self.x, self.y, self.width, self.height));
        }
        let top = origin_y + (first as i32) * char_h;
        let bottom = origin_y + ((last + 1) as i32) * char_h;
        Some(Rect::new(
            self.x,
            top,
            self.width,
            (bottom - top).max(0) as u32,
        ))
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
        self.fill_padding(buffer, buffer_width, buffer_height);

        let mut row = Vec::new();
        for display_row in 0..self.rows {
            self.render_row_into(display_row, &mut row);
            self.draw_row(buffer, buffer_width, buffer_height, display_row, &row);
        }

        self.draw_cursor_block(buffer, buffer_width, buffer_height);
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        if self.selecting {
            self.selection_end = Some(self.pixel_to_cell(x, y));
        }
    }

    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent> {
        if pressed {
            let cell = self.pixel_to_cell(x, y);
            let is_double = self.last_click_cell == Some(cell)
                && self.tick_counter.wrapping_sub(self.last_click_tick) < 20;
            self.last_click_cell = Some(cell);
            self.last_click_tick = self.tick_counter;

            if is_double {
                // Double-click: select word
                let (start_col, end_col) = self.word_bounds_at(cell.0, cell.1);
                self.selection_start = Some((cell.0, start_col));
                self.selection_end = Some((cell.0, end_col));
                self.selecting = false;
                self.publish_selection();
            } else {
                self.selection_start = Some(cell);
                self.selection_end = Some(cell);
                self.selecting = true;
            }
        } else {
            // A drag that has just ended is a finished selection, so this is
            // where the primary buffer is filled.
            if self.selecting {
                self.selecting = false;
                self.publish_selection();
            }
        }
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

        // Alt marks a chord as the window manager's. The manager claims the
        // ones it acts on and those never arrive, but an unclaimed Alt chord
        // still does, and neither this widget nor the program running in it
        // bound one: sending its bare character would type `f` into the shell
        // for Alt+F. `altgr` is a separate flag and still selects the third
        // character on a layout.
        if self.modifiers.alt {
            return None;
        }

        // Ctrl+Shift+C/V: copy/paste -- intercept before map_keycode turns them into control chars.
        if self.modifiers.ctrl && self.modifiers.shift {
            match scancode {
                keycode::C => {
                    self.copy_selection();
                    return None;
                }
                keycode::V => {
                    self.paste_clipboard();
                    return None;
                }
                _ => {}
            }
        }

        // Reset cursor blink
        self.cursor_visible = true;
        self.cursor_blink_counter = 0;

        // Handle special keys (escape sequences, scrollback)
        match scancode {
            keycode::ARROW_UP => {
                self.clear_selection();
                self.input_buffer.extend("\x1B[A".chars());
            }
            keycode::ARROW_DOWN => {
                self.clear_selection();
                self.input_buffer.extend("\x1B[B".chars());
            }
            keycode::ARROW_RIGHT => {
                self.clear_selection();
                self.input_buffer.extend("\x1B[C".chars());
            }
            keycode::ARROW_LEFT => {
                self.clear_selection();
                self.input_buffer.extend("\x1B[D".chars());
            }
            keycode::HOME => {
                self.clear_selection();
                self.input_buffer.extend("\x1B[H".chars());
            }
            keycode::END => {
                self.clear_selection();
                self.input_buffer.extend("\x1B[F".chars());
            }
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
            keycode::DELETE => {
                self.clear_selection();
                self.input_buffer.extend("\x1B[3~".chars());
            }
            _ => {
                // Try to decode keycode to a character via the layout
                if let Some(ch) = map_keycode(scancode, &self.modifiers) {
                    self.clear_selection();
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
