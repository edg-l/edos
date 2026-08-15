//! Prices scheduling *latency*: how long a thread that becomes runnable waits
//! before it gets a CPU, and what that wait costs in switches and throughput.
//!
//! `switchbench` measures a handover once it has been decided on and
//! `balancebench` measures where work is placed. Neither can see the quantity
//! every remaining EEVDF knob trades in — a slice is a *request*, and what a
//! shorter one buys is a sooner turn at the price of more turns. This one
//! measures both ends of that trade at once, so a change to the slice or to the
//! wakeup rule can be argued from a number rather than from a story.
//!
//! **Run it on a single-CPU boot** (`scripts/edos-vm start --smp 1`). Latency
//! only exists where something is already using the CPU; a spare one serves the
//! waking thread immediately and every reading collapses to the wake path,
//! which is `balancebench wake`'s question and not this one's.
//!
//! Three modes:
//!
//! - default: the hogs at the kernel's own `BASE_SLICE`, the measuring thread
//!   at the same and then at the shortest slice it may ask for. The pair is the
//!   whole case for `sched_setattr`: a thread that wants to run sooner says so
//!   by asking for *less*, and pays for it in its own switches rather than in
//!   anyone else's throughput.
//! - `sweep`: the same measurement with every thread's slice set to each value
//!   in turn, which is the curve `BASE_SLICE` is chosen off.
//! - `clamp`: what the kernel does with a request outside the range it serves.
//!
//! `-l` mirrors the report to `/dev/klog`, which is how a headless run is read.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use edos_lib::io::Tee;
use edos_lib::process::{SchedAttr, sched_getattr, sched_setattr};

/// Online CPUs, or 0 if `/proc/cpuinfo` could not be read.
fn cpus_online() -> u64 {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("cpus online:"))
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// Context switches performed across every CPU, from `/proc/sched`.
///
/// Only the difference across a measurement means anything: it is a free
/// running total from boot, and the desktop behind this program contributes to
/// it the whole time.
fn switches_total() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/sched") else {
        return 0;
    };
    let mut header = text.lines();
    let Some(columns) = header.next() else {
        return 0;
    };
    let Some(column) = columns.split_whitespace().position(|c| c == "SWITCHES") else {
        return 0;
    };
    header
        .filter_map(|line| line.split_whitespace().nth(column))
        .filter_map(|value| value.parse::<u64>().ok())
        .sum()
}

/// Arithmetic the compiler cannot fold away or hoist out of the loop, so a
/// hog's cost is the loop and nothing else. Each round seeds the next: `work`
/// is pure, and `#[inline(never)]` does not stop LLVM lifting a pure call whose
/// arguments never change clean out of the loop.
#[inline(never)]
fn work(rounds: u64, seed: u64) -> u64 {
    let mut state = seed | 1;
    for i in 0..rounds {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(i | 1)
            .rotate_left(17);
    }
    state
}

/// Rounds per counted chunk. Short against the shortest slice on offer, so the
/// count has resolution inside a single turn and the stop flag is answered
/// promptly at the end of a window.
const HOG_CHUNK: u64 = 20_000;

/// How long the measuring thread sleeps between samples.
///
/// Longer than the longest slice under test, so that every sample starts from
/// a CPU that is genuinely busy with something else and the wake is a real
/// contest rather than a return to an idle machine.
const SAMPLE_SLEEP: Duration = Duration::from_millis(12);

/// How long a reading runs for.
///
/// A wall-clock window rather than a sample count, and that is not a detail:
/// a late sleep stretches the round it is in, so counting rounds makes a
/// reading with a long tail run for longer and hands its hogs more real time to
/// work in. Sized at 1.8 s, which is ~150 rounds when nothing is late — enough
/// samples that a p95 is the eighth worst rather than the third.
const WINDOW: Duration = Duration::from_millis(1800);

/// CPU-bound threads competing with the measuring thread.
const HOGS: u64 = 2;

/// A thread that never blocks, asking for `slice_ns` of service per turn.
fn hog(stop: Arc<AtomicBool>, chunks: Arc<AtomicU64>, slice_ns: u64, seed: u64) {
    set_slice(slice_ns);
    let mut state = seed;
    while !stop.load(Ordering::Relaxed) {
        state = work(HOG_CHUNK, state);
        chunks.fetch_add(1, Ordering::Relaxed);
    }
    // Keep the final state alive so the loop cannot be optimised to a counter.
    if state == 0 {
        println!("latbench: unreachable {state}");
    }
}

