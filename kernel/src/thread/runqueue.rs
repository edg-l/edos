//! Earliest Eligible Virtual Deadline First.
//!
//! Every runnable thread accumulates *virtual* time at a rate inverse to its
//! weight, so a heavy thread's clock runs slow and it is owed more real CPU to
//! reach the same virtual point. `V` is the weighted average of those clocks: a
//! thread behind `V` has been under-served and is **eligible**, one ahead of it
//! has had its share already. Each activation asks for a slice, which fixes a
//! **virtual deadline** at `vruntime + slice/weight`, and the pick is the
//! eligible thread with the earliest deadline.
//!
//! What that buys over the strict-priority buckets it replaces:
//!
//! - **Priority is continuous.** The buckets gave a fixed 2:1 of the picks
//!   between adjacent levels whatever the gap, so a 16-level dial had two
//!   settings. A weight ratio is the share.
//! - **Starvation is impossible by construction** rather than by an escape
//!   hatch. A passed-over thread falls behind `V`, becomes eligible, and its
//!   deadline is already in the past, so it wins outright. The bucket scheme
//!   needed `STARVE_STREAK_LIMIT` to bound the inversion that lets a low
//!   priority lock holder block a high priority spinner, and serviced only the
//!   highest non-empty level below the top — so with three levels occupied the
//!   bottom one could wait indefinitely.
//! - **Latency is a per-thread request, not a global constant.** A shorter
//!   slice means an earlier deadline, so a thread that wants to run *sooner*
//!   asks for *less*, and pays for it in switches rather than in throughput
//!   taken from anyone else.
//!
//! **Deliberately not Linux-shaped in three places.** Linux keys an RB-tree on
//! vruntime, augmented with subtree minimum deadlines, and maintains
//! `avg_vruntime` incrementally, because one of its runqueues can hold
//! thousands of threads. A runqueue here holds single digits and the pick it
//! replaces was already an O(16) walk of the priority buckets, so this scans a
//! single unsorted list and recomputes `V` in the same pass. That keeps `V` a
//! fact about the queue rather than a running sum every enqueue, dequeue and
//! steal has to remember to adjust — the same reason `Scheduler::load` is
//! derived. Linux also preserves lag across a sleep; see [`RunQueue::place`].

use core::{cmp, sync::atomic::Ordering, time::Duration};

use alloc::sync::Arc;
use intrusive_list::IntrusiveList;

use crate::thread::thread::{State, Thread};

/// Priority levels a thread may be set to. Kept from the bucket scheme it
/// replaces because it is the shape `set_priority` and `/proc` already speak;
/// the level now selects a weight rather than a queue.
pub const PRIORITY_LEVELS: usize = 16;

pub const DEFAULT_PRIORITY: u8 = 7;
pub const IO_PRIORITY: u8 = 8;

/// The weight of [`DEFAULT_PRIORITY`], and the unit every virtual charge is
/// scaled against. Only ratios matter; this fixes the scale.
pub const NICE_0_WEIGHT: u64 = 1024;

/// Weight per priority level: 1.25x per step, centred on [`DEFAULT_PRIORITY`].
///
/// The ratio is Linux's, and it is chosen rather than derived: 1.25 per level
/// is about a 10% change in share against one competitor, which is small enough
/// that a single step is a nudge and large enough that a few steps are decisive.
/// End to end the table spans 28x, against Linux's ~5900x over 40 nice levels —
/// this dial is shorter and each step is worth the same.
const PRIORITY_WEIGHT: [u64; PRIORITY_LEVELS] = [
    215, 268, 336, 419, 524, 655, 819, 1024, 1280, 1600, 2000, 2500, 3125, 3906, 4883, 6104,
];

/// The service a thread asks for each time it is picked.
///
/// Derived from what an activation costs rather than picked: `arm_timer_until`
/// writes the APIC one-shot, which a hypervisor traps and answers by re-arming
/// a host timer, measured at ~1 us. A 1 ms request keeps that under 0.15% with
/// the switch itself on top. It also sets the latency floor this kernel can
/// offer — the reason a Shinjuku-style 5 us quantum is not available here is
/// that it would spend 20% of the machine in the hypervisor.
///
/// The compositor wakes about 77 times a second, so a frame is ~13 ms: at this
/// request roughly thirteen runnable threads each get the CPU once per frame.
pub const BASE_SLICE: Duration = Duration::from_micros(1000);

