//! edos-vi: minimal vi-like text editor for EDOS.

use std::env;
use std::io::{BufWriter, Write, stdout};

use edos_lib::io::{poll_stdin, pty_set_canonical, pty_set_raw, sys_read};

// --- Constants and types ---

const COLS: usize = 80;
const ROWS: usize = 30;
const TEXT_ROWS: usize = ROWS - 1; // bottom row is status line

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Normal,
    Insert,
    Command,
}

enum Key {
    Char(u8),
    Enter,
    Backspace,
    Delete,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Unknown,
}

// --- Undo history ---

enum UndoAction {
    InsertChar { row: usize, col: usize },
    DeleteChar { row: usize, col: usize, ch: char },
    SplitLine { row: usize, col: usize },
    JoinLine { row: usize, col: usize },
    InsertLine { row: usize },
    DeleteLine { row: usize, content: String },
}

// --- Key reading ---

/// Read one key from stdin, handling ANSI escape sequences.
fn read_key() -> Key {
    let mut buf = [0u8; 1];
    loop {
        let n = sys_read(0, &mut buf);
        if n <= 0 {
            continue;
        }
        break;
    }

    let b = buf[0];

    if b == 0x1b {
        // Check for follow-up bytes within 50 ms.
        if !poll_stdin(50) {
            return Key::Escape;
        }
        // Read the bracket or letter.
        let mut seq = [0u8; 4];
        let n = sys_read(0, &mut seq[..1]);
        if n <= 0 {
            return Key::Escape;
        }
        if seq[0] != b'[' {
            return Key::Escape;
        }
        // Read next byte.
        let n = sys_read(0, &mut seq[1..2]);
        if n <= 0 {
            return Key::Escape;
        }
        match seq[1] {
            b'A' => return Key::Up,
            b'B' => return Key::Down,
            b'C' => return Key::Right,
            b'D' => return Key::Left,
            b'H' => return Key::Home,
            b'F' => return Key::End,
            b'3' => {
                // Could be [3~ (Delete)
                if poll_stdin(50) {
                    let n = sys_read(0, &mut seq[2..3]);
                    if n > 0 && seq[2] == b'~' {
                        return Key::Delete;
                    }
                }
                return Key::Unknown;
            }
            _ => return Key::Unknown,
        }
    }

    match b {
        0x7f | 0x08 => Key::Backspace,
        0x0d | 0x0a => Key::Enter,
        0x20..=0x7e => Key::Char(b),
        // Control characters passed through as Char so callers can act on them.
        _ => Key::Char(b),
    }
}

// --- Editor struct ---

struct Editor {
    lines: Vec<String>,
    cx: usize,
    cy: usize,
    scroll: usize,
    mode: Mode,
    filename: Option<String>,
    modified: bool,
    running: bool,
    status_msg: String,
    command_buf: String,
    pending: Option<u8>,
    undo_stack: Vec<UndoAction>,
}

impl Editor {
    fn new(filename: Option<String>) -> Self {
        let lines = match &filename {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(content) => {
                    let mut ls: Vec<String> = content.lines().map(|l| l.to_string()).collect();
                    if ls.is_empty() {
                        ls.push(String::new());
                    }
                    ls
                }
                Err(_) => vec![String::new()],
            },
            None => vec![String::new()],
        };

