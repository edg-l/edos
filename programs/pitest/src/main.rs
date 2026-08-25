//! Whether a userspace lock hands its holder the waiter's priority.
//!
//! The kernel's own `prio-inversion` cases (`thread/sched_test.rs`) cover
//! `BlockingMutex` and `RwLock`. Neither reaches the futex path, which is the
//! one a *program* blocks on, and which is different in kind: a futex word is
//! opaque to the kernel, so unlike every in-kernel lock there is no owner to
//! read out of it. `edos_lib::sync::PiMutex` puts the owner in the word and
//! waits with `futex_wait_pi`; this is what says the loan arrives.
//!
//! # What it measures, and why it is a difference rather than a number
//!
//! Userspace cannot ask what CPU time a thread has had — there is no clockid on
//! `clock_gettime` for it — so the kernel test's "section took N times its own
//! CPU" is not available here. What is available is a controlled difference:
//! run the **same fixed work** inside the section twice, once with nobody
//! waiting and once with a top-priority thread blocked on the lock, against the
//! same hogs both times. Wall clock is then the only thing that varies, and the
//! ratio between the two runs is what inheritance is worth.
//!
//! With the loan the holder is served at the waiter's weight, without it at its
//! own: priority 15 against priority 7 is 6104/1024 of the weight table, so the
//! contended section should finish several times sooner when somebody important
//! is waiting for it. Without inheritance the two runs are the same length,
//! because the waiter changes nothing about how the holder is scheduled.
//!
//! # The hogs
//!
//! The inversion needs the holder to be *preempted*, which needs a CPU with
//! more than the holder on it, and enough of them that the balancer cannot
//! find it an emptier one. Their count comes from `/proc/sched` rather than a
//! guess, so the shape is the same on a 1-CPU and a 16-CPU guest.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

use edos_lib::process::{SchedAttr, sched_getattr, sched_setattr};
use edos_lib::sync::{PiMutex, gettid};

/// The holder's own priority: the default, so it is the thread nothing is
/// giving anything to.
const LOW_PRIORITY: u32 = 7;
/// The hogs. Five levels up is 1.25^5 = 3.05x of the weight table, enough that
/// they take most of a shared CPU from an unlent holder.
const MID_PRIORITY: u32 = 12;
/// The waiter, at the top of the table, so what it lends is unambiguous.
const HIGH_PRIORITY: u32 = 15;

/// Spins of the section's inner loop. Fixed rather than timed: the whole method
/// is that both runs do identical work, so the only thing wall clock can be
/// reporting is how much CPU the holder was given.
const SECTION_SPINS: u64 = 12_000_000;

/// Mid-priority spinners per CPU.
///
/// Enough that no placement can give the holder a CPU to itself, which is what
/// the multi-CPU case needs and the single-CPU case gets for free. At two per
/// CPU a 4-CPU guest measured 1.77x where one CPU gave 3.86x, because the
/// balancer kept finding the holder somewhere less crowded; the fix is to leave
/// nowhere less crowded.
const HOGS_PER_CPU: usize = 4;

/// How long the holder stays in the section before the clock starts, so a
/// waiter has reached the lock and blocked on it by then.
const SETTLE_MS: u128 = 20;

/// How much faster the contended section must be with a top-priority waiter
/// behind it than without one.
///
/// Derived from the weight table for the shape this builds: the holder shares
/// its CPU with [`HOGS_PER_CPU`] hogs, so it is served `w / (w + 4 * 3125)` of
/// it -- 1024 unlent against 6104 lent, which is 7.6% against 32.8%, a ratio of
/// 4.3x. Gating at 2x leaves headroom for the wake at the end of the section
/// and for a holder that is briefly better placed, while staying far above the
/// 1.0x that no inheritance gives.
const GATE_X100: u64 = 200;

fn set_priority(priority: u32) {
    let slice_ns = sched_getattr(0).map(|a| a.slice_ns).unwrap_or(1_000_000);
    let attr = SchedAttr {
        priority,
        _pad: 0,
        slice_ns,
    };
    assert!(
        sched_setattr(0, &attr).is_ok(),
        "pitest: sched_setattr(priority {priority}) failed"
    );
}

/// CPUs the scheduler has registered, read from `/proc/sched`.
///
/// Its body is one row per CPU under a header line, which is what makes the hog
/// count a fact about the machine rather than a constant that is wrong on every
/// other one.
fn cpu_count() -> usize {
    let text = match std::fs::read_to_string("/proc/sched") {
        Ok(text) => text,
        Err(e) => {
            println!("pitest: /proc/sched unreadable ({e}), assuming 1 CPU");
            return 1;
        }
    };
    let rows = text
        .lines()
        .filter(|line| {
            let mut fields = line.split_whitespace();
            // A CPU row starts with the number; the header starts with "CPU".
            fields.next().is_some_and(|f| f.parse::<u32>().is_ok())
        })
        .count();
    rows.max(1)
}