/// The request an interrupt-priority wake makes on the thread's behalf.
///
/// A shorter request is an earlier deadline, which is how EEVDF says "run this
/// sooner" — where the bucket scheme said it by adding two priority levels and
/// so also gave the thread a larger *share*, which was never the intent. It
/// applies to that activation alone: the next deadline comes from the thread's
/// own slice, so the boost decays without anyone having to undo it.
pub const LATENCY_SLICE: Duration = Duration::from_micros(250);

/// Weight for a priority level, saturating at the top of the table.
pub fn weight_of(priority: u8) -> u64 {
    PRIORITY_WEIGHT[cmp::min(priority as usize, PRIORITY_LEVELS - 1)]
}

/// Virtual nanoseconds bought by `delta_ns` of CPU at `weight`.
///
/// A heavier thread's virtual clock runs slower, so it takes more real time to
/// reach the same virtual point — which is the whole of how weight becomes
/// share. `u64` is enough: overflow needs a single run of 2^54 ns, 208 days.
pub fn virtual_delta(delta_ns: u64, weight: u64) -> u64 {
    debug_assert!(weight > 0, "virtual_delta: zero weight");
    delta_ns.saturating_mul(NICE_0_WEIGHT) / weight.max(1)
}

pub(crate) struct RunQueue {
    queue: IntrusiveList<Thread>,

    /// A watermark for `V`, never allowed to move backwards.
    ///
    /// `V` is computed from the queue's contents, so an empty queue has none
    /// and a queue that drains and refills would otherwise restart its virtual
    /// clock — handing whatever arrives next an unbounded head start over
    /// everything that ran before. This is what a thread is placed against.
    vtime: u64,
}

impl RunQueue {
    pub(crate) fn new() -> Self {
        Self {
            queue: IntrusiveList::new(),
            vtime: 0,
        }
    }

    /// The weighted average of the queue's virtual clocks: the point a thread
    /// has been served exactly its share up to.
    ///
    /// Taken over the queue alone, which is the right set. The thread running
    /// now is not in it and does not belong in it — it is being replaced, and
    /// if it stays runnable it has already been enqueued by the time this is
    /// asked, so it is counted then and only then.
    fn avg_vruntime(&self) -> u64 {
        let mut weighted: u128 = 0;
        let mut total: u128 = 0;
        for ptr in self.queue.iter() {
            let thread = unsafe { &*ptr };
            let weight = weight_of(thread.priority()) as u128;
            // Relative to the watermark so the products stay small; absolute
            // vruntimes are free to grow for as long as the machine is up.
            let relative = thread
                .vruntime
                .load(Ordering::Acquire)
                .saturating_sub(self.vtime);
            weighted += relative as u128 * weight;
            total += weight;
        }
        if total == 0 {
            return self.vtime;
        }
        self.vtime.saturating_add((weighted / total) as u64)
    }

    /// Where a thread that was not runnable a moment ago joins the queue.
    ///
    /// It starts level: `vruntime = V` is a lag of zero, so it is eligible at
    /// once and its deadline is the only thing deciding when it runs. That is
    /// deliberately simpler than Linux, which carries a task's lag across the
    /// sleep and hands it back on return. Carrying lag is what compensates a
    /// thread for service it was owed while it slept; dropping it costs some
    /// responsiveness for a thread that sleeps in short bursts, and buys
    /// immunity to the other side of it — a long sleeper returning with a large
    /// credit and monopolising a CPU until it is spent. The deadline already
    /// delivers most of the responsiveness: a waking thread is eligible
    /// immediately, and with a short request it is picked immediately.
    ///
    /// A migrated thread comes through here too, and must: its `vruntime` was
    /// on another CPU's virtual clock and means nothing against this one's.
    pub(crate) fn place(&mut self, thread: &Thread, request_ns: u64) {
        let v = self.avg_vruntime();
        self.vtime = cmp::max(self.vtime, v);
        let weight = weight_of(thread.priority());
        thread.vruntime.store(v, Ordering::Release);
        thread.vdeadline.store(
            v.saturating_add(virtual_delta(request_ns, weight)),
            Ordering::Release,
        );
    }

    /// Enqueue a thread that is already on this CPU's virtual clock, keeping the
    /// vruntime and deadline it has. This is a preempted thread going back to
    /// wait its turn, and re-placing it would forgive the time it just used.
    pub(crate) fn enqueue(&mut self, thread: Arc<Thread>) {
        self.link(thread);
    }

