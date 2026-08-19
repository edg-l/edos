//! less - read something longer than the screen.
//!
//! Every text tool here writes to a terminal that scrolls and keeps nothing, so
//! `dmesg`, a long `ps` or a source file are readable only in pieces through
//! `head` and `tail`. This holds the whole text and moves a window over it:
//! line and page motion, sideways motion for output wider than the screen,
//! search with the matches highlighted, and `:n`/`:p` across several files.
//!
//! The text is read into memory whole, which is what makes `dmesg | less` work
//! at all: the pipe has to be drained before the keyboard can be read, and on
//! this system the keyboard is the same terminal the pager is drawing on. When
//! stdin is not a terminal the keys come from stderr instead, which is the
//! descriptor a pipeline leaves pointing at the PTY.

use std::io::{Read, Write, stdin, stdout};
use std::process::exit;

use edos_lib::io::{get_winsize, isatty, poll_readable, pty_set_canonical, pty_set_raw, sys_read};
use edos_lib::term::{Cell, cells, render, window};

const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;
/// How far left and right arrows move, in columns.
const SHIFT_STEP: usize = 8;
/// A lone Escape and the start of an arrow key are the same byte, so a sequence
/// that does not continue within this long is the key itself.
const ESCAPE_WAIT_MS: u64 = 50;

struct Options {
    line_numbers: bool,
    ignore_case: bool,
}

fn usage() -> ! {
    eprintln!("usage: less [-N] [-i] [file...]");
    eprintln!("  -N, --line-numbers   number the lines");
    eprintln!("  -i, --ignore-case    searches ignore case");
    eprintln!();
    eprintln!("keys: j/k or arrows line, space/b page, d/u half page, g/G top/bottom,");
    eprintln!("      left/right scroll sideways, /pat and ?pat search, n/N repeat,");
    eprintln!("      :n and :p change file, h help, q quit");
    exit(1)
}

fn parse_args() -> (Options, Vec<String>) {
    let mut options = Options {
        line_numbers: false,
        ignore_case: false,
    };
    let mut files = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-N" | "--line-numbers" => options.line_numbers = true,
            "-i" | "--ignore-case" => options.ignore_case = true,
            "-h" | "--help" => usage(),
            _ => files.push(arg),
        }
    }
    (options, files)
}

/// One file's text, already split into columns so that a line is measured and
/// sliced the same way every frame.
struct Document {
    name: String,
    lines: Vec<Vec<Cell>>,
    /// The characters of each line with the escapes taken out, so that a search
    /// hit is a column range and can be highlighted in place.
    plain: Vec<String>,
    /// First line on screen, and the column the screen starts at.
    top: usize,
    left: usize,
}

impl Document {
    fn new(name: String, text: &str) -> Self {
        let mut lines: Vec<Vec<Cell>> = text.split('\n').map(cells).collect();
        // A trailing newline ends the last line; it does not start an empty one.
        if lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let plain = lines
            .iter()
            .map(|line| line.iter().map(|c| c.ch).collect())
            .collect();
        Self {
            name,
            lines,
            plain,
            top: 0,
            left: 0,
        }
    }

    fn widest(&self) -> usize {
        self.lines.iter().map(|l| l.len()).max().unwrap_or(0)
    }
}

fn read_file(path: &str) -> Result<String, String> {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| format!("{path}: {e}"))
}

