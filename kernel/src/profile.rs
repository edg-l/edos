//! Where the CPU actually is, sampled from the timer interrupt.
//!
//! Everything else that measures this kernel is a probe somebody placed by
//! hand or a benchmark that times a whole operation, so both require knowing
//! the answer before asking the question. This asks it the other way round:
//! interrupt the machine on a fixed period, write down the interrupted
//! instruction pointer and the return addresses above it, and let the shape of
//! a thousand of those say where the time went.
//!
//! # What the guest does and does not do
//!
//! The guest resolves nothing. A sample is raw addresses, because the ELF
//! files with the DWARF in them are on the build host and `addr2line` already
//! reads them there. `scripts/profile-resolve` is the other half.
//!
//! # Frame pointers
//!
//! The kernel is built with `-C force-frame-pointers=yes` (see
//! `kernel/GNUmakefile`), so `rbp` really is the head of a linked list of
//! frames and the walk below is exact rather than heuristic. The same flag is
//! set for the userspace workspace for the same reason. Without it a walk
//! reads whatever an optimised function left in `rbp` and invents stacks.
//!
//! # Two things it cannot see, and they are properties of the mechanism
//!
//! - **Code running with interrupts disabled.** The sample is taken *by* an
//!   interrupt, so a region that has them off is never the interrupted
//!   instruction; its time is charged to whatever runs next. `IrqSpinlock`
//!   holders and `without_interrupts` bodies are therefore systematically
//!   under-reported. The fix is an NMI, which needs a PMU the `qemu64` model
//!   this tree boots does not have.
//! - **The user half of a stack caught in a syscall.** A sample lands either
//!   in ring 3 or in ring 0, and reports the stack it interrupted. A thread
//!   inside a syscall gives its kernel stack and no user frames, so a user
//!   profile shows where a program computes, not which of its call sites
//!   entered the kernel.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use edos_profile_abi::{
    MAX_FRAMES, MAX_PERIOD_NS, MIN_PERIOD_NS, SAMPLE_BROKEN_CHAIN, SAMPLE_IDLE, SAMPLE_TRUNCATED,
    SAMPLE_USER, Sample, Stats,
};
use x86_64::PrivilegeLevel;

use crate::{
    memory::{KTHREAD_STACK_SIZE, vma::USER_VA_END},
    smp::current_cpu_index,
    thread::{context::CpuContext, irqlock::IrqSpinlock, waitqueue::WaitQueue},
    timer::Instant,
    util::{per_cpu::get_percpu_data, uaccess::read_u64_nofault},
};

/// Samples the ring holds. At 296 bytes each this is ~1.2 MiB, allocated when
/// a profiler claims the session and freed when it lets go.
///
/// Sized against the drain rather than the sample rate: a profiler that reads
/// once every 100 ms at 1 kHz on 16 CPUs has 1600 samples outstanding, and the
/// ring is meant to absorb a profiler descheduled for rather longer than that.
const RING_CAP: usize = 4096;

/// Samples one `profile_read` may move in a single call, bounding both the
/// bounce buffer and how long the ring stays locked. A CPU whose timer fires
/// while the drainer holds the lock spins for the rest of this batch with
/// interrupts already off, so it is deliberately much smaller than the ring.
pub const MAX_READ_BATCH: usize = 128;

/// Longest a `profile_read` with no samples waiting parks before returning 0.
const MAX_WAIT_MS: u64 = 200;

/// Lowest kernel address. Anything below is a user address or garbage.
const KERNEL_BASE: u64 = 0xFFFF_8000_0000_0000;

struct Ring {
    buf: Vec<Sample>,
    /// Index of the oldest sample.
    tail: usize,
    len: usize,
}