    /// Enqueue a thread arriving from a sleep, a block, or another CPU, placed
    /// against this queue's `V` for `request_ns` of service. See
    /// [`RunQueue::place`].
    pub(crate) fn enqueue_waking(&mut self, thread: Arc<Thread>, request_ns: u64) {
        self.place(&thread, request_ns);
        self.link(thread);
    }

    fn link(&mut self, thread: Arc<Thread>) {
        debug_assert!(
            !thread.rq_link.is_linked(),
            "runqueue::enqueue: thread {} already linked",
            thread.id.0
        );
        debug_assert!(
            thread.state() == State::Ready,
            "runqueue::enqueue: thread {} state {:?}, expected Ready",
            thread.id.0,
            thread.state()
        );

        let ptr = Arc::into_raw(thread) as *mut Thread;
        unsafe { self.queue.push_back(ptr) };
    }

    /// The eligible thread with the earliest virtual deadline.
    ///
    /// Two passes over the list: one for `V`, one for the winner. Eligibility is
    /// `vruntime <= V`, and among the eligible the earliest deadline wins, with
    /// the smaller vruntime breaking a tie so a queue of identical threads still
    /// rotates. The fallback matters and is not dead code: `V` is the *average*,
    /// so a queue can transiently have nobody at or below it only when it is
    /// empty, but a thread whose clock ran while it was not on this queue can
    /// leave the set of eligible threads empty for a pick, and refusing to
    /// choose would idle a CPU with runnable work on it.
    pub(crate) fn pick_next(&mut self) -> Option<Arc<Thread>> {
        if self.queue.is_empty() {
            return None;
        }
        let v = self.avg_vruntime();
        self.vtime = cmp::max(self.vtime, v);

        let mut best: Option<(*mut Thread, u64, u64)> = None;
        let mut earliest: Option<(*mut Thread, u64)> = None;
        for ptr in self.queue.iter() {
            let thread = unsafe { &*ptr };
            let vruntime = thread.vruntime.load(Ordering::Acquire);
            let vdeadline = thread.vdeadline.load(Ordering::Acquire);

            match earliest {
                Some((_, seen)) if seen <= vruntime => {}
                _ => earliest = Some((ptr, vruntime)),
            }

            if vruntime > v {
                continue;
            }
            match best {
                Some((_, best_deadline, best_vruntime))
                    if (best_deadline, best_vruntime) <= (vdeadline, vruntime) => {}
                _ => best = Some((ptr, vdeadline, vruntime)),
            }
        }

        let winner = best
            .map(|(ptr, _, _)| ptr)
            .or(earliest.map(|(ptr, _)| ptr))?;
        Some(self.unlink(winner))
    }

    /// The thread this CPU would miss least: the latest virtual deadline among
    /// those `allowed` accepts.
    ///
    /// The mirror of the pick, and for the same reason the old scheme took the
    /// lowest-priority tail — a steal should cost the victim as little as it
    /// can. Selecting under the predicate rather than popping and pushing back
    /// is what keeps a rejected candidate's position, and its deadline, intact.
    pub(crate) fn steal_victim(
        &mut self,
        allowed: impl Fn(&Thread) -> bool,
    ) -> Option<Arc<Thread>> {
        let mut best: Option<(*mut Thread, u64)> = None;
        for ptr in self.queue.iter() {
            let thread = unsafe { &*ptr };
            if !allowed(thread) {
                continue;
            }
            let vdeadline = thread.vdeadline.load(Ordering::Acquire);
            match best {
                Some((_, seen)) if seen >= vdeadline => {}
                _ => best = Some((ptr, vdeadline)),
            }
        }
        Some(self.unlink(best?.0))
    }

    fn unlink(&mut self, ptr: *mut Thread) -> Arc<Thread> {
        unsafe { self.queue.remove(ptr) };
        let thread = unsafe { Arc::from_raw(ptr as *const Thread) };
        debug_assert!(
            !thread.rq_link.is_linked(),
            "runqueue: thread {} still linked after removal",
            thread.id.0
        );
        thread
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn total_len(&self) -> usize {
        self.queue.len()
    }

    /// This queue's `V`, for reporting. Does not move the watermark.
    pub(crate) fn vtime(&self) -> u64 {
        self.vtime
    }
}