/// Ask for `slice_ns` per turn on this thread, keeping whatever priority it
/// has. A slice of 0 means "leave it alone".
fn set_slice(slice_ns: u64) {
    if slice_ns == 0 {
        return;
    }
    let Ok(mut attr) = sched_getattr(0) else {
        return;
    };
    attr.slice_ns = slice_ns;
    sched_setattr(0, &attr);
}

/// One reading: how late the measuring thread's sleeps returned, what the hogs
/// got through while it slept, and what the machine spent on switches.
struct Reading {
    delays: Vec<Duration>,
    chunks: u64,
    switches: u64,
    elapsed: Duration,
}

impl Reading {
    /// The delay at `percent` of the distribution, which is what a latency
    /// figure has to be reported as: a mean hides the tail this is looking for.
    fn at(&self, percent: usize) -> Duration {
        if self.delays.is_empty() {
            return Duration::ZERO;
        }
        let index = (self.delays.len() - 1) * percent / 100;
        self.delays[index]
    }

    /// Hog work per millisecond of the window, which is the throughput half of
    /// the trade and the only form of it that compares across readings.
    fn throughput(&self) -> f64 {
        self.chunks as f64 / self.elapsed.as_secs_f64().max(1e-9) / 1000.0
    }

    fn line(&self, label: &str) -> String {
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        format!(
            "latbench {label:<26} p50 {:>7.1} p95 {:>8.1} max {:>8.1} us | hog {:>6.1} chunks/ms | \
             switches {:>6} | {} samples",
            us(self.at(50)),
            us(self.at(95)),
            us(self.at(100)),
            self.throughput(),
            self.switches,
            self.delays.len(),
        )
    }
}

/// Run one reading: `hog_slice` on each of the [`HOGS`] competitors,
/// `measurer_slice` on the thread doing the sleeping. Zero means the default.
fn measure(hog_slice: u64, measurer_slice: u64) -> Reading {
    let stop = Arc::new(AtomicBool::new(false));
    let chunks = Arc::new(AtomicU64::new(0));
    let hogs: Vec<_> = (0..HOGS)
        .map(|i| {
            let stop = stop.clone();
            let chunks = chunks.clone();
            thread::spawn(move || hog(stop, chunks, hog_slice, i + 2))
        })
        .collect();

    // Let every hog reach its loop, and its requested slice take effect, before
    // the first sample: a hog still being spawned is not yet competition, and
    // the samples it is absent from are the fastest ones in the set.
    thread::sleep(Duration::from_millis(50));

    set_slice(measurer_slice);
    let switches_before = switches_total();
    let chunks_before = chunks.load(Ordering::Relaxed);

    let mut delays = Vec::new();
    let window = Instant::now();
    while window.elapsed() < WINDOW {
        let start = Instant::now();
        thread::sleep(SAMPLE_SLEEP);
        delays.push(start.elapsed().saturating_sub(SAMPLE_SLEEP));
    }
    let elapsed = window.elapsed();

    let chunks_after = chunks.load(Ordering::Relaxed);
    let switches_after = switches_total();
    stop.store(true, Ordering::Relaxed);
    for handle in hogs {
        let _ = handle.join();
    }
    // Back to the default, so the next reading starts where this one did.
    set_slice(BASE_SLICE_NS);

    delays.sort_unstable();
    Reading {
        delays,
        chunks: chunks_after - chunks_before,
        switches: switches_after - switches_before,
        elapsed,
    }
}

/// The kernel's `runqueue::BASE_SLICE`, restated here because a program cannot
/// read it. A mismatch shows up in the `clamp` mode, which reports what the
/// kernel actually granted.
const BASE_SLICE_NS: u64 = 1_000_000;

/// The slices the sweep prices, spanning the kernel's whole servable range.
const SWEEP_US: [u64; 6] = [250, 500, 1_000, 2_000, 4_000, 10_000];

