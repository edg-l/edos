//! Terminal rendering benchmark.
//!
//! Answers one question the frame telemetry in `edos-wm` cannot: how much of a
//! terminal's frame is spent turning cells into pixels, as opposed to getting
//! those pixels to the display. It needs no window and no compositor, so the
//! number it reports is the client's own cost and nothing else.
//!
//! The report also goes to `/dev/klog`, so a headless run leaves it in the
//! host's serial capture.

use std::fmt::Write as _;
use std::time::Instant;

use edos_render::surface::Surface;
use edos_render::widgets::{Terminal, Widget};

/// The terminal's default window size, so the numbers describe the real thing.
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;

/// Iterations per case. Enough that the glyph cache is warm for all but the
/// first, and short enough that the whole run is a few seconds.
const ITERS: usize = 60;

/// Representative content: a shell session, not a wall of one repeated glyph.
/// Cell coverage decides the cost, and a screen of `x` and a screen of real
/// output do not have the same number of non-space cells.
const SAMPLE: &[&str] = &[
    "$ ls -l /bin",
    "-rwxr-xr-x  1 root root   142336 edos-wm",
    "-rwxr-xr-x  1 root root    98112 edos-terminal",
    "-rwxr-xr-x  1 root root    76544 edos-taskbar",
    "$ cat /proc/meminfo",
    "MemTotal:       2097152 kB",
    "MemFree:        1783296 kB",
    "Cached:          212992 kB",
    "$ grep -n fn programs/edos_render/src/window.rs | head",
    "520:    pub fn swap_buffers(&mut self) {",
    "561:    pub fn resize(&mut self, w: u32, h: u32) -> Result<(), i64> {",
];

fn main() {
    let mut out = String::new();
    let _ = writeln!(out, "termbench {}x{}, {} iterations", WIDTH, HEIGHT, ITERS);

    let mut buffer = vec![0u32; (WIDTH * HEIGHT) as usize];

    // An empty screen: the floor, and the part of a draw that has nothing to do
    // with how much text is on it.
    let empty = new_terminal();
    let _ = writeln!(out, "grid       {}x{} cells", empty.cols(), empty.rows());
    bench_draw(&mut out, "empty", &mut buffer, &empty);

    // A full screen, redrawn unchanged. This is what a terminal holding a
    // screen of output costs per frame.
    let full = filled_terminal();
    bench_draw(&mut out, "full", &mut buffer, &full);

    // Every cell carrying a glyph. Real output leaves a third of the grid
    // blank and a blank cell costs nothing to rasterise, so this is the
    // ceiling the realistic case sits under.
    let dense = dense_terminal();
    bench_draw(&mut out, "dense", &mut buffer, &dense);

    // One new line at the bottom, which scrolls everything up by a row. Every
    // pixel on screen changes, and today every glyph is rasterised again.
    let mut term = filled_terminal();
    let mut samples = Vec::with_capacity(ITERS);
    let start = Instant::now();
    for i in 0..ITERS {
        let t = Instant::now();
        term.write_str(SAMPLE[i % SAMPLE.len()]);
        term.write_str("\n");
        term.draw(&mut Surface::new(&mut buffer, WIDTH, HEIGHT));
        samples.push(t.elapsed().as_micros() as u64);
    }
    report(
        &mut out,
        "scroll",
        &mut samples,
        start.elapsed().as_micros() as u64,
    );

    // The same workloads through the incremental path, which repaints only the
    // rows that differ from what the buffer being drawn already holds. `slot`
    // alternates because the window alternates its two shm buffers, and a
    // repaint that is only correct on one of them is the trap this path exists
    // to avoid.
    let mut term = filled_terminal();
    let mut slot = 0usize;
    let warm = |term: &mut Terminal, buffer: &mut [u32], slot: &mut usize| {
        term.draw_changed(*slot, &mut Surface::new(buffer, WIDTH, HEIGHT));
        *slot ^= 1;
        term.draw_changed(*slot, &mut Surface::new(buffer, WIDTH, HEIGHT));
        *slot ^= 1;
    };
    warm(&mut term, &mut buffer, &mut slot);

    bench_changed(
        &mut out,
        "idle+",
        &mut buffer,
        &mut term,
        &mut slot,
        |_, _| {},
    );
    bench_changed(
        &mut out,
        "type+",
        &mut buffer,
        &mut term,
        &mut slot,
        |t, _| t.write_str("x"),
    );
    // A line of text, not a bare newline: repeated newlines blank the screen
    // within one screenful and every scroll after that is free, which reports
    // a scroll as costing nothing.
    bench_changed(
        &mut out,
        "scroll+",
        &mut buffer,
        &mut term,
        &mut slot,
        |t, i| {
            t.write_str(SAMPLE[i % SAMPLE.len()]);
            t.write_str("\n");
        },
    );

    let _ = writeln!(
        out,
        "16ms is the frame budget. `full` and `scroll` near it means the client\n\
         is the bottleneck; well under it means the cost is elsewhere. The `+`\n\
         rows are the same work through the incremental path."
    );

    print!("{}", out);
    edos_lib::io::klog_dump("termbench:", out.lines());
}

