//! Terminal text: a line of output as the columns it occupies.
//!
//! Programs here colour their own output — `ps` colours the state column, `ls`
//! the file types — so the characters in a line and the columns on the screen
//! are not the same sequence. Anything that clips a line to the screen width,
//! scrolls it sideways or paints over part of it has to separate the escape
//! sequences out first: counting them as columns clips a line short, and
//! splitting one in half prints its tail as text.

use std::string::String;
use std::vec::Vec;

/// A tab stops every eight columns, which is what the terminal assumes.
pub const TAB_WIDTH: usize = 8;

/// One column of a line: the character in it, and any escape sequences that
/// come immediately before it.
#[derive(Clone)]
pub struct Cell {
    pub escapes: String,
    pub ch: char,
}

/// Split a line into columns, expanding tabs to the next eight-column stop
/// while doing it, since the column a tab lands on is only known here.
pub fn cells(line: &str) -> Vec<Cell> {
    let mut cells: Vec<Cell> = Vec::new();
    let mut pending = String::new();
    let mut chars = line.trim_end_matches('\r').chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            pending.push(c);
            // A CSI sequence runs to its final byte; anything else is the two
            // characters of an escape and its selector.
            if chars.peek() == Some(&'[') {
                pending.push(chars.next().unwrap_or('['));
                for c in chars.by_ref() {
                    pending.push(c);
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            } else if let Some(c) = chars.next() {
                pending.push(c);
            }
            continue;
        }
        let width = if c == '\t' {
            TAB_WIDTH - cells.len() % TAB_WIDTH
        } else {
            1
        };
        for i in 0..width {
            cells.push(Cell {
                escapes: if i == 0 {
                    core::mem::take(&mut pending)
                } else {
                    String::new()
                },
                ch: if c == '\t' { ' ' } else { c },
            });
        }
    }
    cells
}

/// Clip to the screen width, leaving the last column empty so that a full-width
/// line does not make the terminal wrap and cost a second row.
pub fn clip(line: &str, cols: usize) -> Vec<Cell> {
    let mut cells = cells(line);
    cells.truncate(cols.saturating_sub(1));
    cells
}

/// The `width` columns starting at `start`.
///
/// The escapes of every column scrolled past are carried into the first visible
/// one, so a line scrolled sideways keeps the colour that was set before the
/// part now on screen.
pub fn window(cells: &[Cell], start: usize, width: usize) -> Vec<Cell> {
    let mut carried = String::new();
    for cell in cells.iter().take(start) {
        carried.push_str(&cell.escapes);
    }
    let mut out: Vec<Cell> = cells.iter().skip(start).take(width).cloned().collect();
    if let Some(first) = out.first_mut() {
        carried.push_str(&first.escapes);
        first.escapes = carried;
    }
    out
}

/// Write columns back out as text, optionally reverse-videoing every run that
/// differs from `previous`. A column `previous` does not have counts as
/// unchanged: a whole new line is obvious without marking every column in it.
///
/// Reverse video ends with SGR 27 rather than SGR 0 so that a highlight laid
/// over coloured output does not also switch the colour off.
pub fn render(cells: &[Cell], previous: Option<&[Cell]>) -> String {
    let mut out = String::new();
    let mut inverted = false;
    let mut coloured = false;
    for (column, cell) in cells.iter().enumerate() {
        if !cell.escapes.is_empty() {
            out.push_str(&cell.escapes);
            coloured = true;
        }
        let changed = previous.is_some_and(|p| p.get(column).map(|c| c.ch) != Some(cell.ch));
        if changed != inverted {
            out.push_str(if changed { "\x1b[7m" } else { "\x1b[27m" });
            inverted = changed;
        }
        out.push(cell.ch);
    }
    if inverted || coloured {
        out.push_str("\x1b[0m");
    }
    out
}