/// The section itself: identical work every time it is called.
///
/// `black_box` on the accumulator, or the whole loop is dead code and both runs
/// measure nothing at the same speed.
fn burn(counter: &mut u64) {
    for i in 0..SECTION_SPINS {
        *counter = counter.wrapping_add(i ^ 0x9e37_79b9);
        std::hint::black_box(*counter);
    }
}

/// One timed section, with `waiter` deciding whether a top-priority thread
/// blocks on the lock while it runs.
///
/// The waiter is started *after* the holder is inside the section and is
/// waited for before returning, so the whole of its wait overlaps the whole of
/// the measured work.
fn timed_section(lock: &Arc<PiMutex<u64>>, waiter: bool) -> u64 {
    let inside = Arc::new(AtomicBool::new(false));
    let waiter_thread = if waiter {
        let lock = Arc::clone(lock);
        let inside = Arc::clone(&inside);
        Some(thread::spawn(move || {
            set_priority(HIGH_PRIORITY);
            while !inside.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            drop(lock.lock());
        }))
    } else {
        None
    };

    let elapsed;
    {
        let mut guard = lock.lock();
        inside.store(true, Ordering::Release);
        // Give a waiter time to reach the lock and block, so the loan is in
        // force for the whole of the work rather than for the tail of it.
        let settle = Instant::now();
        while settle.elapsed().as_millis() < SETTLE_MS {
            std::hint::spin_loop();
        }
        // Clock started after the settle, not before. It is a fixed cost either
        // way, so leaving it in would not change which run is faster -- but it
        // would shrink the ratio between them towards 1 and make the gate below
        // read as a weaker result than the mechanism actually gives.
        let start = Instant::now();
        burn(&mut guard);
        elapsed = start.elapsed().as_micros() as u64;
    }

    if let Some(t) = waiter_thread {
        t.join().expect("pitest: waiter thread panicked");
    }
    elapsed
}

fn main() {
    let cpus = cpu_count();
    let hog_count = cpus * HOGS_PER_CPU;
    println!(
        "pitest: {cpus} cpu(s), {hog_count} hogs at priority {MID_PRIORITY}, tid {}",
        gettid()
    );

    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicU64::new(0));
    let hogs: Vec<_> = (0..hog_count)
        .map(|_| {
            let stop = Arc::clone(&stop);
            let ready = Arc::clone(&ready);
            thread::spawn(move || {
                set_priority(MID_PRIORITY);
                ready.fetch_add(1, Ordering::AcqRel);
                while !stop.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
            })
        })
        .collect();
    while ready.load(Ordering::Acquire) < hog_count as u64 {
        std::hint::spin_loop();
    }

    set_priority(LOW_PRIORITY);
    let lock = Arc::new(PiMutex::new(0u64));

    // Warm first: the section's pages are untouched on the first pass and would
    // otherwise charge that run for page faults the other does not pay.
    timed_section(&lock, false);

    let alone = timed_section(&lock, false);
    let with_waiter = timed_section(&lock, true);

    stop.store(true, Ordering::Release);
    for h in hogs {
        h.join().expect("pitest: hog thread panicked");
    }

    let speedup_x100 = alone.saturating_mul(100) / with_waiter.max(1);
    println!(
        "pitest: section {alone} us with nobody waiting, {with_waiter} us with a \
         priority-{HIGH_PRIORITY} waiter ({}.{:02}x)",
        speedup_x100 / 100,
        speedup_x100 % 100,
    );

    // On one CPU the hogs and the holder cannot avoid each other; on many they
    // could in principle, and a section that was never preempted says nothing
    // about inheritance either way. The unwaited run is the one that would show
    // it, so it is the one to check.
    assert!(
        alone > 1_000,
        "pitest: the unwaited section took only {alone} us, so the hogs never preempted the \
         holder and there was no inversion to measure"
    );

    assert!(
        speedup_x100 >= GATE_X100,
        "pitest: a priority-{HIGH_PRIORITY} waiter made the priority-{LOW_PRIORITY} holder's \
         section {}.{:02}x faster, under the {}.{:02}x gate -- the holder was served at its own \
         weight while the top-priority thread in the process waited behind it, so futex_wait_pi \
         lent it nothing",
        speedup_x100 / 100,
        speedup_x100 % 100,
        GATE_X100 / 100,
        GATE_X100 % 100,
    );

    println!("pitest: all tests passed");
}
