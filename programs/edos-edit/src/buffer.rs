//! The document: a vector of lines, the cursor, and what open and save need to
//! know about the bytes that became them.
//!
//! A `Vec<Line>`, not a rope. The cost of a line vector is on inserting and
//! deleting *lines*, which moves pointers rather than text, and this editor is
//! comfortable to a few megabytes — past that a rope is the answer and this is
//! the wrong program.

use std::fs;

/// One line of text, without its terminator.
pub struct Line {
    pub text: String,
}

/// A place in the document, in characters rather than bytes. `text_input`
/// carries the same rule and says why: a byte index can land inside a
/// multi-byte character, and the next `String::insert` panics on the boundary
/// assertion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

/// The line ending a file was read with, preserved on save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Lf,
    CrLf,
}

/// A file over this size is refused rather than decoded: `Buffer::open` splits
/// the whole thing before the window redraws, and there is nothing on screen
/// to say why it stopped responding. Eight is the limit `edos-files` already
/// applies to previews, for the same reason.
const MAX_SIZE: u64 = 8 * 1024 * 1024;

/// The document a window is open on.
pub struct Buffer {
    /// None for a buffer that has never been saved.
    pub path: Option<String>,
    pub lines: Vec<Line>,
    pub cursor: Position,
    /// The other end of a selection. None when nothing is selected. Read once
    /// a selection can be made.
    #[allow(dead_code)]
    pub anchor: Option<Position>,
    pub scroll_line: usize,
    pub scroll_col: usize,
    /// Read on save, to write the file back in the ending it was found with.
    #[allow(dead_code)]
    pub eol: Eol,
    /// Whether the file on disk ended with a newline. Read on save, to decide
    /// whether to put the terminator back.
    #[allow(dead_code)]
    pub trailing_newline: bool,
    /// Whether opening this file needed lossy UTF-8 repair.
    pub repaired: bool,
    /// Read once the buffer can be edited.
    #[allow(dead_code)]
    pub dirty: bool,
}

impl Buffer {
    /// An empty, unsaved buffer.
    pub fn empty() -> Self {
        Self {
            path: None,
            lines: vec![Line {
                text: String::new(),
            }],
            cursor: Position::default(),
            anchor: None,
            scroll_line: 0,
            scroll_col: 0,
            eol: Eol::Lf,
            trailing_newline: true,
            repaired: false,
            dirty: false,
        }
    }

    /// Read `path` in. Files are decoded as UTF-8 with lossy replacement
    /// rather than refused, so a stray byte in a config file can still be
    /// fixed rather than only reported.
    pub fn open(path: &str) -> Result<Self, String> {
        let meta = fs::metadata(path).map_err(|err| format!("{path}: {err}"))?;
        if meta.len() > MAX_SIZE {
            return Err(format!(
                "{path} is {} — too large to open (limit 8 MiB).",
                format_size(meta.len())
            ));
        }

        let bytes = fs::read(path).map_err(|err| format!("{path}: {err}"))?;
        let (raw, repaired) = match String::from_utf8(bytes) {
            Ok(text) => (text, false),
            Err(err) => (String::from_utf8_lossy(err.as_bytes()).into_owned(), true),
        };

        let eol = if raw.contains("\r\n") {
            Eol::CrLf
        } else {
            Eol::Lf
        };
        // Line endings are normalized before splitting so a CRLF file's last
        // line does not carry a stray `\r`, then split back out per `Eol` on
        // save.
        let normalized = raw.replace("\r\n", "\n");
        let trailing_newline = normalized.ends_with('\n');
        let body = normalized.strip_suffix('\n').unwrap_or(&normalized);
        let lines = if body.is_empty() {
            vec![Line {
                text: String::new(),
            }]
        } else {
            body.split('\n')
                .map(|text| Line {
                    text: text.to_string(),
                })
                .collect()
        };

        Ok(Self {
            path: Some(path.to_string()),
            lines,
            cursor: Position::default(),
            anchor: None,
            scroll_line: 0,
            scroll_col: 0,
            eol,
            trailing_newline,
            repaired,
            dirty: false,
        })
    }

    /// Number of characters on line `index`, or 0 past the end of the buffer.
    pub fn line_chars(&self, index: usize) -> usize {
        self.lines
            .get(index)
            .map_or(0, |line| line.text.chars().count())
    }

    /// The character at `pos`, or None past the end of its line or the file.
    /// Read once anything needs to look at an arbitrary character rather than
    /// a whole line — finding a match, or reading either side of the cursor.
    #[allow(dead_code)]
    pub fn char_at(&self, pos: Position) -> Option<char> {
        self.lines.get(pos.line)?.text.chars().nth(pos.col)
    }

    /// Pull the cursor back inside the buffer, after the line count or a
    /// line's length has changed under it.
    pub fn clamp_cursor(&mut self) {
        self.cursor.line = self.cursor.line.min(self.lines.len().saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.line_chars(self.cursor.line));
    }
}

/// A byte count in the units `edos-files` already uses for the same warning.
fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [
        (1024 * 1024 * 1024, "GiB"),
        (1024 * 1024, "MiB"),
        (1024, "KiB"),
    ];
    for (scale, suffix) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {suffix}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} bytes")
}
