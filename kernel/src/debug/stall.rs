//! Names every thread and where it is blocked when the machine stops running
//! anything.
//!
//! A kernel that wedges with no runnable thread leaves no evidence at all:
//! the serial log simply stops, every vCPU sits in `run_idle`, and QMP can say
//! that the guest is alive but not what it is waiting for. That shape has cost
//! more than one investigation, because the one question worth asking --
//! *which wait never returned* -- is the one nothing answers.
//!
//! This answers it. Under the `stall-dump` feature the scheduler counts
//! switches into real threads, and an idle CPU that watches that count stand
//! still for [`STALL_MS`] prints every thread, its state, and a backtrace
//! walked from its saved frame pointer. Resolve the addresses on the host with
//! `addr2line -e kernel/target/x86_64-unknown-none/debug/edos-kernel -f -C`.
//!
//! It is a feature and not a cmdline knob because the counter sits on the
//! context switch, which `doc/SCHED-ROADMAP.md` measures in tens of
//! nanoseconds. Nothing is paid for it in an ordinary build.
//!
//! A guest that is *legitimately* idle -- a desktop waiting for input -- looks
//! exactly like a wedged one from here, so this is a debugging build to point
//! at a workload that should never be idle, the way `heap-poison` is a
//! debugging build to point at a heap that should never be corrupt.
//!
//! Not every switch is progress. The AHCI and NVMe watchdogs wake once a
//! second for as long as the machine is up, so a counter that took their
//! switches as work stood still for at most one second and could never reach
//! [`STALL_MS`] on any boot with a storage controller -- which is every boot
//! this tree runs. A thread that only polls therefore calls
//! [`mark_heartbeat`] and is not counted; see it for what qualifies.
//!
//! `make stall-check` is the proof that this fires: it boots a kernel whose
//! `stalltest` cmdline deadlocks two kthreads on a [`BlockingMutex`] instead
//! of loading init, and fails unless the dump names them both. Reach for
//! `scripts/wedge-probe` first all the same -- it reads the kernel's counters
//! out of a running guest over QMP and needs no kernel build at all.

use alloc::sync::Arc;
use core::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use crate::{
    emergency_println, log,
    thread::{
        mutex::BlockingMutex,
        scheduler::{current_thread, thread_park, thread_sleep},
        thread::{State, Thread, list_threads},
        util::queue_spawn_kthread_named,
    },
    timer::Instant,
};

/// How long the machine may run nothing before this calls it a stall.
///
/// Well above any legitimate pause on the workloads this is pointed at, and
/// well below the twelve seconds of silence `scripts/wedge-probe` waits out
/// before it stops the guest, so a dump lands inside the run that produced it.
const STALL_MS: u64 = 4_000;

/// Switches into a thread that is not the idle loop. Relaxed and monotonic:
/// the only question asked of it is whether it changed.
pub static SWITCHES: AtomicU64 = AtomicU64::new(0);

/// Claimed by the first CPU to call the stall, so four idle CPUs print one
/// dump between them rather than four interleaved ones.
static DUMPED: AtomicBool = AtomicBool::new(false);

/// Threads whose switches are not progress, by [`ThreadId`]. Zero is free.
///
/// [`ThreadId`]: crate::thread::thread::ThreadId
static HEARTBEATS: [AtomicU64; MAX_HEARTBEATS] = [const { AtomicU64::new(0) }; MAX_HEARTBEATS];

/// Room for the two storage watchdogs and any later poller of the same shape.
/// A registry that fills silently would make the detector unfirable again, so
/// an overflow says so on the serial line.
const MAX_HEARTBEATS: usize = 8;

