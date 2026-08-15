//! Prices a context switch, split by what each shape of switch has to do.
//!
//! `sched_yield` is the whole benchmark's lever because it is the shortest
//! path into the scheduler: no wait queue, no readiness to evaluate, nothing
//! but the switch itself. What varies between the cases is who the scheduler
//! finds to run next.
//!
//! - **idle** has nothing else Ready, so the scheduler picks the caller back
//!   up. Every step still runs -- the context is saved and restored, the FPU
//!   area is written and read back, the timer is re-armed -- so this is the
//!   bookkeeping floor rather than a free call.
//! - **thread** yields to a sibling of the same process. A real handover, but
//!   both threads share an address space, so `CR3` is never reloaded.
//! - **process** yields to a forked child. The same handover with an address
//!   space switch on top, which is what a TLB flush costs.
//! - **pipe** blocks instead of yielding, so it prices the park/wake path a
//!   real IPC round trip takes rather than the voluntary one.
//!
//! **Run this under `make run-single`.** Every case above needs the two
//! runnable threads to be on ONE CPU; give the scheduler a second CPU and it
//! puts them on both, where neither ever waits for the other and the yield
//! cases silently collapse back into the idle case. The report says which CPU
//! count it saw so a number taken the wrong way is recognisable later.
//!
//! `-l` mirrors the report to `/dev/klog`, which is how a headless run is read.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use edos_lib::io::Tee;
use edos_lib::process::{close, fork, pipe, read, sched_yield, write};

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

/// Batches a short measurement runs, of the `iters` it was given each.
///
/// Six, not more: the whole run costs about 0.45 s at the default iteration
/// count and every batch past the first few buys less, since one clean batch
/// is all the minimum needs. Raise it only for a number that has to settle a
/// question, and remember the build-and-boot cycle around this is minutes.
const BATCHES: u64 = 6;

/// Nanoseconds per operation, taken as the **best** batch rather than the mean
/// of one.
///
/// Everything measured here is a few hundred nanoseconds, and two things
/// interrupt it. Inside the guest, the desktop: the compositor wakes about 74
/// times a second and the panel clock once a second. Outside it, the **host**,
/// which deschedules the whole vCPU whenever it has something else to run --
/// a build running on the host machine is invisible from in here and looks
/// exactly like slow code.
///
/// A mean therefore reports the machine's mood rather than the code, and it
/// did: the same binary in one boot gave 243, 302 and 96 ns for `getpid`, and
/// 530, 1103 and 2090 for the pipe echo. Interference only ever *adds* time,
/// so the fastest batch is the one least contaminated by it, and it is stable
/// where the mean is not.
///
/// This is why the numbers here are lower than earlier records of the same
/// quantities: those were means of a single batch.
fn best_ns(iters: u64, mut op: impl FnMut()) -> f64 {
    for _ in 0..64 {
        op();
    }
    let mut best = f64::MAX;
    for _ in 0..BATCHES {
        let t0 = Instant::now();
        for _ in 0..iters {
            op();
        }
        let per = t0.elapsed().as_nanos() as f64 / iters as f64;
        if per < best {
            best = per;
        }
    }
    best
}

/// Nanoseconds per `sched_yield`.
fn time_yield(iters: u64) -> f64 {
    best_ns(iters, sched_yield)
}

/// A partner yielding in a loop keeps exactly one other thread Ready, so every
/// yield the measuring side makes is answered by a handover back.
fn yield_until(stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        sched_yield();
    }
}

fn main() {
    let mut iters: u64 = 20_000;
    let mut klog = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-l" => klog = true,
            other => match other.parse::<u64>() {
                Ok(n) if n > 0 => iters = n,
                _ => {
                    eprintln!("usage: switchbench [iterations] [-l]");
                    std::process::exit(2);
                }
            },
        }
    }
    let mut out = Tee::new(klog);

    let cpus = cpus_online();
    out.line(&format!("switchbench {iters} iters, {cpus} cpus online"));
    if cpus != 1 {
        out.line("switchbench: NOT a single-CPU boot, the handover cases are meaningless");
    }

    // Nothing else is Ready, so the scheduler hands the caller back to itself.
    let idle = time_yield(iters);
    out.line(&format!("switchbench yield idle {idle:.0} ns"));

    // One partner means two switches and two syscalls per iteration: this
    // thread into the partner, the partner back into this one.
    let stop = Arc::new(AtomicBool::new(false));
    let partner = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || yield_until(&stop))
    };
    let pair = time_yield(iters) / 2.0;
    stop.store(true, Ordering::Relaxed);
    let _ = partner.join();
    out.line(&format!(
        "switchbench yield thread {pair:.0} ns per handover, {:.0} ns over idle",
        pair - idle
    ));

    // The same handover with an address space switch on top.
    let child = fork();
    if child == 0 {
        loop {
            sched_yield();
        }
    }
    if child < 0 {
        out.line("switchbench: fork failed, skipping the cross-process case");
    } else {
        let cross = time_yield(iters) / 2.0;
        edos_lib::process::kill(child as u64, edos_lib::process::SIGKILL);
        edos_lib::process::waitpid(child as u64);
        out.line(&format!(
            "switchbench yield process {cross:.0} ns per handover, {:.0} ns over a shared \
             address space",
            cross - pair
        ));
    }

    // A blocking round trip: each side parks in `read` and is woken by the
    // other's `write`, so this prices the wake path the yield cases never take.
    // Both round trips at two working-set sizes. The cross-process case pays
    // for refills the thread case does not, so the difference of the two
    // differences is what one user page costs after a `CR3` reload -- which is
    // the number that says whether larger pages are worth having.
    for touch in [0, TOUCH_PAGES] {
        pipe_round_trip(&mut out, iters.min(2_000), touch);
        pipe_round_trip_threads(&mut out, iters.min(2_000), touch);
    }

    syscall_floor(&mut out, iters);
    pipe_no_block(&mut out, iters.min(5_000));

    sleep_overshoot(&mut out);
}

