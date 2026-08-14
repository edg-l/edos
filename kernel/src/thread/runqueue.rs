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
//!   taken from anyone else. Measured with `programs/latbench` against two CPU
//!   hogs: a thread asking for a quarter of the default slice took its p95 wake
//!   from 1008 us to 11 us, for 19 extra switches and no throughput taken from
//!   the hogs. `sched_setattr` is how a program asks.
//! - **Service owed survives a sleep.** A thread's lag is recorded when it stops
//!   being runnable and handed back when it is placed again, so a sleep neither
//!   forgives a debt nor forgets a credit. See [`RunQueue::place`].
//!
//! **Deliberately not Linux-shaped in two places.** Linux keys an RB-tree on
//! vruntime, augmented with subtree minimum deadlines, and maintains
//! `avg_vruntime` incrementally, because one of its runqueues can hold
//! thousands of threads. A runqueue here holds single digits and the pick it
//! replaces was already an O(16) walk of the priority buckets, so this scans a
//! single unsorted list and recomputes `V` in the same pass. That keeps `V` a
//! fact about the queue rather than a running sum every enqueue, dequeue and
//! steal has to remember to adjust — the same reason `Scheduler::load` is
//! derived.

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

/// The shortest slice a thread may ask for through `sched_setattr`.
///
/// [`LATENCY_SLICE`] is the shortest request the kernel makes on anyone's
/// behalf, so it is the floor userspace gets too: below it a thread would be
/// asking to be served sooner than an interrupt wake, and the arming cost the
/// [`BASE_SLICE`] derivation prices would start to show — at 250 us the ~1 us
/// APIC one-shot is 0.4% of the slice.
pub const MIN_SLICE: Duration = LATENCY_SLICE;

/// The longest slice a thread may ask for through `sched_setattr`.
///
/// A request is how long the holder runs before the next pick, so it is also
/// the delay it can add to everything that becomes runnable behind it. The
/// compositor's frame is ~13 ms; a ceiling under that keeps a thread asking for
/// the maximum from costing the desktop a frame on its own.
pub const MAX_SLICE: Duration = Duration::from_millis(10);

/// A slice request held to what the scheduler will serve.
pub fn clamp_slice(slice_ns: u64) -> u64 {
    slice_ns.clamp(MIN_SLICE.as_nanos() as u64, MAX_SLICE.as_nanos() as u64)
}

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

/// A lag held to one [`BASE_SLICE`] of virtual service in either direction.
///
/// **The bound must not be the thread's own request**, however natural that
/// reads. A placement sets `vdeadline = (V - lag) + request`, so a thread
/// carrying more credit than its request cancels the request out of its own
/// deadline: bound the lag at `request` and every such thread lands on `V`
/// exactly, whatever it asked for. Measured, that cost a thread asking for a
/// quarter-slice its entire latency advantage — p95 wake 1007 us, against 10 us
/// once the bound stopped moving with the request.
///
/// A fixed unit is also the truer quantity. Lag is service *owed*, and what a
/// thread is owed for a turn it did not get is set by how long it can be kept
/// waiting — one holder's slice — not by how much of that turn it would have
/// used.
fn clamp_lag(thread: &Thread, lag: i64) -> i64 {
    let limit = virtual_delta(BASE_SLICE.as_nanos() as u64, weight_of(thread.priority())) as i64;
    lag.clamp(-limit, limit)
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

    /// Remember what a thread leaving the runnable set was owed, so that a
    /// sleep neither forgives a debt nor forgets a credit.
    ///
    /// `lag = V - vruntime`: positive means under-served and owed service,
    /// negative means it ran past its share. Without this a thread is placed at
    /// `V` on return, which resets both — and the debt side is the one that
    /// gets abused, because the reset is free and repeatable. A thread that
    /// runs just under a slice and then sleeps briefly hands back its overrun
    /// every cycle, so it takes more than its weight against a competitor that
    /// simply stays runnable.
    ///
    /// Called for the outgoing thread once it is no longer runnable, with `V`
    /// taken over the queue it is leaving behind.
    pub(crate) fn record_lag(&mut self, thread: &Thread) {
        let v = self.avg_vruntime();
        self.vtime = cmp::max(self.vtime, v);
        let vruntime = thread.vruntime.load(Ordering::Acquire);
        let lag = (v as i64).saturating_sub(vruntime as i64);
        thread.vlag.store(clamp_lag(thread, lag), Ordering::Release);
    }

    /// Where a thread that was not runnable a moment ago joins the queue.
    ///
    /// It is placed at `V` less whatever [`RunQueue::record_lag`] saw it owed,
    /// so it returns as far behind or ahead of its share as it left. The lag is
    /// consumed here: a thread placed twice without having slept in between —
    /// a migration, a steal — is placed level the second time, because the
    /// credit belongs to the sleep and not to the move.
    ///
    /// **The clamp is what makes carrying lag safe**, and it is one slice of
    /// virtual service in either direction. A slice is the unit the scheduler
    /// grants in, so it is the most a thread can be owed for one turn it did
    /// not get; bounding it there is also what removes the need for a decay,
    /// since a sleeper of any length returns with at most one slice of credit
    /// rather than the unbounded head start an uncapped lag would give it.
    ///
    /// A migrated thread comes through here too, and must: its `vruntime` was
    /// on another CPU's virtual clock and means nothing against this one's.
    pub(crate) fn place(&mut self, thread: &Thread, request_ns: u64) {
        let v = self.avg_vruntime();
        self.vtime = cmp::max(self.vtime, v);
        let weight = weight_of(thread.priority());
        let lag = clamp_lag(thread, thread.vlag.swap(0, Ordering::AcqRel));
        let vruntime = (v as i64).saturating_sub(lag).max(0) as u64;
        thread.vruntime.store(vruntime, Ordering::Release);
        thread.vdeadline.store(
            vruntime.saturating_add(virtual_delta(request_ns, weight)),
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
