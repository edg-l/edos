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
//! **It has never been seen to fire, and that is worth knowing before it is
//! trusted.** It was built to explain a wedge that turned out not to be one:
//! the machine was running about a thousand threads a second inside a
//! watchdog's own millisecond sleep, so there was no stall to detect. What
//! answered that question was reading the kernel's counters out of the guest
//! over QMP (`scripts/wedge-probe`), which needs no kernel build at all.
//! Reach for that first, and treat a silent run under this feature as
//! unproven rather than as evidence of anything.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{
    emergency_println,
    thread::thread::{State, Thread, list_threads},
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

/// Count a switch into a real thread. Called from the scheduler.
#[inline(always)]
pub fn note_switch() {
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