/// What a syscall costs before it does anything.
///
/// `getpid` is the shortest call the kernel has: dispatch, and a read of the
/// current thread. A read of a descriptor that does not exist adds the fd
/// table lookup and the error return, so the difference between the two is
/// what every call that touches a descriptor pays before reaching its own
/// work. Both are the floor under `pipe echo` below.
fn syscall_floor(out: &mut Tee, iters: u64) {
    let getpid = best_ns(iters, || {
        edos_lib::process::getpid();
    });
    out.line(&format!("switchbench getpid {getpid:.0} ns"));

    let mut byte = [0u8; 1];
    let badfd = best_ns(iters, || {
        read(9999, &mut byte);
    });
    out.line(&format!(
        "switchbench read of a bad fd {badfd:.0} ns, so the fd table costs {:.0} ns",
        badfd - getpid
    ));
}

/// A pipe write and read where the data is already there, so nothing blocks
/// and nothing is scheduled.
///
/// This is what separates the pipe from the scheduler. The blocking round trip
/// above is two switches, two wakes, four syscalls and two trips through the
/// pipe's own machinery, and subtracting a yield handover from it charges the
/// whole remainder to the wake path -- which is wrong, the wake itself is
/// about 50 ns. Whatever this case costs is the part that has nothing to do
/// with handing over a CPU.
fn pipe_no_block(out: &mut Tee, iters: u64) {
    let Some((r, w)) = pipe() else {
        out.line("switchbench: pipe failed, skipping the non-blocking case");
        return;
    };
    let mut byte = [0u8; 1];
    let mut failed = false;
    let each = best_ns(iters, || {
        if write(w, b"x") != 1 || read(r, &mut byte) != 1 {
            failed = true;
        }
    });
    close(r);
    close(w);
    if failed {
        out.line("switchbench: non-blocking pipe echo failed");
        return;
    }
    out.line(&format!(
        "switchbench pipe echo, nothing blocks {each:.0} ns for a write plus a read"
    ));
}

/// How late a sleep returns.
///
/// This is the check on the APIC timer: the scheduler arms the one-shot for
/// the earliest deadline it owes anyone, and a sleeper is the only thing that
/// asks for a deadline sooner than the running thread's timeslice. Arm it
/// wrong and a sleep returns on some unrelated later event instead.
///
/// It has to be measured with nothing else to run. Overshoot on a busy
/// machine is dominated by the woken thread waiting its turn behind everything
/// already queued -- under the 50-thread `sched-test` suite a 50 ms sleep
/// takes 130-280 ms, and none of that is the timer.
fn sleep_overshoot(out: &mut Tee) {
    // The first sleep of a process pays for whatever its return path touches
    // for the first time, which lands as a millisecond of overshoot on a
    // measurement that is otherwise accurate to tens of microseconds.
    thread::sleep(std::time::Duration::from_millis(1));

    let mut worst = 0i64;
    for ms in [1u64, 5, 20, 50] {
        let want = std::time::Duration::from_millis(ms);
        let t0 = Instant::now();
        thread::sleep(want);
        let took = t0.elapsed();
        let over = took.as_micros() as i64 - want.as_micros() as i64;
        worst = worst.max(over);
        out.line(&format!(
            "switchbench sleep {ms} ms took {} us, over by {over} us",
            took.as_micros()
        ));
    }
    out.line(&format!("switchbench sleep worst overshoot {worst} us"));
}

/// Pages of user memory a round trip touches per trip, on each side, when
/// asked to. Sized to be a working set rather than a rounding error: 32 pages
/// is 128 KiB, about what a small program's hot data is.
const TOUCH_PAGES: usize = 32;

