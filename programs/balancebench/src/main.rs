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
//! `balancebench wake` prices the other half of placement. The straggler test
//! above spawns its workers, and a spawn already picks the least-loaded CPU; a
//! *wake* deliberately does not, because `complete_wake` enqueues on the waker's
//! CPU for cache locality. That mode parks a worker per spare CPU, lets the
//! machine go fully idle, and times a burst of wakes from one thread — which is
//! work-stealing's problem alone, and so measures how fast an idle CPU notices
//! there is something to steal.
//!
//! `balancebench sleep` prices the third case, which neither of the others can
//! reach: a thread that *sleeps* is placed by nobody. The sleepers heap is per
//! CPU, so a sleeper comes back out onto the CPU it slept on however busy that
//! CPU has become and however much of the machine is halted. That mode wakes a
//! burst the way `wake` does, has each worker sleep before it works, and asks
//! whether the concentration one round created is still there the next.
//!
//! `-l` mirrors the report to `/dev/klog`, which is how a headless run is read.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use edos_lib::io::Tee;
use edos_lib::process::{close, pipe, read, write};
use edos_lib::procinfo::cpus_online;

/// Online CPUs, or 0 if `/proc/cpuinfo` could not be read.

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

/// Which half of placement this run prices.
enum Mode {
    /// Straggler spread over freshly spawned workers.
    Spawn,
    /// Fan-out of a burst of wakes from one thread.
    Wake,
    /// Whether a thread that sleeps in a loop ever leaves the CPU it slept on.
    Sleep,
}

fn main() {
    let mut klog = false;
    let mut mode = Mode::Spawn;
    let mut workers_arg: Option<u64> = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-l" => klog = true,
            "wake" => mode = Mode::Wake,
            "sleep" => mode = Mode::Sleep,
            other => match other.parse::<u64>() {
                Ok(n) if n > 0 => workers_arg = Some(n),
                _ => {
                    eprintln!("usage: balancebench [wake|sleep] [workers] [-l]");
                    std::process::exit(2);
                }
            },
        }
    }
    let mut out = Tee::new(klog);

    match mode {
        Mode::Wake => return wake_burst(&mut out, workers_arg),
        Mode::Sleep => return sleep_burst(&mut out, workers_arg),
        Mode::Spawn => {}
    }

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

/// Rounds of the work loop for one woken worker, sized against the solo cost of
/// [`WORK_ROUNDS`] so the lump takes about one 5 ms timeslice. That size is the
/// point: a worker that gets a CPU of its own finishes inside its first slice,
/// so anything slower is time it spent waiting for a CPU rather than working.
const WAKE_ROUNDS: u64 = 3_300_000;

/// Burst wake-ups per round.
const WAKE_BURSTS: u32 = 10;

/// How long the machine is left alone before each burst, so every CPU reaches
/// the idle loop and halts. Without this the CPUs are still spinning down from
/// the previous burst and the wake lands on a machine that is not actually idle.
const WAKE_SETTLE: Duration = Duration::from_millis(60);

/// A worker that is woken by a byte, works for one timeslice, and answers.
///
/// The answer carries the microseconds the worker itself spent in the loop, so
/// the burst can be read from both ends: the waker's wall clock says how long
/// the fan-out took, and this says how much of that was arithmetic.
fn wake_worker(req_read: u64, ack_write: u64, rounds: u32, seed: u64) {
    let mut byte = [0u8; 1];
    // Each round seeds the next. `work` is a pure function of its arguments and
    // `#[inline(never)]` does not stop LLVM hoisting a pure call whose arguments
    // never change clean out of the loop: with a fixed seed the arithmetic ran
    // once, every round after it was a bare pipe round trip, and the burst
    // measured 0.02 ms against a 4 ms lump. The loop-carried dependency is what
    // makes each round do the work, and it has to stay.
    let mut state = seed;
    for _ in 0..rounds {
        if read(req_read, &mut byte) != 1 {
            return;
        }
        let t0 = Instant::now();
        state = work(WAKE_ROUNDS, state);
        let micros = t0.elapsed().as_micros() as u32;
        let _ = write(ack_write, &micros.to_le_bytes());
    }
}

/// Block until this worker answers, and refuse to report a time for a burst
/// that did not happen.
///
/// One ack pipe per worker rather than one shared queue: a shared queue is read
/// N times and cannot say *which* worker answered, so a worker that died, or one
/// that answered twice, still lets the burst look complete and fast. Here a
/// missing answer is a hang and a broken one is a panic, and neither can be
/// mistaken for a good measurement.
fn await_ack(ack_read: u64, worker: usize) -> u32 {
    let mut buf = [0u8; 4];
    let n = read(ack_read, &mut buf);
    assert!(
        n == 4,
        "balancebench wake: worker {worker} answered {n} bytes, not 4"
    );
    u32::from_le_bytes(buf)
}

