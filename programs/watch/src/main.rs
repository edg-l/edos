//! watch - run a command on a timer and keep its output on the screen.
//!
//! The loop this replaces is `while true; do clear; cmd; sleep 2; done`, which
//! this system can write but which scrolls, flickers and cannot say what moved
//! between two frames. Here each frame is painted over the previous one line by
//! line, and `-d` reverse-videos the columns that changed, so a counter in
//! `/proc` that ticks once a minute is visible without reading numbers.
//!
//! The command runs through `sh -c` by default, so `watch 'ls /bin | wc -l'` is
//! a pipeline rather than four literal arguments; `-x` runs argv directly for
//! the case where the shell's own parsing is in the way.

use std::io::{Write, stdout};
use std::process::exit;
use std::time::{Duration, Instant};

use edos_lib::io::{get_winsize, isatty, poll_stdin, pty_set_canonical, pty_set_raw, sys_read};
use edos_lib::process::{close, pipe, read, spawn_program_with_fds, waitpid};
use edos_lib::time::local_time;

const DEFAULT_INTERVAL_MS: u64 = 2000;
/// Below this the command spends the whole interval starting up, and the
/// screen is unreadable anyway.
const MIN_INTERVAL_MS: u64 = 100;
const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;
/// A tab stops every eight columns, which is what the terminal assumes.
const TAB_WIDTH: usize = 8;

struct Options {
    interval_ms: u64,
    title: bool,
    differences: bool,
    /// Run argv directly instead of handing the joined line to `sh -c`.
    exec: bool,
    /// Stop on the first non-zero exit status instead of running forever.
    errexit: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            interval_ms: DEFAULT_INTERVAL_MS,
            title: true,
            differences: false,
            exec: false,
            errexit: false,
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: watch [-n secs] [-t] [-d] [-x] [-e] command [args...]");
    eprintln!("  -n, --interval SECS  seconds between runs (default 2, minimum 0.1)");
    eprintln!("  -t, --no-title       drop the header line");
    eprintln!("  -d, --differences    highlight what changed since the previous run");
    eprintln!("  -x, --exec           run the command directly, not through `sh -c`");
    eprintln!("  -e, --errexit        stop when the command exits non-zero");
    exit(1)
}

/// Seconds as written on the command line, in milliseconds. Fractional because
/// a half-second `watch` on `/proc/ahci_stats` is a real thing to want.
fn parse_interval(text: &str) -> Option<u64> {
    let (whole, fraction) = match text.split_once('.') {
        Some((w, f)) => (w, f),
        None => (text, ""),
    };
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut millis = whole.checked_mul(1000)?;
    for (i, c) in fraction.chars().take(3).enumerate() {
        let digit = c.to_digit(10)? as u64;
        millis += digit * [100, 10, 1][i];
    }
    if fraction.chars().any(|c| !c.is_ascii_digit()) {
        return None;
    }
    Some(millis.max(MIN_INTERVAL_MS))
}

/// Returns the options and the command words. Option parsing stops at the first
/// word that is not an option, so `watch ls -l` passes `-l` to `ls`.
fn parse_args() -> (Options, Vec<String>) {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    let mut command: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-n" | "--interval" => {
                let Some(value) = args.next() else { usage() };
                let Some(ms) = parse_interval(&value) else {
                    eprintln!("watch: not a number of seconds: {value}");
                    exit(1);
                };
                options.interval_ms = ms;
            }
            "-t" | "--no-title" => options.title = false,
            "-d" | "--differences" => options.differences = true,
            "-x" | "--exec" => options.exec = true,
            "-e" | "--errexit" => options.errexit = true,
            "-h" | "--help" => usage(),
            _ => {
                command.push(arg);
                command.extend(args);
                break;
            }
        }
    }

    if command.is_empty() {
        usage();
    }
    (options, command)
}

/// What one run of the command produced.
struct Run {
    lines: Vec<String>,
    status: i32,
}

/// Run the command once with both its streams on a pipe, and read the pipe dry
/// before waiting: a command whose output exceeds the pipe buffer would
/// otherwise block in `write` while this process blocks in `waitpid`.
fn run_command(command: &[String], exec: bool) -> Result<Run, String> {
    let (read_fd, write_fd) = pipe().ok_or_else(|| "pipe: out of descriptors".to_string())?;

    let (program, args) = if exec {
        (command[0].clone(), command[1..].to_vec())
    } else {
        ("sh".to_string(), vec!["-c".to_string(), command.join(" ")])
    };

    let Some(pid) = spawn_program_with_fds(&program, &args, 0, write_fd, write_fd) else {
        close(read_fd);
        close(write_fd);
        return Err(format!("{program}: command not found"));
    };
    // The child holds the only other write end now; without this close the read
    // below never sees EOF.
    close(write_fd);

    let mut output: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read(read_fd, &mut buf);
        if n <= 0 {
            break;
        }
        output.extend_from_slice(&buf[..n as usize]);
    }
    close(read_fd);

    let status = waitpid(pid);
    let text = String::from_utf8_lossy(&output).into_owned();
    let mut lines: Vec<String> = text.split('\n').map(|l| l.to_string()).collect();
    // A trailing newline ends the last line; it does not start an empty one.
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    Ok(Run { lines, status })
}

/// One column of a line: the character in it, and any escape sequences that
/// come immediately before it.
///
/// Commands here colour their own output — `ps` colours the state column, `ls`
/// the file types — so a line is not a sequence of columns until the escapes in
/// it are separated out. Counting them as columns clips a line short of the
/// screen width, and splitting one to insert a highlight prints its tail as
/// text.
struct Cell {
    escapes: String,
    ch: char,
}