/// A working set to walk, one byte per page.
///
/// It exists to price the refills a `CR3` reload causes. `black_box` keeps the
/// walk from being optimised away, and pre-touching in `new` keeps demand
/// faults out of the measurement -- what is being measured is the second and
/// later visits to a page, which is when only the TLB entry is missing.
struct WorkingSet {
    buf: Vec<u8>,
    pages: usize,
}

impl WorkingSet {
    fn new(pages: usize) -> Self {
        let mut set = Self {
            buf: alloc_pages(pages),
            pages,
        };
        set.walk();
        set
    }

    fn walk(&mut self) {
        for i in 0..self.pages {
            let at = i * 4096;
            self.buf[at] = self.buf[at].wrapping_add(1);
            std::hint::black_box(self.buf[at]);
        }
    }
}

fn alloc_pages(pages: usize) -> Vec<u8> {
    vec![0u8; pages.max(1) * 4096]
}

/// The same blocking round trip with a **thread** at the far end instead of a
/// process, so both sides share an address space.
///
/// This is the control for the cross-process case below. The two differ by
/// exactly two `CR3` reloads per trip and everything those cost -- the TLB
/// entries a reload discards and the refills the next syscall then takes -- and
/// nothing else: the same two pipes, the same two parks, the same two wakes.
/// Subtracting one from the other says how much of a blocking IPC is the
/// address space rather than the scheduler.
///
/// It is best-of-batches, unlike the cross-process case, which is a single
/// batch of `iters` and reads 16% apart between runs for that reason.
fn pipe_round_trip_threads(out: &mut Tee, iters: u64, touch: usize) {
    let (Some((up_r, up_w)), Some((down_r, down_w))) = (pipe(), pipe()) else {
        out.line("switchbench: pipe failed, skipping the thread round trip");
        return;
    };

    // The far end runs until its read fails, which is what closing `up_w`
    // below does to it.
    let peer = thread::spawn(move || {
        let mut byte = [0u8; 1];
        let mut set = WorkingSet::new(touch);
        while read(up_r, &mut byte) == 1 {
            if touch > 0 {
                set.walk();
            }
            if write(down_w, &byte) != 1 {
                return;
            }
        }
    });

    let mut byte = [0u8; 1];
    let mut failed = false;
    let mut set = WorkingSet::new(touch);
    let each = best_ns(iters, || {
        if write(up_w, b"x") != 1 || read(down_r, &mut byte) != 1 {
            failed = true;
        }
        if touch > 0 {
            set.walk();
        }
    });

    close(up_w);
    let _ = peer.join();
    close(down_r);

    if failed {
        out.line("switchbench: thread round trip failed");
        return;
    }
    out.line(&format!(
        "switchbench pipe round trip, one address space, touching {touch} pages \
         {each:.0} ns, {:.0} ns per park/wake",
        each / 2.0
    ));
}

/// Time a one-byte round trip through a pair of pipes, forked child at the
/// far end. Both sides block, so each trip is two parks and two wakes.
fn pipe_round_trip(out: &mut Tee, iters: u64, touch: usize) {
    let (Some((up_r, up_w)), Some((down_r, down_w))) = (pipe(), pipe()) else {
        out.line("switchbench: pipe failed, skipping the pipe case");
        return;
    };

    let child = fork();
    if child == 0 {
        close(up_w);
        close(down_r);
        let mut byte = [0u8; 1];
        let mut set = WorkingSet::new(touch);
        loop {
            if read(up_r, &mut byte) != 1 {
                std::process::exit(0);
            }
            if touch > 0 {
                set.walk();
            }
            if write(down_w, &byte) != 1 {
                std::process::exit(0);
            }
        }
    }
    if child < 0 {
        out.line("switchbench: fork failed, skipping the pipe case");
        return;
    }
    close(up_r);
    close(down_w);

    // Best of batches, with warmup, exactly as the thread case above -- and
    // that matters more here than anywhere else in this file. A `fork`ed child
    // starts with every page copy-on-write, so a single unwarmed batch charges
    // the round trip for the COW faults of starting up: measured the old way,
    // one batch of 2000 trips, this case read ~4900 ns against 1980 for the
    // thread case, and most of the difference was the faults rather than the
    // address space.
    let mut byte = [0u8; 1];
    let mut failed = false;
    let mut set = WorkingSet::new(touch);
    let each = best_ns(iters, || {
        if write(up_w, b"x") != 1 || read(down_r, &mut byte) != 1 {
            failed = true;
        }
        if touch > 0 {
            set.walk();
        }
    });

    close(up_w);
    close(down_r);
    edos_lib::process::kill(child as u64, edos_lib::process::SIGKILL);
    edos_lib::process::waitpid(child as u64);

    if failed {
        out.line("switchbench: pipe round trip failed");
        return;
    }
    out.line(&format!(
        "switchbench pipe round trip, touching {touch} pages {each:.0} ns, \
         {:.0} ns per park/wake",
        each / 2.0
    ));
}