/// Declare the calling thread a heartbeat: its switches do not count as work.
///
/// For a thread whose whole body is `sleep(tick); look; repeat` and which
/// therefore keeps running at its tick on a machine that has stopped. The
/// test is not "wakes periodically" but "wakes periodically *and* finding
/// nothing is its normal answer": a thread that sleeps between units of real
/// work would hide exactly the stall this exists to catch.
pub fn mark_heartbeat() {
    let Some(thread) = current_thread() else {
        return;
    };
    let tid = thread.id.0;
    for slot in &HEARTBEATS {
        if slot
            .compare_exchange(0, tid, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
    emergency_println!("stall: heartbeat registry full, tid {tid} counts as work");
}

fn is_heartbeat(tid: u64) -> bool {
    HEARTBEATS
        .iter()
        .any(|slot| slot.load(Ordering::Relaxed) == tid)
}

/// Count a switch into a real thread. Called from the scheduler.
#[inline(always)]
pub fn note_switch(tid: u64) {
    if is_heartbeat(tid) {
        return;
    }
    SWITCHES.fetch_add(1, Ordering::Relaxed);
}

/// The switch count last seen to move, and when. Global rather than per-CPU
/// or per-idle-entry: an idle loop is entered and left constantly -- a waker
/// that sets `has_work` without leaving a runnable thread sends a CPU round
/// again -- so a window that started over on each entry would measure the
/// gaps between those and never the stall.
static LAST_SWITCHES: AtomicU64 = AtomicU64::new(0);
static LAST_MOVED_MS: AtomicU64 = AtomicU64::new(0);

/// Poll once from an idle CPU. Prints the dump, at most once per boot, if the
/// machine has run nothing for [`STALL_MS`].
pub fn poll() {
    let switches = SWITCHES.load(Ordering::Relaxed);
    let now = now_ms();
    if switches != LAST_SWITCHES.load(Ordering::Relaxed) {
        LAST_SWITCHES.store(switches, Ordering::Relaxed);
        LAST_MOVED_MS.store(now, Ordering::Relaxed);
        return;
    }
    let since = LAST_MOVED_MS.load(Ordering::Relaxed);
    // Zero until the first switch, which is every boot before the scheduler
    // starts; there is nothing to dump then and the clock has not started.
    if since == 0 {
        LAST_MOVED_MS.store(now, Ordering::Relaxed);
        return;
    }
    if now.saturating_sub(since) < STALL_MS {
        return;
    }
    // Re-arm whatever happens, so a CPU that loses the claim below does not
    // re-enter this on every pass for the rest of the boot.
    LAST_MOVED_MS.store(now, Ordering::Relaxed);
    if DUMPED.swap(true, Ordering::AcqRel) {
        // One line per window after the first dump, so a reader can tell a
        // machine that stayed stopped from one that started again: the full
        // dump is a snapshot and says nothing about what happened after it.
        emergency_println!("stall: still nothing, switches={switches}");
        return;
    }
    dump(switches);
}

fn now_ms() -> u64 {
    Instant::now().as_nanos() / 1_000_000
}

/// Print every thread and the stack it is blocked on.
///
/// `emergency_println!` rather than `log!` or `println!`, for two reasons that
/// both bite here. The ring buffer is drained by a kthread, and a machine with
/// nothing runnable has no kthread to drain it; and the ordinary serial path
/// takes a lock whose holder may be one of the threads this is trying to
/// name. The emergency path spins on the UART's THRE bit and takes nothing.
fn dump(switches: u64) {
    emergency_println!(
        "\n=== STALL: nothing ran for {} ms ({} switches this boot) ===",
        STALL_MS,
        switches
    );
    for thread in list_threads() {
        let state = thread.state();
        emergency_println!(
            "  tid {:<4} {:<8?} {}",
            thread.id.0,
            state,
            thread.name.as_str()
        );
        if matches!(state, State::Parked | State::Sleeping | State::Waking) {
            backtrace(&thread);
        }
    }
    emergency_println!("=== end stall dump ===");
}

/// Walk a blocked thread's saved frame-pointer chain.
///
/// The kernel is built with `-C force-frame-pointers=yes`, so `rbp` is a
/// linked list: `[rbp]` is the caller's `rbp` and `[rbp + 8]` its return
/// address. `ctx` is the frame the scheduler saved when it switched this
/// thread out, which for a blocked thread is the park itself.
fn backtrace(thread: &Arc<Thread>) {
    const KERNEL_BASE: u64 = 0xFFFF_8000_0000_0000;
    const MAX_FRAMES: usize = 16;

    // `try_lock` because this runs from an idle loop with no business waiting
    // on the scheduler, and a contended `ctx` means the thread is being
    // switched right now -- in which case it is not the one that is stuck.
    let Some(ctx) = thread.ctx.try_lock() else {
        emergency_println!("        <ctx busy>");
        return;
    };
    let mut rbp = ctx.rbp;
    drop(ctx);

    for _ in 0..MAX_FRAMES {
        if rbp < KERNEL_BASE || !(rbp as *const u64).is_aligned() {
            break;
        }
        let frame = rbp as *const u64;
        // SAFETY: `rbp` is above the kernel base and aligned, and every
        // kernel stack is mapped for the life of its thread, which this
        // `Arc` holds. A frame pointer that has been clobbered walks off
        // into kernel memory rather than out of it, and the checks above
        // and the `next <= rbp` test below bound how far.
        let (ret, next) = unsafe { (*frame.add(1), *frame) };
        if ret == 0 {
            break;
        }
        emergency_println!("        {ret:#018x}");
        if next <= rbp {
            break;
        }
        rbp = next;
    }
}

/// The lock the `stalltest` cmdline deadlocks two kthreads on.
static DEADLOCK: BlockingMutex<()> = BlockingMutex::new(());

/// Deadlock two kthreads on [`DEADLOCK`], so the machine has a real stall to
/// find. Started from `mount_system_fs` in place of loading init, because a
/// desktop is never idle for [`STALL_MS`]: the taskbar clock alone is work,
/// and work is what this refuses to call a stall.
///
/// `stall-holder` takes the lock and parks forever; `stall-waiter` blocks
/// behind it. Neither is a heartbeat, so both stop the counter, and both are
/// `Parked` and therefore backtraced by the dump.
pub fn spawn_deadlock() {
    log!("stall: stalltest — deadlocking two kthreads instead of loading init");
    queue_spawn_kthread_named("stall-holder", holder as *const () as u64);
    queue_spawn_kthread_named("stall-waiter", waiter as *const () as u64);
}

extern "C" fn holder() -> ! {
    let _guard = DEADLOCK.lock();
    loop {
        thread_park();
    }
}

extern "C" fn waiter() -> ! {
    // Let the holder win the lock, so the two names in the dump describe what
    // each thread actually did. Either order deadlocks; only this one is
    // legible.
    thread_sleep(Duration::from_millis(500));
    let _guard = DEADLOCK.lock();
    loop {
        thread_park();
    }
}
