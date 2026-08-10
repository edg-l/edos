//! top - the thread table, re-read on a timer, sorted by what it costs.
//!
//! `ps` answers "what is running"; this answers "what is running *now*", which
//! is a different question because the kernel only publishes a monotonic
//! `CPUms` per thread. A share of the CPU is a rate, so it exists only between
//! two samples: every percentage here is the growth of that counter over the
//! interval just elapsed, and the first frame has nothing to subtract from and
//! reports zero.
//!
//! Interactive by default and batch when stdout is not a terminal, so
//! `top | head` and `top -b -n1 > file` both do the obvious thing instead of
//! writing cursor escapes into a pipe.

use std::collections::HashMap;
use std::io::{Write, stdout};
use std::process::exit;
use std::time::{Duration, Instant};

use edos_lib::io::{get_winsize, isatty, poll_stdin, pty_set_canonical, pty_set_raw, sys_read};
use edos_lib::procinfo::{Process, read_memory, read_table};
use edos_lib::time::local_time;

/// Refresh interval when `-d` is not given.
const DEFAULT_DELAY_MS: u64 = 1000;
/// Rows and columns assumed when nothing can answer, which is the batch case.
const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;
/// Rows the summary and the column header take, above the process rows.
const CHROME_ROWS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Cpu,
    Mem,
    Time,
    Pid,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Mem => "mem",
            Sort::Time => "time",
            Sort::Pid => "pid",
        }
    }
}

struct Options {
    delay_ms: u64,
    /// Frames to draw before exiting, or `None` to run until quit.
    iterations: Option<u64>,
    batch: bool,
    sort: Sort,
    /// Kernel threads are shown by default: on this system they are half of
    /// what the CPU is spent on, and hiding them makes the total look wrong.
    show_kernel: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            delay_ms: DEFAULT_DELAY_MS,
            iterations: None,
            batch: false,
            sort: Sort::Cpu,
            show_kernel: true,
        }
    }
}

/// One thread as this frame sees it: the table row plus the rate derived from
/// the previous frame.
struct Sample {
    process: Process,
    cpu_percent: f64,
}

fn usage() -> ! {
    eprintln!("usage: top [-d SECONDS] [-n COUNT] [-b] [-u] [-o cpu|mem|time|pid]");
    exit(2)
}

fn parse_args() -> Options {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // `-n1` and `-n 1` are both what people type, so a flag that takes a
        // value reads it from the rest of its own word first.
        let value = |flag: &str, args: &mut dyn Iterator<Item = String>| -> String {
            match arg.strip_prefix(flag) {
                Some("") | None => args.next().unwrap_or_else(|| usage()),
                Some(rest) => rest.to_string(),
            }
        };
        match arg.as_str() {
            a if a.starts_with("-d") => {
                let seconds: f64 = value("-d", &mut args).parse().unwrap_or_else(|_| usage());
                if !(seconds.is_finite() && seconds > 0.0) {
                    usage();
                }
                options.delay_ms = (seconds * 1000.0) as u64;
            }
            a if a.starts_with("-n") => {
                options.iterations =
                    Some(value("-n", &mut args).parse().unwrap_or_else(|_| usage()));
            }
            a if a.starts_with("-o") => {
                options.sort = match value("-o", &mut args).as_str() {
                    "cpu" => Sort::Cpu,
                    "mem" => Sort::Mem,
                    "time" => Sort::Time,
                    "pid" => Sort::Pid,
                    _ => usage(),
                };
            }
            "-b" => options.batch = true,
            "-u" => options.show_kernel = false,
            _ => usage(),
        }
    }
    options
}