impl Ring {
    /// Append `sample`, reporting whether there was room.
    ///
    /// A full ring refuses the new sample rather than evicting the oldest.
    /// Both directions lose the same amount of time; refusing keeps what
    /// survives a contiguous stretch of the program's life, which is the half
    /// that can still be read as a profile.
    fn push(&mut self, sample: Sample) -> bool {
        if self.len == RING_CAP {
            return false;
        }
        let head = (self.tail + self.len) % RING_CAP;
        self.buf[head] = sample;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<Sample> {
        if self.len == 0 {
            return None;
        }
        let sample = self.buf[self.tail];
        self.tail = (self.tail + 1) % RING_CAP;
        self.len -= 1;
        Some(sample)
    }
}

/// The ring, present only while a profiler holds the session.
///
/// Deliberately **unranked**. The rank stack is per thread, and an interrupt
/// handler pushes onto the stack of whichever thread it interrupted, so
/// ranking this lock would report an inversion against every lock that thread
/// legitimately holds. It is a true leaf: the sample path takes it, copies one
/// struct, and releases it without calling anything, and `IrqSpinlock` keeps
/// interrupts off while it is held so the timer cannot re-enter it on the CPU
/// that owns it.
static RING: IrqSpinlock<Option<Ring>> = IrqSpinlock::new(None);

/// Read on every timer tick, so it is the one thing the untraced path pays
/// for: a single relaxed load.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Sampling period in force, in nanoseconds.
static PERIOD_NS: AtomicU64 = AtomicU64::new(0);

/// Thread holding the session, or 0.
static OWNER_TID: AtomicU64 = AtomicU64::new(0);

static TAKEN: AtomicU64 = AtomicU64::new(0);

/// Samples the interrupt handler threw away because the ring was full.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Samples in the ring, published outside the lock so a parking reader can
/// test its wake condition without taking one.
static AVAILABLE: AtomicUsize = AtomicUsize::new(0);

/// Set while the profiler is parked, so the sample path only pays for a wake
/// when somebody is actually waiting on it.
static READER_WAITING: AtomicUsize = AtomicUsize::new(0);

static WAITQ: WaitQueue = WaitQueue::new();

/// Whether sampling is on. One relaxed load; this is the whole cost of the
/// profiler to a kernel that is not being profiled.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The sampling period, for the scheduler's timer clamp.
#[inline]
pub fn period_ns() -> u64 {
    PERIOD_NS.load(Ordering::Relaxed)
}

/// Claim the session and start sampling at `period_ns`.
///
/// The ring is built before the lock is taken. Allocating ~1.2 MiB reaches the
/// heap, and the heap has its own lock: taking it under this one would make
/// this lock something an interrupt handler waits behind an allocation for,
/// which is exactly what the "true leaf" claim above forbids.
pub fn start(owner: u64, period_ns: u64) -> Result<u64, ()> {
    let period = period_ns.clamp(MIN_PERIOD_NS, MAX_PERIOD_NS);
    let ring = Ring {
        buf: vec![Sample::zeroed(); RING_CAP],
        tail: 0,
        len: 0,
    };
    let mut guard = RING.lock();
    if guard.is_some() {
        return Err(());
    }
    *guard = Some(ring);
    TAKEN.store(0, Ordering::Relaxed);
    DROPPED.store(0, Ordering::Relaxed);
    AVAILABLE.store(0, Ordering::Relaxed);
    OWNER_TID.store(owner, Ordering::Relaxed);
    PERIOD_NS.store(period, Ordering::Relaxed);
    // Last, so no CPU samples into a ring that is not there yet.
    ENABLED.store(true, Ordering::Release);
    Ok(period)
}

/// Stop sampling and free the ring.
///
/// Clearing `ENABLED` first is what makes dropping the ring safe: a sample
/// path that already read it as true may still be holding the lock, and taking
/// the lock here waits that out.
pub fn stop() {
    ENABLED.store(false, Ordering::Release);
    let old = {
        let mut guard = RING.lock();
        OWNER_TID.store(0, Ordering::Relaxed);
        PERIOD_NS.store(0, Ordering::Relaxed);
        AVAILABLE.store(0, Ordering::Relaxed);
        guard.take()
    };
    // Freed outside the lock, for the reason `start` builds outside it.
    drop(old);
    WAITQ.wake_all();
}

/// Release a session its owner died holding, so the ring does not stay claimed
/// by a thread that will never call `stop`.
pub fn release_if_owner(tid: u64) {
    if OWNER_TID.load(Ordering::Relaxed) == tid {
        stop();
    }
}

pub fn owner_tid() -> u64 {
    OWNER_TID.load(Ordering::Relaxed)
}

pub fn stats() -> Stats {
    Stats {
        taken: TAKEN.load(Ordering::Relaxed),
        dropped: DROPPED.load(Ordering::Relaxed),
        period_ns: PERIOD_NS.load(Ordering::Relaxed),
        queued: AVAILABLE.load(Ordering::Relaxed) as u64,
    }
}

/// Move up to `max` samples out of the ring.
pub fn drain(out: &mut Vec<Sample>, max: usize) -> usize {
    let mut guard = RING.lock();
    let Some(ring) = guard.as_mut() else {
        return 0;
    };
    let mut moved = 0;
    while moved < max
        && let Some(sample) = ring.pop()
    {
        out.push(sample);
        moved += 1;
    }
    AVAILABLE.store(ring.len, Ordering::Relaxed);
    moved
}

/// Park until a sample is waiting, at most `MAX_WAIT_MS`.
pub fn wait_for_samples(timeout_ms: u64) {
    let timeout = timeout_ms.min(MAX_WAIT_MS);
    if timeout == 0 {
        return;
    }
    READER_WAITING.fetch_add(1, Ordering::AcqRel);
    WAITQ.wait_until_timeout(
        || AVAILABLE.load(Ordering::Acquire) > 0 || !ENABLED.load(Ordering::Acquire),
        Some(core::time::Duration::from_millis(timeout)),
    );
    READER_WAITING.fetch_sub(1, Ordering::AcqRel);
}

/// Take one sample of the CPU this runs on.
///
/// Called from the timer tick with interrupts already off, before the tick
/// does anything else: the frame is the interrupted one, and nothing in the
/// scheduler has run yet to appear in it.
///
/// `context` is the interrupt frame the tick handler pushed. Everything read
/// out of it belongs to the interrupted code, including `rbp`, which is the
/// head of its frame list.
pub fn take_sample(context: *const CpuContext) {
    // SAFETY: the timer interrupt handler passes the frame it pushed on the
    // current stack, so it is a live, aligned `CpuContext` for the whole of
    // this call, and interrupts are off so nothing overwrites it.
    let ctx = unsafe { &*context };
    let frame = &ctx.interrupt_stack_frame;

    let mut sample = Sample::zeroed();
    sample.time_ns = Instant::now().as_nanos();
    sample.cpu = current_cpu_index() as u32;

    let percpu = get_percpu_data();
    match percpu.with_current_thread(|t| t.id.0) {
        Some(tid) => sample.tid = tid,
        None => sample.flags |= SAMPLE_IDLE,
    }

    let rip = frame.instruction_pointer.as_u64();
    let rbp = ctx.rbp;
    sample.frames[0] = rip;

    let user = frame.code_segment.rpl() == PrivilegeLevel::Ring3;
    let (depth, flags) = if user {
        sample.flags |= SAMPLE_USER;
        walk_user(rbp, &mut sample.frames)
    } else {
        walk_kernel(rbp, &mut sample.frames)
    };
    sample.depth = depth as u32;
    sample.flags |= flags;

    let mut guard = RING.lock();
    let Some(ring) = guard.as_mut() else {
        return;
    };
    if ring.push(sample) {
        TAKEN.fetch_add(1, Ordering::Relaxed);
        AVAILABLE.store(ring.len, Ordering::Relaxed);
    } else {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    let waiting = READER_WAITING.load(Ordering::Acquire) > 0;
    drop(guard);
    if waiting {
        // The interrupt-context wake: this runs inside the timer tick.
        WAITQ.wake_all_irq();
    }
}

/// Walk the kernel frame list from `rbp`, filling `frames[1..]`.
///
/// Every read is bounded to the kernel stack the interrupted code was standing
/// on, which is what makes this safe to do in an interrupt: an address inside
/// that range is mapped by construction, so no read here can fault. A frame
/// pointer that leaves the range ends the walk rather than being chased.
///
/// Returns the number of valid frames including `frames[0]`, and the sample
/// flags the walk earned.
fn walk_kernel(rbp: u64, frames: &mut [u64; MAX_FRAMES]) -> (usize, u32) {
    let Some((lo, hi)) = current_kernel_stack() else {
        return (1, SAMPLE_BROKEN_CHAIN);
    };
    let mut rbp = rbp;
    let mut depth = 1;
    while depth < MAX_FRAMES {
        // Both the saved rbp and the return address must lie inside the frame,
        // so the range check covers the pair.
        if rbp < lo || rbp.saturating_add(16) > hi || !rbp.is_multiple_of(8) {
            return (depth, ended_at(depth));
        }
        let frame_ptr = rbp as *const u64;
        // SAFETY: the check above put `rbp` and `rbp + 16` inside the current
        // kernel stack and confirmed 8-byte alignment, so both reads are of
        // initialised, aligned, mapped memory. Their *contents* are untrusted —
        // a frame chain the walk cannot follow ends the walk rather than
        // faulting, which is what the next two checks are for.
        let next_rbp = unsafe { *frame_ptr };
        // SAFETY: as above; the range check covered both words of the frame.
        let ret = unsafe { *frame_ptr.add(1) };
        if ret < KERNEL_BASE {
            return (depth, ended_at(depth));
        }
        frames[depth] = ret;
        depth += 1;
        // The stack grows down, so a chain that does not climb is a cycle.
        if next_rbp <= rbp {
            return (depth, ended_at(depth));
        }
        rbp = next_rbp;
    }
    (depth, SAMPLE_TRUNCATED)
}

/// What a walk that stopped at `depth` frames has to say for itself.
///
/// Reaching the outermost frame is how a walk *normally* ends: the entry
/// trampoline leaves no caller above it, so the chain runs off the end of the
/// stack and the last read is refused. That is a complete stack, not a broken
/// one, and flagging it would mark every sample in a healthy profile.
///
/// A walk that produced no caller at all is the case worth reporting: the code
/// was interrupted between a function's `push rbp` and the `mov rbp, rsp` that
/// follows it, or is a leaf built without a frame pointer. Its one frame is
/// true and its callers are unknown.
fn ended_at(depth: usize) -> u32 {
    if depth == 1 { SAMPLE_BROKEN_CHAIN } else { 0 }
}

/// Walk the user frame list from `rbp`, filling `frames[1..]`.
///
/// Reads go through [`read_u64_nofault`], so a page that is not present ends
/// the walk instead of being faulted in — filling one blocks, and this runs
/// inside the tick. A stack whose pages are swapped out or not yet touched
/// therefore yields a short chain, flagged as broken.
fn walk_user(rbp: u64, frames: &mut [u64; MAX_FRAMES]) -> (usize, u32) {
    let mut rbp = rbp;
    let mut depth = 1;
    while depth < MAX_FRAMES {
        if rbp == 0 || rbp >= USER_VA_END {
            return (depth, ended_at(depth));
        }
        let (Some(next_rbp), Some(ret)) = (read_u64_nofault(rbp), read_u64_nofault(rbp + 8)) else {
            return (depth, ended_at(depth));
        };
        if ret == 0 || ret >= USER_VA_END {
            return (depth, ended_at(depth));
        }
        frames[depth] = ret;
        depth += 1;
        if next_rbp <= rbp {
            return (depth, ended_at(depth));
        }
        rbp = next_rbp;
    }
    (depth, SAMPLE_TRUNCATED)
}

/// The kernel stack the interrupted code is standing on, as `(low, high)`.
///
/// A thread on this CPU is on its own kernel stack; with no current thread the
/// CPU is in its idle loop on the per-CPU scheduler stack, which is allocated
/// by the same allocator and so has the same size.
fn current_kernel_stack() -> Option<(u64, u64)> {
    let percpu = get_percpu_data();
    let top = match percpu.with_current_thread(|t| t.kstack_top) {
        Some(top) => top,
        None => percpu.scheduler_stack_top.get(),
    };
    if top < KTHREAD_STACK_SIZE {
        return None;
    }
    Some((top - KTHREAD_STACK_SIZE, top))
}
