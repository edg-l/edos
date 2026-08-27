//! Where a context switch spends its time, stage by stage.
//!
//! Off unless the `sched-prof` feature is on, and then the probes are two
//! clock reads apiece around each step of the voluntary switch path. Totals
//! accumulate into one global table and are read through `/proc/sched_prof`;
//! the file reports cumulative work rather than a rate, so a measurement is a
//! read, the workload, and a second read.
//!
//! Two things bound how the numbers should be read:
//!
//! - **Take them on a single-CPU boot.** The table is global, so several CPUs
//!   switching at once both contend for its lines and average their stages
//!   together. `switchbench` wants one CPU for its own reasons anyway.
//! - **A stage carries its own probe.** Consecutive stages share the clock
//!   read between them, so the path grows by roughly one read per stage and
//!   every figure here is an upper bound on the step it names. `switch` spans
//!   the stages after it in the list, so the two do not add.

#[cfg(feature = "sched-prof")]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "sched-prof")]
use crate::timer::Instant;

/// A step of the switch path, and its index into the accumulator table.
#[derive(Clone, Copy)]
#[cfg_attr(
    not(feature = "sched-prof"),
    allow(dead_code, reason = "live only under the sched-prof feature")
)]
pub enum Stage {
    /// Copy the outgoing thread's registers into its `ctx`.
    SaveCtx,
    /// `fxsave` of the outgoing thread's FPU area.
    SaveFpu,
    /// Read `FS.base` and publish it on the outgoing thread.
    SaveTls,
    /// The transition function: state change and runqueue enqueue.
    Transition,
    /// Move any expired sleeper onto a runqueue.
    WakeSleepers,
    /// Take the runqueue and pop the next thread.
    Pick,
    /// Everything `context_switch_to` does, which is every stage below.
    Switch,
    /// Publish the incoming thread as current and compute its slice deadline.
    Publish,
    /// Re-arm the APIC one-shot for that deadline.
    Timer,
    /// Copy the incoming thread's `ctx` over the frame to be restored.
    RestoreCtx,
    /// Reload `CR3` if the incoming thread has another address space.
    Page,
    /// Write the incoming thread's `FS.base`.
    RestoreTls,
    /// `fxrstor` of the incoming thread's FPU area.
    RestoreFpu,
    /// Everything `do_wake` does, from publishing the token to handing the
    /// target to a runqueue or nudging the CPU it is already on.
    Wake,
    /// The enqueue half of a wake: pick the target CPU, publish Ready, take
    /// that CPU's runqueue.
    WakeEnqueue,
    /// `sys_write` on a pipe: copy the user's bytes into the kernel.
    PipeCopyIn,
    /// `sys_write` on a pipe: take the pipe, append, build the notification.
    PipeWrite,
    /// `sys_read` on a pipe: take the pipe, drain, build the notification.
    PipeRead,
    /// Deliver a pipe's notification: poller updates and a reader wake.
    PipeFlush,
    /// `sys_read` on a pipe: copy the drained bytes out to the user.
    PipeCopyOut,
}

#[cfg(feature = "sched-prof")]
const STAGE_NAMES: [&str; 20] = [
    "save_ctx",
    "save_fpu",
    "save_tls",
    "transition",
    "wake_sleepers",
    "pick",
    "switch",
    "publish",
    "timer",
    "restore_ctx",
    "page",
    "restore_tls",
    "restore_fpu",
    "wake",
    "wake_enqueue",
    "pipe_copy_in",
    "pipe_write",
    "pipe_read",
    "pipe_flush",
    "pipe_copy_out",
];

#[cfg(feature = "sched-prof")]
static NANOS: [AtomicU64; STAGE_NAMES.len()] = [const { AtomicU64::new(0) }; STAGE_NAMES.len()];
#[cfg(feature = "sched-prof")]
static COUNTS: [AtomicU64; STAGE_NAMES.len()] = [const { AtomicU64::new(0) }; STAGE_NAMES.len()];

/// Timestamp to hand to the first [`record`] of a run of stages.
#[cfg(feature = "sched-prof")]
#[inline]
pub fn now_ns() -> u64 {
    Instant::now().as_nanos()
}

#[cfg(not(feature = "sched-prof"))]
#[inline(always)]
pub fn now_ns() -> u64 {
    0
}

/// Charge the time since `start` to `stage`, and return the timestamp that
/// closed it so the next stage can open from the same read.
#[cfg(feature = "sched-prof")]
#[inline]
pub fn record(stage: Stage, start: u64) -> u64 {
    let now = Instant::now().as_nanos();
    let index = stage as usize;
    NANOS[index].fetch_add(now.wrapping_sub(start), Ordering::Relaxed);
    COUNTS[index].fetch_add(1, Ordering::Relaxed);
    now
}

#[cfg(not(feature = "sched-prof"))]
#[inline(always)]
pub fn record(_stage: Stage, _start: u64) -> u64 {
    0
}

/// `/proc/sched_prof`: cumulative nanoseconds and calls per stage.
#[cfg(feature = "sched-prof")]
pub fn render() -> alloc::string::String {
    use alloc::string::String;
    use core::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<14} {:>12} {:>12} {:>10}",
        "stage", "calls", "ns", "ns/call"
    );
    for (index, name) in STAGE_NAMES.iter().enumerate() {
        let calls = COUNTS[index].load(Ordering::Relaxed);
        let nanos = NANOS[index].load(Ordering::Relaxed);
        let per = nanos.checked_div(calls).unwrap_or(0);
        let _ = writeln!(out, "{name:<14} {calls:>12} {nanos:>12} {per:>10}");
    }
    out
}

#[cfg(not(feature = "sched-prof"))]
pub fn render() -> alloc::string::String {
    alloc::string::String::from("sched-prof feature is off; rebuild with it to collect stages\n")
}