/// Build this frame's rows, and replace `previous` with what the next frame
/// has to subtract from.
///
/// `elapsed_ms` is measured rather than assumed to be the delay: a slow refresh
/// or a keystroke that forces one early would otherwise scale every percentage
/// by the wrong divisor.
fn sample(
    processes: Vec<Process>,
    previous: &mut HashMap<u64, u64>,
    elapsed_ms: u64,
    first: bool,
) -> Vec<Sample> {
    let mut samples = Vec::with_capacity(processes.len());
    let mut current = HashMap::with_capacity(processes.len());
    for process in processes {
        // A pid the previous frame did not carry is one that has just been
        // spawned, and its whole CPU time was not necessarily spent inside this
        // interval, so it starts at zero like the first frame does.
        let before = previous.get(&process.pid).copied();
        let cpu_percent = match (first, before) {
            (false, Some(before)) if elapsed_ms > 0 => {
                let delta = process.cpu_ms.saturating_sub(before);
                (delta as f64 * 100.0 / elapsed_ms as f64).min(100.0)
            }
            _ => 0.0,
        };
        current.insert(process.pid, process.cpu_ms);
        samples.push(Sample {
            process,
            cpu_percent,
        });
    }
    *previous = current;
    samples
}

fn sort_samples(samples: &mut [Sample], sort: Sort) {
    match sort {
        // Ties on the rate are broken by total time, so a table of idle threads
        // is still ordered by something rather than by table order.
        Sort::Cpu => samples.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap()
                .then(b.process.cpu_ms.cmp(&a.process.cpu_ms))
        }),
        Sort::Mem => samples.sort_by(|a, b| {
            b.process
                .rss_kib
                .unwrap_or(0)
                .cmp(&a.process.rss_kib.unwrap_or(0))
        }),
        Sort::Time => samples.sort_by(|a, b| b.process.cpu_ms.cmp(&a.process.cpu_ms)),
        Sort::Pid => samples.sort_by_key(|s| s.process.pid),
    }
}

/// The summary lines: the clock, what the threads are doing, and memory.
fn summary(samples: &[Sample], sort: Sort) -> Vec<String> {
    // The kernel's states, bucketed the way a reader scans them: on a CPU,
    // waiting for one, waiting for something else, or held by a signal.
    let mut running = 0;
    let mut ready = 0;
    let mut sleeping = 0;
    let mut stopped = 0;
    for sample in samples {
        match sample.process.state.as_str() {
            "Running" => running += 1,
            "Ready" | "Waking" => ready += 1,
            "Stopped" => stopped += 1,
            "Sleeping" | "Parked" => sleeping += 1,
            _ => {}
        }
    }

    let clock = local_time()
        .map(|t| format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second))
        .unwrap_or_else(|| "--:--:--".to_string());

    let mut lines = vec![
        format!("top - {clock}  sort: {}", sort.label()),
        format!(
            "threads: {} total, {running} running, {ready} ready, {sleeping} sleeping, {stopped} stopped",
            samples.len()
        ),
    ];

    lines.push(match read_memory() {
        Ok(memory) => {
            let free = memory.total_kib.saturating_sub(memory.used_kib);
            let percent = if memory.total_kib > 0 {
                memory.used_kib as f64 * 100.0 / memory.total_kib as f64
            } else {
                0.0
            };
            format!(
                "memory: {} KiB total, {} KiB used ({percent:.1}%), {} KiB free",
                memory.total_kib, memory.used_kib, free
            )
        }
        Err(e) => format!("memory: unavailable ({e})"),
    });
    lines
}

const HEADER: &str = "  PID  PPID  PGID TYPE   STATE      CPU%    CPUms  RSSKiB NAME";

fn row(sample: &Sample) -> String {
    let process = &sample.process;
    let rss = process
        .rss_kib
        .map(|kib| kib.to_string())
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{:>5} {:>5} {:>5} {:<6} {:<9} {:>5.1} {:>8} {:>7} {}",
        process.pid,
        process.ppid,
        process.pgid,
        process.kind,
        process.state,
        sample.cpu_percent,
        process.cpu_ms,
        rss,
        process.name
    )
}

/// Clip `line` to fit a screen `cols` wide. Every glyph here is one column
/// wide, so truncating by character is truncating by column.
///
/// The last column is left empty: a line that fills the width exactly makes the
/// terminal wrap on its own, and the `\r\n` that follows then costs a second
/// line, so the table scrolls its own header off the top.
fn clip(line: &str, cols: usize) -> String {
    let cols = cols.saturating_sub(1);
    if line.chars().count() <= cols {
        return line.to_string();
    }
    line.chars().take(cols).collect()
}