        Editor {
            lines,
            cx: 0,
            cy: 0,
            scroll: 0,
            mode: Mode::Normal,
            filename,
            modified: false,
            running: true,
            status_msg: String::new(),
            command_buf: String::new(),
            pending: None,
            undo_stack: Vec::new(),
        }
    }

    /// Ensure the cursor row is within the visible viewport; adjust scroll if needed.
    fn ensure_cursor_in_view(&mut self) {
        if self.cy < self.scroll {
            self.scroll = self.cy;
        } else if self.cy >= self.scroll + TEXT_ROWS {
            self.scroll = self.cy - TEXT_ROWS + 1;
        }
    }

    /// Clamp cursor column to valid range for the current line and mode.
    fn clamp_cursor(&mut self) {
        let len = self.lines[self.cy].len();
        let max_col = if self.mode == Mode::Insert {
            len
        } else {
            len.saturating_sub(1)
        };
        if self.cx > max_col {
            self.cx = max_col;
        }
    }

    /// Render the full screen.
    fn draw(&self) {
        let out = stdout();
        let mut w = BufWriter::new(out.lock());

        // Hide cursor, move to top-left.
        write!(w, "\x1b[?25l\x1b[H").unwrap();

        for row in 0..TEXT_ROWS {
            let buf_row = self.scroll + row;
            if buf_row < self.lines.len() {
                let line = &self.lines[buf_row];
                // Truncate to COLS columns.
                let display: &str = if line.len() > COLS {
                    &line[..COLS]
                } else {
                    line.as_str()
                };
                write!(w, "{}\x1b[K\r\n", display).unwrap();
            } else {
                // Past end of file: blue tilde.
                write!(w, "\x1b[34m~\x1b[0m\x1b[K\r\n").unwrap();
            }
        }

        // Status line (last row).
        write!(w, "\x1b[7m").unwrap(); // reverse video
        let status = self.build_status_line();
        // Pad or truncate to COLS.
        let padded = format!("{:<width$}", status, width = COLS);
        let truncated = if padded.len() > COLS {
            &padded[..COLS]
        } else {
            padded.as_str()
        };
        write!(w, "{}", truncated).unwrap();
        write!(w, "\x1b[0m").unwrap(); // reset attributes

        // Position cursor: screen_row is cy - scroll, screen_col is cx.
        let screen_row = self.cy.saturating_sub(self.scroll);
        let screen_col = self.cx;
        write!(w, "\x1b[{};{}H", screen_row + 1, screen_col + 1).unwrap();

        // Show cursor.
        write!(w, "\x1b[?25h").unwrap();

        w.flush().unwrap();
    }

    fn build_status_line(&self) -> String {
        match self.mode {
            Mode::Command => format!(":{}  ", self.command_buf),
            _ => {
                let mode_str = match self.mode {
                    Mode::Normal => "NORMAL",
                    Mode::Insert => "INSERT",
                    Mode::Command => "COMMAND",
                };
                let name = self
                    .filename
                    .as_deref()
                    .unwrap_or("[No Name]");
                let modified = if self.modified { " [+]" } else { "" };
                let pos = format!("{}:{}", self.cy + 1, self.cx + 1);
                if self.status_msg.is_empty() {
                    format!(" {} | {}{} | {}", mode_str, name, modified, pos)
                } else {
                    format!(" {} | {} | {}", mode_str, self.status_msg, pos)
                }
            }
        }
    }

    /// Dispatch key to the appropriate mode handler.
    fn handle_key(&mut self, key: Key) {
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Insert => self.handle_insert_key(key),
            Mode::Command => self.handle_command_key(key),
        }
    }

    // --- Normal mode ---

    fn handle_normal_key(&mut self, key: Key) {
        self.status_msg.clear();
        let old_pending = self.pending;
        self.pending = None;

        match key {
            // Movement: left
            Key::Char(b'h') | Key::Left => {
                if self.cx > 0 {
                    self.cx -= 1;
                }
            }
            // Movement: right
            Key::Char(b'l') | Key::Right => {
                let max = self.lines[self.cy].len().saturating_sub(1);
                if self.cx < max {
                    self.cx += 1;
                }
            }
            // Movement: down
            Key::Char(b'j') | Key::Down => {
                if self.cy < self.lines.len() - 1 {
                    self.cy += 1;
                }
                self.clamp_cursor();
            }
            // Movement: up
            Key::Char(b'k') | Key::Up => {
                if self.cy > 0 {
                    self.cy -= 1;
                }
                self.clamp_cursor();
            }
            // Line start
            Key::Char(b'0') | Key::Home => {
                self.cx = 0;
            }
            // Line end
            Key::Char(b'$') | Key::End => {
                self.cx = self.lines[self.cy].len().saturating_sub(1);
            }
            // Word forward
            Key::Char(b'w') => {
                self.move_word_forward();
            }
            // Word backward
            Key::Char(b'b') => {
                self.move_word_backward();
            }
            // gg / G
            Key::Char(b'g') => {
                if old_pending == Some(b'g') {
                    self.cy = 0;
                    self.cx = 0;
                } else {
                    self.pending = Some(b'g');
                    return;
                }
            }
            Key::Char(b'G') => {
                self.cy = self.lines.len() - 1;
                self.cx = 0;
            }
            // Enter Insert mode
            Key::Char(b'i') => {
                self.mode = Mode::Insert;
                return;
            }
            // Enter Insert mode after cursor
            Key::Char(b'a') => {
                let line_len = self.lines[self.cy].len();
                self.cx = (self.cx + 1).min(line_len);
                self.mode = Mode::Insert;
                return;
            }
            // Open line below
            Key::Char(b'o') => {
                let new_row = self.cy + 1;
                self.lines.insert(new_row, String::new());
                self.cy = new_row;
                self.cx = 0;
                self.mode = Mode::Insert;
                self.modified = true;
                self.undo_stack.push(UndoAction::InsertLine { row: new_row });
                return;
            }
            // Open line above
            Key::Char(b'O') => {
                self.lines.insert(self.cy, String::new());
                self.cx = 0;
                self.mode = Mode::Insert;
                self.modified = true;
                self.undo_stack.push(UndoAction::InsertLine { row: self.cy });
                return;
            }
            // Enter Command mode
            Key::Char(b':') => {
                self.mode = Mode::Command;
                self.command_buf.clear();
                return;
            }
            // Delete char under cursor
            Key::Char(b'x') => {
                let line_len = self.lines[self.cy].len();
                if line_len > 0 && self.cx < line_len {
                    let removed_col = self.cx;
                    let ch = self.lines[self.cy].remove(self.cx);
                    if self.cx > 0 && self.cx >= self.lines[self.cy].len() {
                        self.cx = self.lines[self.cy].len().saturating_sub(1);
                    }
                    self.modified = true;
                    self.undo_stack.push(UndoAction::DeleteChar {
                        row: self.cy,
                        col: removed_col,
                        ch,
                    });
                }
            }
            // dd: delete line
            Key::Char(b'd') => {
                if old_pending == Some(b'd') {
                    let content = self.lines[self.cy].clone();
                    self.undo_stack.push(UndoAction::DeleteLine {
                        row: self.cy,
                        content,
                    });
                    self.lines.remove(self.cy);
                    if self.lines.is_empty() {
                        self.lines.push(String::new());
                    }
                    if self.cy >= self.lines.len() {
                        self.cy = self.lines.len() - 1;
                    }
                    self.cx = 0;
                    self.modified = true;
                } else {
                    self.pending = Some(b'd');
                    return;
                }
            }
            // Undo
            Key::Char(b'u') => {
                self.undo();
            }
            // Quit via Ctrl-C
            Key::Char(0x03) => {
                self.running = false;
                return;
            }
            _ => {}
        }

        self.ensure_cursor_in_view();
    }

    fn move_word_forward(&mut self) {
        let line = &self.lines[self.cy];
        let bytes = line.as_bytes();
        let mut col = self.cx;

        // Skip non-whitespace (current word).
        while col < bytes.len() && bytes[col] != b' ' && bytes[col] != b'\t' {
            col += 1;
        }
        // Skip whitespace.
        while col < bytes.len() && (bytes[col] == b' ' || bytes[col] == b'\t') {
            col += 1;
        }

        if col >= bytes.len() {
            // Move to start of next line if possible.
            if self.cy + 1 < self.lines.len() {
                self.cy += 1;
                self.cx = 0;
            } else {
                // Stay at end of current line.
                self.cx = bytes.len().saturating_sub(1);
            }
        } else {
            self.cx = col;
        }
    }

    fn move_word_backward(&mut self) {
        if self.cx == 0 {
            // Move to end of previous line if possible.
            if self.cy > 0 {
                self.cy -= 1;
                self.cx = self.lines[self.cy].len().saturating_sub(1);
            }
            return;
        }

        let line = &self.lines[self.cy];
        let bytes = line.as_bytes();
        let mut col = self.cx;

        // Step back one before scanning.
        if col > 0 {
            col -= 1;
        }
        // Skip whitespace backwards.
        while col > 0 && (bytes[col] == b' ' || bytes[col] == b'\t') {
            col -= 1;
        }
        // Skip non-whitespace backwards to find word start.
        while col > 0 && bytes[col - 1] != b' ' && bytes[col - 1] != b'\t' {
            col -= 1;
        }

        self.cx = col;
    }

    // --- Insert mode ---

    fn handle_insert_key(&mut self, key: Key) {
        match key {
            Key::Escape => {
                self.mode = Mode::Normal;
                if self.cx > 0 {
                    self.cx -= 1;
                }
            }
            Key::Char(byte) => {
                let row = self.cy;
                let col = self.cx;
                self.lines[row].insert(col, byte as char);
                self.cx += 1;
                self.modified = true;
                self.undo_stack.push(UndoAction::InsertChar { row, col });
            }
            Key::Enter => {
                let row = self.cy;
                let col = self.cx;
                let tail = self.lines[row][col..].to_string();
                self.lines[row].truncate(col);
                self.lines.insert(row + 1, tail);
                self.cy += 1;
                self.cx = 0;
                self.modified = true;
                self.undo_stack.push(UndoAction::SplitLine { row, col });
            }
            Key::Backspace => {
                if self.cx > 0 {
                    let row = self.cy;
                    let col = self.cx - 1;
                    let ch = self.lines[row].remove(col);
                    self.cx -= 1;
                    self.modified = true;
                    self.undo_stack.push(UndoAction::DeleteChar { row, col, ch });
                } else if self.cy > 0 {
                    // Join with previous line.
                    let row = self.cy;
                    let col = self.lines[row - 1].len();
                    let current = self.lines.remove(row);
                    self.lines[row - 1].push_str(&current);
                    self.cy -= 1;
                    self.cx = col;
                    self.modified = true;
                    self.undo_stack.push(UndoAction::JoinLine { row: row - 1, col });
                }
            }
            Key::Delete => {
                let row = self.cy;
                let col = self.cx;
                let line_len = self.lines[row].len();
                if col < line_len {
                    let ch = self.lines[row].remove(col);
                    self.modified = true;
                    self.undo_stack.push(UndoAction::DeleteChar { row, col, ch });
                } else if row + 1 < self.lines.len() {
                    // Join next line into current.
                    let join_col = self.lines[row].len();
                    let next = self.lines.remove(row + 1);
                    self.lines[row].push_str(&next);
                    self.modified = true;
                    self.undo_stack
                        .push(UndoAction::JoinLine { row, col: join_col });
                }
            }
            // Arrow keys and Home/End behave like normal mode movement.
            Key::Left => {
                if self.cx > 0 {
                    self.cx -= 1;
                }
                self.ensure_cursor_in_view();
            }
            Key::Right => {
                let max = self.lines[self.cy].len();
                if self.cx < max {
                    self.cx += 1;
                }
                self.ensure_cursor_in_view();
            }
            Key::Up => {
                if self.cy > 0 {
                    self.cy -= 1;
                }
                self.clamp_cursor();
                self.ensure_cursor_in_view();
            }
            Key::Down => {
                if self.cy < self.lines.len() - 1 {
                    self.cy += 1;
                }
                self.clamp_cursor();
                self.ensure_cursor_in_view();
            }
            Key::Home => {
                self.cx = 0;
                self.ensure_cursor_in_view();
            }
            Key::End => {
                self.cx = self.lines[self.cy].len();
                self.ensure_cursor_in_view();
            }
            _ => {}
        }
    }

    // --- Command mode ---

    fn handle_command_key(&mut self, key: Key) {
        match key {
            Key::Escape => {
                self.mode = Mode::Normal;
                self.command_buf.clear();
            }
            Key::Enter => {
                self.execute_command();
                self.mode = Mode::Normal;
            }
            Key::Backspace => {
                if self.command_buf.is_empty() {
                    self.mode = Mode::Normal;
                } else {
                    self.command_buf.pop();
                }
            }
            Key::Char(byte) => {
                self.command_buf.push(byte as char);
            }
            _ => {}
        }
    }

    fn execute_command(&mut self) {
        let cmd = self.command_buf.clone();
        let trimmed = cmd.trim();

        if trimmed == "w" {
            self.save();
        } else if let Some(rest) = trimmed.strip_prefix("w ") {
            let fname = rest.trim().to_string();
            if !fname.is_empty() {
                self.filename = Some(fname);
            }
            self.save();
        } else if trimmed == "q" {
            if self.modified {
                self.status_msg =
                    "No write since last change (add ! to override)".to_string();
            } else {
                self.running = false;
            }
        } else if trimmed == "wq" {
            self.save();
            if self.running {
                // save() may have set an error; only quit on success.
                if !self.modified {
                    self.running = false;
                }
            }
        } else if trimmed == "q!" {
            self.running = false;
        } else {
            self.status_msg = format!("Not an editor command: {}", trimmed);
        }
    }

    fn save(&mut self) {
        let path = match &self.filename {
            Some(p) => p.clone(),
            None => {
                self.status_msg = "No filename".to_string();
                return;
            }
        };

        let mut content = self.lines.join("\n");
        content.push('\n');
        let content_len = content.len() as u64;

        match std::fs::File::create(&path) {
            Ok(mut file) => match file.write_all(content.as_bytes()) {
                Ok(()) => {
                    // Explicit truncate: set_len handles EDOS where truncate is a no-op.
                    let _ = file.set_len(content_len);
                    let nlines = self.lines.len();
                    self.modified = false;
                    self.status_msg =
                        format!("\"{}\" written, {} lines", path, nlines);
                }
                Err(e) => {
                    self.status_msg = format!("Error writing: {}", e);
                }
            },
            Err(e) => {
                self.status_msg = format!("Error writing: {}", e);
            }
        }
    }

    // --- Undo ---

    fn undo(&mut self) {
        let action = match self.undo_stack.pop() {
            Some(a) => a,
            None => {
                self.status_msg = "Already at oldest change".to_string();
                return;
            }
        };

        match action {
            UndoAction::InsertChar { row, col } => {
                // Reverse of insert: remove the char that was inserted.
                if row < self.lines.len() && col < self.lines[row].len() {
                    self.lines[row].remove(col);
                }
                self.cy = row;
                self.cx = col;
            }
            UndoAction::DeleteChar { row, col, ch } => {
                // Reverse of delete: re-insert the char.
                if row < self.lines.len() {
                    let insert_col = col.min(self.lines[row].len());
                    self.lines[row].insert(insert_col, ch);
                }
                self.cy = row;
                self.cx = col;
            }
            UndoAction::SplitLine { row, col } => {
                // Reverse of split: join line row and row+1.
                if row + 1 < self.lines.len() {
                    let next = self.lines.remove(row + 1);
                    self.lines[row].push_str(&next);
                }
                self.cy = row;
                self.cx = col;
            }
            UndoAction::JoinLine { row, col } => {
                // Reverse of join: split line row at col into two lines.
                if row < self.lines.len() {
                    let tail = self.lines[row][col..].to_string();
                    self.lines[row].truncate(col);
                    self.lines.insert(row + 1, tail);
                }
                self.cy = row + 1;
                self.cx = 0;
            }
            UndoAction::InsertLine { row } => {
                // Reverse of insert line: remove it.
                if row < self.lines.len() {
                    self.lines.remove(row);
                }
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
                self.cy = row.saturating_sub(1).min(self.lines.len() - 1);
                self.cx = 0;
            }
            UndoAction::DeleteLine { row, content } => {
                // Reverse of delete line: re-insert at row.
                self.lines.insert(row, content);
                self.cy = row;
                self.cx = 0;
            }
        }

        self.clamp_cursor();
        self.ensure_cursor_in_view();
    }
}

fn main() {
    let filename = env::args().nth(1);

    let mut editor = Editor::new(filename);

    // Enter raw mode.
    pty_set_raw(0);

    // Main loop: draw -> read key -> handle key.
    while editor.running {
        editor.ensure_cursor_in_view();
        editor.clamp_cursor();
        editor.draw();

        let key = read_key();
        editor.handle_key(key);
    }

    // Restore canonical mode and clear screen on exit.
    pty_set_canonical(0);
    print!("\x1b[2J\x1b[H");
    let _ = stdout().flush();
}