fn main() {
    let mut klog = false;
    let mut mode = "default";
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-l" => klog = true,
            "sweep" | "clamp" => mode = Box::leak(arg.into_boxed_str()),
            _ => {
                eprintln!("usage: latbench [sweep|clamp] [-l]");
                std::process::exit(2);
            }
        }
    }
    let mut out = Tee::new(klog);

    let cpus = cpus_online();
    out.line(&format!(
        "latbench {cpus} cpus online, {HOGS} hogs, {} ms of {} ms sleeps per reading",
        WINDOW.as_millis(),
        SAMPLE_SLEEP.as_millis()
    ));
    if cpus > 1 {
        out.line(
            "latbench: NOT a single-CPU boot, a spare CPU serves the sleeper and there is no \
             latency to measure",
        );
    }

    match mode {
        "clamp" => clamp(&mut out),
        "sweep" => sweep(&mut out),
        _ => default(&mut out),
    }
}

/// The case for the slice being a per-thread request.
///
/// Both readings run against hogs at the kernel's own default, so the only
/// thing that changes between them is what the measuring thread asks for. A
/// shorter request is an earlier virtual deadline, which is how EEVDF is told
/// "run this sooner"; it should show up as a shorter tail here and as more
/// switches, and as no change at all in what the hogs got through.
fn default(out: &mut Tee) {
    let Ok(attr) = sched_getattr(0) else {
        out.line("latbench: sched_getattr failed, is this kernel new enough?");
        return;
    };
    out.line(&format!(
        "latbench this thread: priority {} slice {} us",
        attr.priority,
        attr.slice_ns / 1000
    ));

    let baseline = measure(0, 0);
    out.line(&baseline.line("sleeper at the default"));

    let short = measure(0, 250_000);
    out.line(&short.line("sleeper asking 250 us"));

    // The other end of the same dial, and the reading that prices a *wakeup*
    // rule rather than a slice. A sleeper asking for four times the hogs' slice
    // has a deadline well behind theirs, so it should not run when it wakes —
    // and an enqueue that requests a preemption anyway buys a save, a pick that
    // chooses the hog again, and a restore, twice per wake. Those switches are
    // the whole of what a deadline-aware wakeup check would save, so if this
    // column does not move when one is added, there was nothing there.
    let long = measure(0, 4_000_000);
    out.line(&long.line("sleeper asking 4 ms"));

    let p95 = |r: &Reading| r.at(95).as_secs_f64() * 1e6;
    out.line(&format!(
        "latbench asking for a quarter-slice moved the p95 wake from {:.1} to {:.1} us, \
         for {:+} switches and {:+.1}% of hog throughput",
        p95(&baseline),
        p95(&short),
        short.switches as i64 - baseline.switches as i64,
        (short.throughput() / baseline.throughput() - 1.0) * 100.0,
    ));
}

/// The curve `BASE_SLICE` is chosen off: every thread at the same slice, so
/// this is the machine-wide setting rather than one thread's request.
///
/// Read it as a trade, because that is what it is. A shorter slice can only
/// shorten the wait by taking more turns, and every turn is a switch plus the
/// cache it leaves behind — which the hog column, not the switch column, is the
/// honest measure of.
fn sweep(out: &mut Tee) {
    out.line("latbench sweep: every thread at the same slice");
    for slice_us in SWEEP_US {
        let reading = measure(slice_us * 1000, slice_us * 1000);
        out.line(&reading.line(&format!("slice {slice_us} us")));
    }
}

/// What the kernel does with a request outside the range it will serve.
///
/// A clamp rather than an error, so a program written against another
/// scheduler's range gets the nearest thing this one serves. The check is that
/// it says so: `sched_getattr` reports what was granted, not what was asked.
fn clamp(out: &mut Tee) {
    let Ok(original) = sched_getattr(0) else {
        out.line("latbench: sched_getattr failed");
        return;
    };
    for asked in [1u64, 100_000, BASE_SLICE_NS, 60_000_000_000] {
        let attr = SchedAttr {
            slice_ns: asked,
            ..original
        };
        assert!(
            sched_setattr(0, &attr) == 0,
            "latbench: sched_setattr({asked}) failed"
        );
        let granted = sched_getattr(0).expect("latbench: sched_getattr").slice_ns;
        out.line(&format!(
            "latbench clamp: asked {asked:>14} ns, granted {granted:>9} ns"
        ));
        assert!(
            granted > 0 && granted <= 10_000_000,
            "latbench: granted slice {granted} ns is outside what the kernel says it serves"
        );
    }
    assert!(
        sched_setattr(u64::MAX, &original) < 0,
        "latbench: sched_setattr on a thread that does not exist reported success"
    );
    sched_setattr(0, &original);
    out.line("latbench clamp: ok");
}