fn new_terminal() -> Terminal {
    let mut term = Terminal::with_size(0, 0, WIDTH, HEIGHT);
    term.set_focused(true);
    term
}

/// A terminal with every row carrying text, built the way the real one fills:
/// through `write_str`, so the cells hold what a shell session would leave.
///
/// One extra line, because the last `\n` scrolls the first off the top and a
/// screen with a blank row is not the case being measured.
fn filled_terminal() -> Terminal {
    let mut term = new_terminal();
    for row in 0..=term.rows() {
        term.write_str(SAMPLE[row % SAMPLE.len()]);
        term.write_str("\n");
    }
    term
}

/// Every cell of every row carrying a glyph: the rasteriser's worst case.
fn dense_terminal() -> Terminal {
    let mut term = new_terminal();
    let cols = term.cols();
    for row in 0..=term.rows() {
        let source = SAMPLE[row % SAMPLE.len()];
        let line: String = source
            .chars()
            .filter(|c| *c != ' ')
            .cycle()
            .take(cols)
            .collect();
        term.write_str(&line);
        term.write_str("\n");
    }
    term
}

fn bench_draw(out: &mut String, name: &str, buffer: &mut [u32], term: &Terminal) {
    let mut samples = Vec::with_capacity(ITERS);
    let start = Instant::now();
    for _ in 0..ITERS {
        let t = Instant::now();
        term.draw(&mut Surface::new(buffer, WIDTH, HEIGHT));
        samples.push(t.elapsed().as_micros() as u64);
    }
    report(out, name, &mut samples, start.elapsed().as_micros() as u64);
}

fn bench_changed(
    out: &mut String,
    name: &str,
    buffer: &mut [u32],
    term: &mut Terminal,
    slot: &mut usize,
    mut mutate: impl FnMut(&mut Terminal, usize),
) {
    let mut samples = Vec::with_capacity(ITERS);
    let start = Instant::now();
    for i in 0..ITERS {
        let t = Instant::now();
        mutate(term, i);
        term.draw_changed(*slot, &mut Surface::new(buffer, WIDTH, HEIGHT));
        samples.push(t.elapsed().as_micros() as u64);
        *slot ^= 1;
    }
    report(out, name, &mut samples, start.elapsed().as_micros() as u64);
}

fn report(out: &mut String, name: &str, samples: &mut [u64], total_us: u64) {
    samples.sort_unstable();
    let n = samples.len();
    let _ = writeln!(
        out,
        "{:<8} p50 {:>7}us  p95 {:>7}us  max {:>7}us  mean {:>7}us",
        name,
        samples[n / 2],
        samples[n * 95 / 100],
        samples[n - 1],
        total_us / n as u64,
    );
}