fn draw(samples: &[Sample], options: &Options) {
    let (cols, rows) = if options.batch {
        (DEFAULT_COLS, usize::MAX)
    } else {
        get_winsize(1)
            .map(|(c, r)| (c as usize, r as usize))
            .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS))
    };

    let out = stdout();
    let mut w = out.lock();

    if options.batch {
        for line in summary(samples, options.sort) {
            let _ = writeln!(w, "{line}");
        }
        let _ = writeln!(w, "{HEADER}");
        for sample in samples {
            let _ = writeln!(w, "{}", row(sample));
        }
        let _ = writeln!(w);
        let _ = w.flush();
        return;
    }

    // Hide the cursor and redraw from the top left, clearing each line as it is
    // written rather than clearing the screen first: a full clear between
    // frames is what makes a terminal monitor flicker.
    let _ = write!(w, "\x1b[?25l\x1b[H");
    for line in summary(samples, options.sort) {
        let _ = write!(w, "{}\x1b[K\r\n", clip(&line, cols));
    }
    let _ = write!(w, "\x1b[7m{}\x1b[0m\x1b[K\r\n", clip(HEADER, cols));

    let visible = rows.saturating_sub(CHROME_ROWS);
    for sample in samples.iter().take(visible) {
        let _ = write!(w, "{}\x1b[K\r\n", clip(&row(sample), cols));
    }
    for _ in samples.len()..visible {
        let _ = write!(w, "\x1b[K\r\n");
    }
    let _ = write!(
        w,
        "\x1b[7m q quit  c cpu  m mem  t time  p pid  u user only  space refresh \x1b[0m\x1b[K"
    );
    let _ = w.flush();
}

/// Give the terminal back: canonical mode, cursor on, cursor off the status
/// line. Every exit from the interactive loop goes through this.
fn restore() {
    pty_set_canonical(0);
    let out = stdout();
    let mut w = out.lock();
    let _ = write!(w, "\x1b[?25h\r\n");
    let _ = w.flush();
}

/// Act on one keystroke. Returns false to quit.
fn handle_key(key: u8, options: &mut Options) -> bool {
    match key {
        b'q' | b'Q' | 0x03 => return false,
        b'c' => options.sort = Sort::Cpu,
        b'm' => options.sort = Sort::Mem,
        b't' => options.sort = Sort::Time,
        b'p' => options.sort = Sort::Pid,
        b'u' => options.show_kernel = !options.show_kernel,
        _ => {}
    }
    true
}

fn main() {
    let mut options = parse_args();
    // Cursor addressing into anything that is not a terminal is noise, so a
    // redirected or piped run is a batch run whether or not `-b` was given.
    if !isatty(1) {
        options.batch = true;
    }

    let mut previous: HashMap<u64, u64> = HashMap::new();
    let mut last = Instant::now();
    let mut first = true;
    let mut frames: u64 = 0;

    if !options.batch {
        pty_set_raw(0);
    }

    loop {
        let table = match read_table() {
            Ok(table) => table,
            Err(e) => {
                if !options.batch {
                    restore();
                }
                eprintln!("top: /proc/processes: {e}");
                exit(1);
            }
        };

        let now = Instant::now();
        let elapsed_ms = now.duration_since(last).as_millis() as u64;
        last = now;

        // The counter map is fed from the whole table, not the filtered view,
        // so toggling kernel threads off and back on does not lose the baseline
        // and report one huge frame.
        let mut samples = sample(table.processes, &mut previous, elapsed_ms, first);
        first = false;
        if !options.show_kernel {
            samples.retain(|s| !s.process.is_kernel());
        }
        sort_samples(&mut samples, options.sort);
        draw(&samples, &options);

        frames += 1;
        if options.iterations.is_some_and(|n| frames >= n) {
            break;
        }

        if options.batch {
            std::thread::sleep(Duration::from_millis(options.delay_ms));
            continue;
        }

        // Wait out the interval, but answer a keystroke as soon as it arrives.
        // A key that is not a command is not worth a redraw, so the wait
        // resumes with whatever is left of the interval.
        let deadline = now + Duration::from_millis(options.delay_ms);
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
            if !handle_key(buf[0], &mut options) {
                restore();
                return;
            }
            // A command redraws now rather than at the end of the interval.
            break;
        }
    }

    if !options.batch {
        restore();
    }
}