/// Split a line into columns, expanding tabs to the next eight-column stop
/// while doing it, since the column a tab lands on is only known here.
fn cells(line: &str) -> Vec<Cell> {
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
                    std::mem::take(&mut pending)
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
fn clip(line: &str, cols: usize) -> Vec<Cell> {
    let mut cells = cells(line);
    cells.truncate(cols.saturating_sub(1));
    cells
}

/// Write a row out, optionally reverse-videoing every run of columns that
/// differs from `previous`. A row the previous frame did not have is left
/// alone: the whole line being new is obvious without marking every column.
///
/// Reverse video ends with SGR 27 rather than SGR 0 so that a highlight laid
/// over coloured output does not also switch the colour off.
fn render(cells: &[Cell], previous: Option<&Vec<Cell>>) -> String {
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

fn banner(command: &[String], interval_ms: u64) -> String {
    let seconds = interval_ms as f64 / 1000.0;
    format!("Every {seconds:.1}s: {}", command.join(" "))
}

fn timestamp() -> String {
    local_time()
        .map(|t| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                t.year, t.month, t.day, t.hour, t.minute, t.second
            )
        })
        .unwrap_or_default()
}

/// The header: what is being run and how often on the left, the wall clock on
/// the right, with the command elided from the middle if the two collide.
fn title(command: &[String], interval_ms: u64, cols: usize) -> String {
    let left = banner(command, interval_ms);
    let right = timestamp();

    let cols = cols.saturating_sub(1);
    if left.chars().count() + right.chars().count() + 1 > cols {
        let room = cols.saturating_sub(right.chars().count() + 1);
        let left: String = left.chars().take(room).collect();
        return format!("{left} {right}");
    }
    let gap = cols - left.chars().count() - right.chars().count();
    format!("{left}{}{right}", " ".repeat(gap))
}

/// The frame as clipped columns, which is both what gets written and what the
/// next frame diffs against.
fn frame(
    run: &Run,
    command: &[String],
    options: &Options,
    cols: usize,
    rows: usize,
) -> Vec<Vec<Cell>> {
    let mut lines = Vec::with_capacity(rows);
    if options.title {
        lines.push(clip(&title(command, options.interval_ms, cols), cols));
        lines.push(Vec::new());
    }
    for line in &run.lines {
        if lines.len() >= rows {
            break;
        }
        lines.push(clip(line, cols));
    }
    lines
}

fn draw(lines: &[Vec<Cell>], previous: &[Vec<Cell>], options: &Options, rows: usize) {
    let out = stdout();
    let mut w = out.lock();
    let _ = write!(w, "\x1b[H");
    for row in 0..rows {
        let text = match lines.get(row) {
            Some(line) if options.differences => render(line, previous.get(row)),
            Some(line) => render(line, None),
            None => String::new(),
        };
        // The last row is written without a line feed: the terminal would
        // scroll, taking the header with it.
        if row + 1 == rows {
            let _ = write!(w, "{text}\x1b[K");
        } else {
            let _ = write!(w, "{text}\x1b[K\r\n");
        }
    }
    let _ = w.flush();
}

fn restore() {
    pty_set_canonical(0);
    let out = stdout();
    let mut w = out.lock();
    let _ = write!(w, "\r\n");
    let _ = w.flush();
}

fn main() {
    let (options, command) = parse_args();
    // Cursor addressing into a pipe or a file is noise, so a redirected run
    // prints plain frames one after another instead.
    let interactive = isatty(1);
    if interactive {
        pty_set_raw(0);
    }

    let mut previous: Vec<Vec<Cell>> = Vec::new();
    loop {
        let started = Instant::now();
        let run = match run_command(&command, options.exec) {
            Ok(run) => run,
            Err(e) => {
                if interactive {
                    restore();
                }
                eprintln!("watch: {e}");
                exit(1);
            }
        };

        let (cols, rows) = if interactive {
            get_winsize(1)
                .map(|(c, r)| (c as usize, r as usize))
                .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS))
        } else {
            (DEFAULT_COLS, usize::MAX)
        };

        if interactive {
            let lines = frame(&run, &command, &options, cols, rows);
            draw(&lines, &previous, &options, rows);
            previous = lines;
        } else {
            let out = stdout();
            let mut w = out.lock();
            if options.title {
                // No screen to right-align the clock against, and nothing to
                // clip the command to, so the header is written flat.
                let _ = writeln!(
                    w,
                    "{}  {}",
                    banner(&command, options.interval_ms),
                    timestamp()
                );
                let _ = writeln!(w);
            }
            for line in &run.lines {
                let _ = writeln!(w, "{line}");
            }
            let _ = w.flush();
        }

        if options.errexit && run.status != 0 {
            if interactive {
                restore();
            }
            eprintln!("watch: command exited with status {}", run.status);
            exit(run.status);
        }

        let deadline = started + Duration::from_millis(options.interval_ms);
        if !interactive {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            continue;
        }

        // Wait out what is left of the interval, but answer a keystroke as soon
        // as it arrives: q quits, space runs the command again now.
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            if !poll_stdin(remaining.as_millis() as u64) {
                break;
            }
            let mut buf = [0u8; 1];
            if sys_read(0, &mut buf) <= 0 {
                restore();
                return;
            }
            match buf[0] {
                b'q' | b'Q' | 0x03 => {
                    restore();
                    return;
                }
                b' ' => break,
                _ => {}
            }
        }
    }
}