/// Prices the *wake* path's placement, which the default mode cannot reach.
///
/// The default mode spawns its workers, and a spawn already goes to the
/// least-loaded CPU. A wake does not: `complete_wake` enqueues the woken thread
/// on the **waker's** CPU for cache locality, so a thread that wakes N workers
/// in a burst puts all N on one runqueue no matter how much of the machine is
/// idle. Getting them anywhere else is work-stealing's job, and an idle CPU only
/// looks for something to steal when its own backoff poll comes round.
///
/// So: park one worker per spare CPU, let the machine go fully idle, then wake
/// them all from one thread and time how long until the last one answers. Each
/// does one timeslice of arithmetic, which is what makes the fan-out necessary —
/// with no work in them, one CPU could serve the whole burst and a perfect score
/// would mean nothing. `wall / solo` is the report: 1.00 is every worker running
/// at once, N is the burst served one worker at a time.
fn wake_burst(out: &mut Tee, workers_arg: Option<u64>) {
    let cpus = cpus_online();
    let workers = workers_arg.unwrap_or_else(|| cpus.saturating_sub(1).max(1));
    out.line(&format!(
        "balancebench wake {cpus} cpus online, {workers} workers, {WAKE_BURSTS} bursts"
    ));
    if cpus < 2 {
        out.line("balancebench wake: NOT a multi-CPU boot, there is nowhere for a wake to go");
    }

    // What one worker's lump costs with a CPU to itself, measured the same way
    // the workers measure it so the ratio below compares like with like.
    //
    // The settle is part of "like with like" and not politeness: every burst is
    // timed after one, and this lump is short enough that the machine still
    // draining the report above it costs a measurable fraction. Timed straight
    // after the write it read 6.8 ms against the 4.2 ms the same arithmetic
    // takes in the default mode, which is a baseline that flatters every ratio
    // computed from it.
    thread::sleep(WAKE_SETTLE);
    let t0 = Instant::now();
    let checksum = work(WAKE_ROUNDS, 1);
    let solo = t0.elapsed();
    out.line(&format!(
        "balancebench wake solo {:.2} ms for one worker alone (checksum {checksum:#x})",
        solo.as_secs_f64() * 1000.0
    ));

    let mut req_writes = Vec::new();
    let mut ack_reads = Vec::new();
    for i in 0..workers {
        let (req_read, req_write) = pipe().expect("balancebench: request pipe");
        let (ack_read, ack_write) = pipe().expect("balancebench: ack pipe");
        req_writes.push(req_write);
        ack_reads.push(ack_read);
        thread::spawn(move || wake_worker(req_read, ack_write, WAKE_BURSTS, i + 2));
    }

    let mut walls: Vec<Duration> = Vec::new();
    for _ in 0..WAKE_BURSTS {
        // Every worker is parked on its read and every other CPU has run out of
        // work; this is the sleep that gets them all the way into `hlt`.
        thread::sleep(WAKE_SETTLE);

        let burst = Instant::now();
        for &req_write in &req_writes {
            let n = write(req_write, &[1u8]);
            assert!(n == 1, "balancebench wake: request write returned {n}");
        }
        let mut slowest_worker = 0u32;
        for (worker, &ack_read) in ack_reads.iter().enumerate() {
            slowest_worker = slowest_worker.max(await_ack(ack_read, worker));
        }
        let wall = burst.elapsed();
        out.line(&format!(
            "balancebench wake   burst {:.2} ms, slowest worker's own loop {:.2} ms",
            wall.as_secs_f64() * 1000.0,
            slowest_worker as f64 / 1000.0
        ));
        walls.push(wall);
    }
    walls.sort_unstable();

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let median = walls[walls.len() / 2];
    out.line(&format!(
        "balancebench wake burst fastest {:.2} ms median {:.2} ms slowest {:.2} ms",
        ms(walls[0]),
        ms(median),
        ms(walls[walls.len() - 1])
    ));
    out.line(&format!(
        "balancebench wake fanout {:.2} (median burst over solo; 1.00 is every worker at once, \
         {workers}.00 is one at a time)",
        ms(median) / ms(solo)
    ));
    report_sched(out, "after the bursts");

    for req_write in req_writes {
        let _ = close(req_write);
    }
    for ack_read in ack_reads {
        let _ = close(ack_read);
    }
}

/// How long each worker sleeps after the wake that placed it, before it works.
///
/// Long enough that every worker has reached the sleepers heap of the CPU the
/// burst put it on before the earliest deadline expires, and short enough that
/// a round is still dominated by the arithmetic rather than by the wait.
const SLEEP_DELAY: Duration = Duration::from_millis(20);

