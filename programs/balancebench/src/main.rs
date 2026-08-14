//! Prices *placement* rather than the switch: how evenly the scheduler spreads
//! equal, runnable threads across the CPUs it has.
//!
//! `switchbench` cannot answer this and says so — it wants a single-CPU boot,
//! because a second CPU makes its handovers disappear. This one is the
//! opposite: **run it under `make run` or `make run-big`**, and a single-CPU
//! boot makes it meaningless.
//!
//! The measurement is a straggler test. One worker alone establishes what a
//! fixed lump of arithmetic costs with a CPU to itself. Then one worker per
//! CPU runs the same lump at the same time. With ideal placement each still
//! finishes in about the solo time, because each has a CPU; with two workers
//! sharing a CPU while another sits idle, the slowest takes twice as long. So
//! `slowest / solo` is the number: 1.00 is perfect, 2.00 means half the
//! machine went unused.
//!
//! What makes it a scheduler measurement and not an arithmetic one is the
//! ballast: `BLOCKED_PER_CPU` threads per CPU that block on a pipe nobody
//! writes to. They are threads the system is carrying and will never run, and
//! placement must not see them as work — a CPU is not busy because something
//! is asleep on it. Each does a wake round trip first, so its home CPU is set
//! by where its waker ran rather than by where it was spawned, which is how a
//! long-lived system's threads actually end up distributed.
//!
//! `-l` mirrors the report to `/dev/klog`, which is how a headless run is read.

use std::io::Write;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use edos_lib::process::{close, pipe, read, write};

/// Report sink: stdout, and optionally the kernel log as well.
struct Out {
    klog: Option<std::fs::File>,
}

impl Out {
    fn new(enabled: bool) -> Self {
        Self {
            klog: enabled
                .then(|| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/klog")
                        .ok()
                })
                .flatten(),
        }
    }

    fn line(&mut self, text: &str) {
        println!("{text}");
        if let Some(klog) = &mut self.klog {
            let _ = writeln!(klog, "{text}");
        }
    }
}

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

/// Blocked threads created per CPU before the workers run.
const BLOCKED_PER_CPU: u64 = 8;

/// Rounds of the work loop. Sized so one worker alone takes about 180 ms: long
/// enough that a 5 ms timeslice is small against it, so a single unlucky
/// preemption cannot move the result, and short enough that the whole run stays
/// well under a minute.
const WORK_ROUNDS: u64 = 120_000_000;

/// Arithmetic the compiler cannot fold away or hoist out of the loop, so a
/// worker's cost is the loop and nothing else.
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

/// A thread that blocks forever on a pipe with no writer but this process.
///
/// It reports its readiness through a second pipe first, so the main thread
/// knows when the ballast is in place, and that round trip is also what gives
/// the thread a wake history: it is enqueued where its waker ran.
fn blocked_thread(idle_read: u64, ack_write: u64) {
    let mut byte = [0u8; 1];
    let _ = write(ack_write, &[1u8]);
    // Blocks until the process exits: nothing ever writes to this pipe.
    let _ = read(idle_read, &mut byte);
}

fn main() {
    let mut klog = false;
    let mut workers_arg: Option<u64> = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-l" => klog = true,
            other => match other.parse::<u64>() {
                Ok(n) if n > 0 => workers_arg = Some(n),
                _ => {
                    eprintln!("usage: balancebench [workers] [-l]");
                    std::process::exit(2);
                }
            },
        }
    }
    let mut out = Out::new(klog);

    let cpus = cpus_online();
    // One fewer worker than there are CPUs, because the machine is not empty:
    // the compositor wakes about 77 times a second, the panel once, and this
    // thread is itself runnable while it waits. Asking for a worker per CPU
    // measures that oversubscription rather than placement, and a spare CPU is
    // also what makes a bad placement visible — two workers sharing a CPU while
    // one stands empty is exactly the failure this is looking for.
    let workers = workers_arg.unwrap_or_else(|| cpus.saturating_sub(1).max(1));
    let blocked = BLOCKED_PER_CPU * cpus.max(1);
    out.line(&format!(
        "balancebench {cpus} cpus online, {workers} workers, {blocked} blocked threads"
    ));
    if cpus < 2 {
        out.line("balancebench: NOT a multi-CPU boot, there is no placement to measure");
    }

    // Solo: what the lump costs with a CPU to itself.
    let t0 = Instant::now();
    let checksum = work(WORK_ROUNDS, 1);
    let solo = t0.elapsed();
    out.line(&format!(
        "balancebench solo {:.1} ms for one worker alone (checksum {checksum:#x})",
        solo.as_secs_f64() * 1000.0
    ));

    // Ballast: threads the system carries but will never run.
    let (ack_read, ack_write) = pipe().expect("balancebench: ack pipe");
    for _ in 0..blocked {
        let (idle_read, _idle_write) = pipe().expect("balancebench: idle pipe");
        thread::spawn(move || blocked_thread(idle_read, ack_write));
    }
    for _ in 0..blocked {
        let mut byte = [0u8; 1];
        let _ = read(ack_read, &mut byte);
    }
    // Let the last of them reach the blocking read rather than merely having
    // published readiness before it.
    thread::sleep(Duration::from_millis(50));
    report_sched(&mut out, "with the ballast blocked");

    // The measurement: one lump per worker, all at once.
    let (done_tx, done_rx) = mpsc::channel();
    let wall = Instant::now();
    let handles: Vec<_> = (0..workers)
        .map(|i| {
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                let t0 = Instant::now();
                let checksum = work(WORK_ROUNDS, i + 2);
                let _ = done_tx.send(t0.elapsed());
                checksum
            })
        })
        .collect();
    drop(done_tx);
    let mut times: Vec<Duration> = done_rx.iter().collect();
    for handle in handles {
        let _ = handle.join();
    }
    let wall = wall.elapsed();
    times.sort_unstable();

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let fastest = times.first().copied().unwrap_or_default();
    let median = times[times.len() / 2];
    let slowest = times.last().copied().unwrap_or_default();
    out.line(&format!(
        "balancebench workers fastest {:.1} ms median {:.1} ms slowest {:.1} ms wall {:.1} ms",
        ms(fastest),
        ms(median),
        ms(slowest),
        ms(wall)
    ));
    out.line(&format!(
        "balancebench imbalance {:.2} (slowest over solo; 1.00 is one worker per cpu)",
        ms(slowest) / ms(solo)
    ));

    let _ = close(ack_read);
    let _ = close(ack_write);
}

/// The kernel's own view of what it thinks each CPU is carrying.
///
/// Printed beside the timings because the two answer the same question from
/// opposite ends: `/proc/sched` is what placement believed, the straggler
/// spread is what it cost.
fn report_sched(out: &mut Out, when: &str) {
    let Ok(text) = std::fs::read_to_string("/proc/sched") else {
        return;
    };
    out.line(&format!("balancebench /proc/sched {when}:"));
    for line in text.lines() {
        out.line(&format!("balancebench   {line}"));
    }
}
