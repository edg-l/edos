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
    /// Differs from what was read off the disk.
    pub changed: bool,
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
    pub eol: Eol,
    /// Whether the file on disk ended with a newline. Read on save, to decide
    /// whether to put the terminator back.
    pub trailing_newline: bool,
    /// Whether opening this file needed lossy UTF-8 repair.
    pub repaired: bool,
    /// Whether the buffer differs from what is on disk.
    pub dirty: bool,
    /// Every edit made since the buffer was opened, in order.
    pub log: Vec<LogEntry>,
    /// Position in `log`: undo pops the entry before it, redo pushes the
    /// entry at it. Entries at or past this point are what redo can still
    /// reach.
    pub log_at: usize,
    /// `log_at` as of the last save. Undo or redo landing back on this value
    /// means the buffer is exactly the file on disk again, which is the
    /// change ribbon's rule for clearing every flag at once.
    pub saved_log_at: usize,
    /// Whether the next single-character insert may merge into the log's
    /// last entry instead of becoming its own, so a run of typing undoes as
    /// one word rather than one letter at a time.
    coalescing: bool,
}

/// One reversible change to the document. Each is its own inverse with the
/// variant swapped: undoing an insert is deleting what it inserted, and
/// undoing a delete is putting back what it removed.
#[derive(Clone)]
pub enum Edit {
    Insert { at: Position, text: String },
    Delete { at: Position, text: String },
}

/// One entry in the undo log: the edit, and where the cursor sat just before
/// it. An undo that only reversed the text and left the cursor wherever the
/// edit happened to leave it would make the next keystroke land in the wrong
/// place.
pub struct LogEntry {
    pub edit: Edit,
    pub cursor_before: Position,
}

/// Where `at` lands once `text` — which may itself contain `\n` — has been
/// inserted there. Shared by `insert_text`, which returns this as the
/// caret's new position, and `apply`'s `Delete` arm, which needs it to turn
/// an edit's `(at, text)` into the range `delete_range` takes.
fn end_position(at: Position, text: &str) -> Position {
    let mut pos = at;
    for (i, part) in text.split('\n').enumerate() {
        if i == 0 {
            pos.col += part.chars().count();
        } else {
            pos.line += 1;
            pos.col = part.chars().count();
        }
    }
    pos
}