/// A worker that is woken by a byte, sleeps, works for one timeslice, answers.
///
/// The sleep is the whole point: it re-enters the sleepers heap of whichever
/// CPU the wake happened to place this thread on, and a sleeper comes back out
/// onto that same CPU. So a burst that buried every worker in one runqueue is
/// not undone by the next round — it is repeated, unless something notices the
/// expiry and lets an idle CPU take one.
fn sleep_worker(req_read: u64, ack_write: u64, rounds: u32, seed: u64) {
    let mut byte = [0u8; 1];
    // Loop-carried, for the reason spelled out in `wake_worker`.
    let mut state = seed;
    for _ in 0..rounds {
        if read(req_read, &mut byte) != 1 {
            return;
        }
        thread::sleep(SLEEP_DELAY);
        let t0 = Instant::now();
        state = work(WAKE_ROUNDS, state);
        let micros = t0.elapsed().as_micros() as u32;
        let _ = write(ack_write, &micros.to_le_bytes());
    }
}

/// Prices where a *sleeper* runs, which neither other mode can reach.
///
/// `wake` measures one burst landing on one runqueue and asks how fast the rest
/// of the machine comes to take it. This asks the question the round after:
/// each worker, once woken, sleeps before it works. A park is placed by its
/// waker and so follows the work; a sleep is placed by nothing at all, since
/// the sleepers heap is per CPU and hands the thread straight back to the CPU
/// it slept on. Left alone, one round's concentration becomes permanent.
///
/// The report is `(wall - sleep) / solo` on the same scale as `wake fanout`:
/// 1.00 is every worker working at once, N is the machine serving them one at
/// a time from a single runqueue while the rest of it is halted.
fn sleep_burst(out: &mut Tee, workers_arg: Option<u64>) {
    let cpus = cpus_online();
    let workers = workers_arg.unwrap_or_else(|| cpus.saturating_sub(1).max(1));
    out.line(&format!(
        "balancebench sleep {cpus} cpus online, {workers} workers, {WAKE_BURSTS} bursts"
    ));
    if cpus < 2 {
        out.line("balancebench sleep: NOT a multi-CPU boot, there is nowhere for a sleeper to go");
    }

    thread::sleep(WAKE_SETTLE);
    let t0 = Instant::now();
    let checksum = work(WAKE_ROUNDS, 1);
    let solo = t0.elapsed();
    out.line(&format!(
        "balancebench sleep solo {:.2} ms for one worker alone (checksum {checksum:#x})",
        solo.as_secs_f64() * 1000.0
    ));

    let mut req_writes = Vec::new();
    let mut ack_reads = Vec::new();
    for i in 0..workers {
        let (req_read, req_write) = pipe().expect("balancebench: request pipe");
        let (ack_read, ack_write) = pipe().expect("balancebench: ack pipe");
        req_writes.push(req_write);
        ack_reads.push(ack_read);
        thread::spawn(move || sleep_worker(req_read, ack_write, WAKE_BURSTS, i + 2));
    }

    let mut walls: Vec<Duration> = Vec::new();
    for _ in 0..WAKE_BURSTS {
        thread::sleep(WAKE_SETTLE);

        let burst = Instant::now();
        for &req_write in &req_writes {
            let n = write(req_write, &[1u8]);
            assert!(n == 1, "balancebench sleep: request write returned {n}");
        }
        let mut slowest_worker = 0u32;
        for (worker, &ack_read) in ack_reads.iter().enumerate() {
            slowest_worker = slowest_worker.max(await_ack(ack_read, worker));
        }
        // Every worker waited the same fixed sleep, and it is not placement.
        let wall = burst.elapsed().saturating_sub(SLEEP_DELAY);
        out.line(&format!(
            "balancebench sleep   burst {:.2} ms, slowest worker's own loop {:.2} ms",
            wall.as_secs_f64() * 1000.0,
            slowest_worker as f64 / 1000.0
        ));
        walls.push(wall);
    }
    walls.sort_unstable();

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let median = walls[walls.len() / 2];
    out.line(&format!(
        "balancebench sleep burst fastest {:.2} ms median {:.2} ms slowest {:.2} ms",
        ms(walls[0]),
        ms(median),
        ms(walls[walls.len() - 1])
    ));
    out.line(&format!(
        "balancebench sleep fanout {:.2} (median burst less the sleep, over solo; 1.00 is every \
         worker at once, {workers}.00 is one at a time)",
        ms(median) / ms(solo)
    ));
    report_sched(out, "after the bursts");

    for req_write in req_writes {
        let _ = close(req_write);
    }
    for ack_read in ack_reads {
        let _ = close(ack_read);
    }
}

/// The kernel's own view of what it thinks each CPU is carrying.
///
/// Printed beside the timings because the two answer the same question from
/// opposite ends: `/proc/sched` is what placement believed, the straggler
/// spread is what it cost.
fn report_sched(out: &mut Tee, when: &str) {
    let Ok(text) = std::fs::read_to_string("/proc/sched") else {
        return;
    };
    out.line(&format!("balancebench /proc/sched {when}:"));
    for line in text.lines() {
        out.line(&format!("balancebench   {line}"));
    }
}