fn read_stdin() -> String {
    let mut bytes = Vec::new();
    let _ = stdin().read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

enum Key {
    Byte(u8),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Escape,
}

/// Read one keystroke, turning the escape sequence an arrow or paging key sends
/// into the key it stands for.
fn read_key(fd: u64) -> Option<Key> {
    let mut buf = [0u8; 1];
    if sys_read(fd, &mut buf) <= 0 {
        return None;
    }
    if buf[0] != 0x1b {
        return Some(Key::Byte(buf[0]));
    }
    if !poll_readable(fd, ESCAPE_WAIT_MS) || sys_read(fd, &mut buf) <= 0 {
        return Some(Key::Escape);
    }
    // Both CSI (`\x1b[`) and the application cursor keys (`\x1bO`) address the
    // same keys; anything else is an escape with a selector we do not use.
    if buf[0] != b'[' && buf[0] != b'O' {
        return Some(Key::Escape);
    }
    let mut params = String::new();
    loop {
        if sys_read(fd, &mut buf) <= 0 {
            return Some(Key::Escape);
        }
        let c = buf[0] as char;
        if ('\x40'..='\x7e').contains(&c) {
            return Some(match (c, params.as_str()) {
                ('A', _) => Key::Up,
                ('B', _) => Key::Down,
                ('C', _) => Key::Right,
                ('D', _) => Key::Left,
                ('H', _) | ('~', "1") | ('~', "7") => Key::Home,
                ('F', _) | ('~', "4") | ('~', "8") => Key::End,
                ('~', "5") => Key::PageUp,
                ('~', "6") => Key::PageDown,
                _ => Key::Escape,
            });
        }
        params.push(c);
    }
}

/// Reverse-video the columns `[start, end)` of a line by pushing the escapes
/// into the columns themselves, so the highlight survives clipping and sideways
/// motion the same way the line's own colours do.
fn highlight(line: &mut [Cell], start: usize, end: usize) {
    let len = line.len();
    if start >= len {
        return;
    }
    line[start].escapes.push_str("\x1b[7m");
    // A highlight that runs to the end of the line is closed by the reset
    // render() writes for any line carrying escapes at all.
    if let Some(cell) = line.get_mut(end.min(len)) {
        cell.escapes.insert_str(0, "\x1b[27m");
    }
}

fn matches(haystack: &str, needle: &str, ignore_case: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let (haystack, needle) = if ignore_case {
        (haystack.to_lowercase(), needle.to_lowercase())
    } else {
        (haystack.to_string(), needle.to_string())
    };
    // Byte offsets are column offsets only for ASCII; a match inside a
    // multi-byte character would be highlighted at the wrong column, so count
    // characters explicitly.
    let hay: Vec<char> = haystack.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    let mut found = Vec::new();
    if pat.len() > hay.len() {
        return found;
    }
    let mut i = 0;
    while i + pat.len() <= hay.len() {
        if hay[i..i + pat.len()] == pat[..] {
            found.push((i, i + pat.len()));
            i += pat.len();
        } else {
            i += 1;
        }
    }
    found
}

const HELP: &[&str] = &[
    "less - keys",
    "",
    "  j, down, enter    forward one line",
    "  k, up             backward one line",
    "  space, f, pgdn    forward one screen",
    "  b, pgup           backward one screen",
    "  d / u             forward / backward half a screen",
    "  g, home           first line          G, end   last line",
    "  left / right      scroll sideways",
    "  /pattern          search forward      ?pattern  search backward",
    "  n / N             next / previous match",
    "  :n / :p           next / previous file",
    "  h                 this help           q         quit",
];

struct Screen {
    cols: usize,
    rows: usize,
}

impl Screen {
    fn measure() -> Self {
        let (cols, rows) = get_winsize(1)
            .map(|(c, r)| (c as usize, r as usize))
            .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
        Self {
            cols: cols.max(20),
            rows: rows.max(3),
        }
    }

    /// Rows of text, the last one being the status line.
    fn text_rows(&self) -> usize {
        self.rows - 1
    }
}

struct Pager {
    documents: Vec<Document>,
    current: usize,
    options: Options,
    pattern: String,
    /// Shown on the status line in place of the position until the next key.
    message: String,
    help: bool,
}

impl Pager {
    fn document(&mut self) -> &mut Document {
        &mut self.documents[self.current]
    }

    fn scroll(&mut self, delta: isize, screen: &Screen) {
        let last = self.documents[self.current].lines.len();
        let page = screen.text_rows();
        // The last line stays reachable, but scrolling past it would leave the
        // screen blank with nothing to say where it is.
        let limit = last.saturating_sub(page.min(last).max(1));
        let doc = self.document();
        doc.top = doc.top.saturating_add_signed(delta).min(limit);
    }

    fn shift(&mut self, delta: isize, screen: &Screen) {
        let widest = self.documents[self.current].widest();
        let limit = widest.saturating_sub(screen.cols / 2);
        let doc = self.document();
        doc.left = doc.left.saturating_add_signed(delta).min(limit);
    }

    /// Search from the line after (or before) the top of the screen, and move
    /// the match to the top. A match in the last screenful is put at the top
    /// too, past where scrolling would stop, since otherwise a hit on the last
    /// line cannot be distinguished from no hit at all.
    fn search(&mut self, forward: bool) -> bool {
        let pattern = self.pattern.clone();
        let ignore_case = self.options.ignore_case;
        let doc = self.document();
        let count = doc.plain.len();
        let hit = if forward {
            (doc.top + 1..count)
                .find(|&i| !matches(&doc.plain[i], &pattern, ignore_case).is_empty())
        } else {
            (0..doc.top)
                .rev()
                .find(|&i| !matches(&doc.plain[i], &pattern, ignore_case).is_empty())
        };
        match hit {
            Some(line) => {
                doc.top = line;
                true
            }
            None => false,
        }
    }

    fn status(&self, screen: &Screen) -> String {
        if !self.message.is_empty() {
            return self.message.clone();
        }
        let doc = &self.documents[self.current];
        let total = doc.lines.len();
        let last = (doc.top + screen.text_rows()).min(total);
        let percent = (last * 100).checked_div(total).unwrap_or(100);
        let mut status = doc.name.clone();
        if self.documents.len() > 1 {
            status.push_str(&format!(
                " (file {} of {})",
                self.current + 1,
                self.documents.len()
            ));
        }
        status.push_str(&format!(
            " lines {}-{}/{} {}%",
            doc.top + 1,
            last,
            total,
            percent
        ));
        if doc.left > 0 {
            status.push_str(&format!(" col {}", doc.left + 1));
        }
        if last >= total {
            status.push_str(" (END)");
        }
        status
    }

    /// The visible rows: either the help, or the window over the text with the
    /// search matches highlighted.
    fn rows(&self, screen: &Screen) -> Vec<Vec<Cell>> {
        if self.help {
            return HELP
                .iter()
                .map(|line| window(&cells(line), 0, screen.cols - 1))
                .collect();
        }
        let doc = &self.documents[self.current];
        let width = doc.lines.len().to_string().len();
        let gutter = if self.options.line_numbers {
            width + 1
        } else {
            0
        };
        doc.lines
            .iter()
            .enumerate()
            .skip(doc.top)
            .take(screen.text_rows())
            .map(|(number, line)| {
                let mut line = line.clone();
                if !self.pattern.is_empty() {
                    for (start, end) in
                        matches(&doc.plain[number], &self.pattern, self.options.ignore_case)
                    {
                        highlight(&mut line, start, end);
                    }
                }
                let mut row = window(&line, doc.left, (screen.cols - 1).saturating_sub(gutter));
                if self.options.line_numbers {
                    let mut prefix = cells(&format!("{:>width$} ", number + 1, width = width));
                    prefix.append(&mut row);
                    row = prefix;
                }
                row
            })
            .collect()
    }

    fn draw(&self, screen: &Screen) {
        let rows = self.rows(screen);
        let out = stdout();
        let mut w = out.lock();
        let _ = write!(w, "\x1b[H");
        for row in 0..screen.text_rows() {
            let text = match rows.get(row) {
                Some(line) => render(line, None),
                // Past the end of the text, which less marks so that a short
                // file does not look like a screen of blank lines.
                None => "~".to_string(),
            };
            let _ = write!(w, "{text}\x1b[K\r\n");
        }
        let status = self.status(screen);
        let status: String = status.chars().take(screen.cols - 1).collect();
        // Written without a line feed: the terminal would scroll and take the
        // first line of text off the top.
        let _ = write!(w, "\x1b[7m{status}\x1b[0m\x1b[K");
        let _ = w.flush();
    }

    /// Read a line typed onto the status line, for the search prompt. Returns
    /// None when the user backs out of it with Escape or an empty backspace.
    fn prompt(&self, screen: &Screen, sigil: char, key_fd: u64) -> Option<String> {
        let mut text = String::new();
        loop {
            {
                let out = stdout();
                let mut w = out.lock();
                let _ = write!(w, "\x1b[{};1H{sigil}{text}\x1b[K", screen.rows);
                let _ = w.flush();
            }
            match read_key(key_fd)? {
                Key::Byte(b'\r') | Key::Byte(b'\n') => return Some(text),
                Key::Byte(0x7f) | Key::Byte(0x08) => {
                    text.pop()?;
                }
                Key::Escape | Key::Byte(0x03) => return None,
                Key::Byte(b) if b >= 0x20 => text.push(b as char),
                _ => {}
            }
        }
    }
}

/// Not a terminal: `less` is `cat`, which is what makes it usable in a
/// pipeline that someone else wrote.
fn dump(documents: &[Document]) {
    let out = stdout();
    let mut w = out.lock();
    for doc in documents {
        for line in &doc.lines {
            let _ = writeln!(w, "{}", render(line, None));
        }
    }
    let _ = w.flush();
}

fn restore() {
    let out = stdout();
    let mut w = out.lock();
    let _ = write!(w, "\r\x1b[K");
    let _ = w.flush();
}

fn main() {
    let (options, files) = parse_args();

    let mut documents = Vec::new();
    if files.is_empty() {
        documents.push(Document::new("(stdin)".to_string(), &read_stdin()));
    } else {
        for file in &files {
            match read_file(file) {
                Ok(text) => documents.push(Document::new(file.clone(), &text)),
                Err(e) => {
                    eprintln!("less: {e}");
                    exit(1);
                }
            }
        }
    }

    // The text may have arrived on stdin, in which case the keyboard is
    // whichever descriptor still points at the terminal.
    let key_fd = if isatty(0) {
        0
    } else if isatty(2) {
        2
    } else {
        u64::MAX
    };
    if !isatty(1) || key_fd == u64::MAX {
        dump(&documents);
        return;
    }

    let mut pager = Pager {
        documents,
        current: 0,
        options,
        pattern: String::new(),
        message: String::new(),
        help: false,
    };

    pty_set_raw(key_fd);
    loop {
        let screen = Screen::measure();
        pager.draw(&screen);
        pager.message.clear();

        let Some(key) = read_key(key_fd) else { break };
        let page = screen.text_rows() as isize;
        match key {
            Key::Byte(b'q') | Key::Byte(b'Q') | Key::Byte(0x03) => break,
            Key::Byte(b'j') | Key::Byte(b'\r') | Key::Byte(b'\n') | Key::Down => {
                pager.scroll(1, &screen)
            }
            Key::Byte(b'k') | Key::Up => pager.scroll(-1, &screen),
            Key::Byte(b' ') | Key::Byte(b'f') | Key::PageDown => pager.scroll(page, &screen),
            Key::Byte(b'b') | Key::PageUp => pager.scroll(-page, &screen),
            Key::Byte(b'd') => pager.scroll(page / 2, &screen),
            Key::Byte(b'u') => pager.scroll(-page / 2, &screen),
            Key::Byte(b'g') | Key::Home => pager.document().top = 0,
            Key::Byte(b'G') | Key::End => pager.scroll(isize::MAX, &screen),
            Key::Left => pager.shift(-(SHIFT_STEP as isize), &screen),
            Key::Right => pager.shift(SHIFT_STEP as isize, &screen),
            Key::Byte(b'h') => pager.help = !pager.help,
            Key::Byte(c @ (b'/' | b'?')) => {
                if let Some(pattern) = pager.prompt(&screen, c as char, key_fd) {
                    if !pattern.is_empty() {
                        pager.pattern = pattern;
                    }
                    if !pager.pattern.is_empty() && !pager.search(c == b'/') {
                        pager.message = format!("pattern not found: {}", pager.pattern);
                    }
                }
            }
            Key::Byte(b'n') | Key::Byte(b'N') => {
                if pager.pattern.is_empty() {
                    pager.message = "no previous search".to_string();
                } else {
                    let forward = matches!(key, Key::Byte(b'n'));
                    if !pager.search(forward) {
                        pager.message = format!("pattern not found: {}", pager.pattern);
                    }
                }
            }
            Key::Byte(b':') => match read_key(key_fd) {
                Some(Key::Byte(b'n')) if pager.current + 1 < pager.documents.len() => {
                    pager.current += 1;
                }
                Some(Key::Byte(b'p')) if pager.current > 0 => pager.current -= 1,
                Some(Key::Byte(b'n')) => pager.message = "no next file".to_string(),
                Some(Key::Byte(b'p')) => pager.message = "no previous file".to_string(),
                _ => {}
            },
            _ => {}
        }
    }

    pty_set_canonical(key_fd);
    restore();
}