impl Buffer {
    /// An empty, unsaved buffer.
    pub fn empty() -> Self {
        Self {
            path: None,
            lines: vec![Line {
                text: String::new(),
                changed: false,
            }],
            cursor: Position::default(),
            anchor: None,
            scroll_line: 0,
            scroll_col: 0,
            eol: Eol::Lf,
            trailing_newline: true,
            repaired: false,
            dirty: false,
            log: Vec::new(),
            log_at: 0,
            saved_log_at: 0,
            coalescing: false,
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
                changed: false,
            }]
        } else {
            body.split('\n')
                .map(|text| Line {
                    text: text.to_string(),
                    changed: false,
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
            log: Vec::new(),
            log_at: 0,
            saved_log_at: 0,
            coalescing: false,
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

    // --- Mutation -----------------------------------------------------------
    //
    // Every public mutation routes through these two: inserting a character,
    // pasting, undo and redo all end up here rather than poking `lines`
    // directly.

    /// Insert `text` at `at`, splitting into lines wherever it contains
    /// `\n`. Returns the position immediately after the inserted text.
    pub fn insert_text(&mut self, at: Position, text: &str) -> Position {
        if text.is_empty() {
            return at;
        }
        let chars: Vec<char> = self.lines[at.line].text.chars().collect();
        let before: String = chars[..at.col].iter().collect();
        let after: String = chars[at.col..].iter().collect();

        let parts: Vec<&str> = text.split('\n').collect();
        let last = parts.len() - 1;
        let mut new_lines: Vec<Line> = parts
            .into_iter()
            .enumerate()
            .map(|(i, part)| Line {
                text: if i == 0 {
                    format!("{before}{part}")
                } else {
                    part.to_string()
                },
                changed: true,
            })
            .collect();
        new_lines[last].text.push_str(&after);

        let end = end_position(at, text);
        self.lines.splice(at.line..=at.line, new_lines);
        self.dirty = true;
        end
    }

    /// Remove the text between `from` and `to` (`from` must not come after
    /// `to`), joining the lines the range spans into one. Returns the
    /// removed text.
    pub fn delete_range(&mut self, from: Position, to: Position) -> String {
        if from == to {
            return String::new();
        }
        let from_chars: Vec<char> = self.lines[from.line].text.chars().collect();
        let before: String = from_chars[..from.col].iter().collect();
        let to_chars: Vec<char> = self.lines[to.line].text.chars().collect();
        let after: String = to_chars[to.col..].iter().collect();

        let mut removed = String::new();
        if from.line == to.line {
            removed.extend(&from_chars[from.col..to.col]);
        } else {
            removed.extend(&from_chars[from.col..]);
            for line in &self.lines[from.line + 1..to.line] {
                removed.push('\n');
                removed.push_str(&line.text);
            }
            removed.push('\n');
            removed.extend(&to_chars[..to.col]);
        }

        self.lines.splice(
            from.line..=to.line,
            [Line {
                text: format!("{before}{after}"),
                changed: true,
            }],
        );
        self.dirty = true;
        removed
    }

    // --- Undo log -------------------------------------------------------

    /// Apply `edit` to the buffer, returning where the cursor lands. The one
    /// path a fresh edit, an undo (given `Buffer::invert` of the logged
    /// edit) and a redo (given the logged edit itself) all run through.
    pub fn apply(&mut self, edit: &Edit) -> Position {
        match edit {
            Edit::Insert { at, text } => self.insert_text(*at, text),
            Edit::Delete { at, text } => {
                self.delete_range(*at, end_position(*at, text));
                *at
            }
        }
    }

    /// The edit that undoes `edit`.
    pub fn invert(edit: &Edit) -> Edit {
        match edit {
            Edit::Insert { at, text } => Edit::Delete {
                at: *at,
                text: text.clone(),
            },
            Edit::Delete { at, text } => Edit::Insert {
                at: *at,
                text: text.clone(),
            },
        }
    }

    /// Record `edit` in the log, coalescing it into the previous entry when
    /// both are single-character inserts that abut, so a run of typing
    /// undoes as one word rather than one letter at a time. Anything already
    /// undone past this point is dropped first: a fresh edit made after an
    /// undo removes the redo branch rather than forking it.
    pub fn push_edit(&mut self, edit: Edit, cursor_before: Position) {
        self.log.truncate(self.log_at);

        let merges_into_last = self.coalescing
            && match (&edit, self.log.last()) {
                (
                    Edit::Insert { at, text },
                    Some(LogEntry {
                        edit:
                            Edit::Insert {
                                at: prev_at,
                                text: prev_text,
                            },
                        ..
                    }),
                ) => text.chars().count() == 1 && *at == end_position(*prev_at, prev_text),
                _ => false,
            };

        if merges_into_last {
            let Edit::Insert { text, .. } = edit else {
                unreachable!()
            };
            let Some(LogEntry {
                edit: Edit::Insert {
                    text: prev_text, ..
                },
                ..
            }) = self.log.last_mut()
            else {
                unreachable!()
            };
            prev_text.push_str(&text);
        } else {
            self.log.push(LogEntry {
                edit,
                cursor_before,
            });
            self.log_at += 1;
        }
        self.coalescing = true;
    }

    /// Stop the next single-character insert from merging into the log's
    /// last entry. Called on every cursor move, `\n` insert, delete, and
    /// save.
    pub fn break_coalesce(&mut self) {
        self.coalescing = false;
    }

    /// Undo the most recent edit, restoring the cursor to where it sat just
    /// before that edit. Does nothing at the start of the log.
    pub fn undo(&mut self) -> bool {
        if self.log_at == 0 {
            return false;
        }
        self.log_at -= 1;
        let entry_edit = self.log[self.log_at].edit.clone();
        let cursor_before = self.log[self.log_at].cursor_before;
        self.apply(&Self::invert(&entry_edit));
        self.cursor = cursor_before;
        self.clamp_cursor();
        self.clear_ribbon_if_saved();
        self.dirty = self.log_at != self.saved_log_at;
        self.break_coalesce();
        true
    }

    /// Redo the edit most recently undone. Does nothing once the log is
    /// caught up.
    pub fn redo(&mut self) -> bool {
        if self.log_at >= self.log.len() {
            return false;
        }
        let entry_edit = self.log[self.log_at].edit.clone();
        let cursor = self.apply(&entry_edit);
        self.log_at += 1;
        self.cursor = cursor;
        self.clamp_cursor();
        self.clear_ribbon_if_saved();
        self.dirty = self.log_at != self.saved_log_at;
        self.break_coalesce();
        true
    }

    /// A `bool` per line cannot tell on its own that an undo has walked the
    /// log back past a save, so every flag clears at once whenever `log_at`
    /// returns to `saved_log_at` — the point where the buffer is exactly the
    /// file on disk again. Undoing only part of the way back leaves a line
    /// marked even though it happens to match the disk again; that is left
    /// over-reporting rather than checked against a second copy of the file.
    fn clear_ribbon_if_saved(&mut self) {
        if self.log_at == self.saved_log_at {
            for line in &mut self.lines {
                line.changed = false;
            }
        }
    }

    /// Write the buffer to `path` in the ending and trailing-newline shape
    /// it was read with, then mark the result as matching the disk.
    pub fn save(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err("No path to save to.".to_string());
        };
        let sep = match self.eol {
            Eol::Lf => "\n",
            Eol::CrLf => "\r\n",
        };
        let mut body = self
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join(sep);
        if self.trailing_newline {
            body.push_str(sep);
        }
        fs::write(&path, body.as_bytes()).map_err(|err| format!("{path}: {err}"))?;

        for line in &mut self.lines {
            line.changed = false;
        }
        self.dirty = false;
        self.saved_log_at = self.log_at;
        self.break_coalesce();
        Ok(())
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
