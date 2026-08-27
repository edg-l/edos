use crate::thread::context::restore_context_and_iretq;
use core::{
    cmp,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    time::Duration,
};

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use crossbeam_queue::ArrayQueue;
use heapless::{BinaryHeap, binary_heap::Min};
use spin::{Mutex, Once, RwLock};
use x86_64::{
    PrivilegeLevel, VirtAddr,
    instructions::interrupts::{disable, enable, enable_and_hlt, without_interrupts},
    registers::control::Cr3,
};

use x86_64::structures::paging::PageTableFlags;

use crate::trace_event;
use crate::{
    apic::{get_lapic, set_apic_timer, set_apic_timer_and_enable},
    boot::boot_info,
    drivers::fpu::{init_fpu_state, restore_fpu_state, save_fpu_state},
    interrupts::InterruptIndex,
    memory::{
        KTHREAD_STACK_REGION_SIZE, KTHREAD_STACK_SIZE, mapper::memory_mapper, valloc::vmalloc,
    },
    println, profile, smp,
    thread::{
        UserThreadInfo,
        context::CpuContext,
        irqlock::IrqSpinlock,
        preempt::{debug_assert_preemptible, preempt_enabled},
        runqueue::{BASE_SLICE, LATENCY_SLICE, RunQueue},
        sched_prof::{self, Stage},
        thread::{Flags, State, THREADS, Thread, ThreadId, get_thread_by_id, record_thread_exit},
        util::pick_sched_for,
    },
    timer::Instant,
    util::per_cpu::{get_percpu_data, read_fs_base, write_fs_base},
};

/// Timer ticks between two periodic rebalance attempts on one CPU.
///
/// Ticks, not milliseconds: the timer is a one-shot armed for whichever comes
/// first of the running thread's slice deadline and the next sleeper's expiry,
/// so a tick has no fixed period and this is not a fixed interval.
///
/// **One, because an imbalance that outlives the burst that caused it was never
/// corrected at all.** This was 10, described as "~50ms at 5ms timeslice" —
/// both halves of which had stopped being true, since the slice is a per-thread
/// request defaulting to [`BASE_SLICE`] and a tick is not the slice. What it
/// left was a correction rate of one thread per CPU per ~10 ms, against bursts
/// that are over in less. Measured with `balancebench crowd` on an 8-CPU boot,
/// median of three runs on a quiet host, where 2.00 is a perfectly spread
/// burst and 9.00 is the whole of one behind a single resident:
///
/// | interval | fanout |
/// |----------|--------|
/// | 10       | 3.99   |
/// | 4        | 3.16   |
/// | 1        | 2.36   |
///
/// The cost is a `SCHEDULERS` read and a walk of the registered CPUs, and it
/// did not show: a solo CPU-bound lump on those same boots read 4.40 ms at an
/// interval of 10 and 4.41 ms at 1. That walk is O(CPUs) under a read lock
/// though, and 8 is where this was measured — so this is the throttle to reach
/// for first if a much larger machine ever finds the scan on the tick path.
const REBALANCE_INTERVAL: u32 = 1;

/// How much more load the busiest CPU must carry before this one takes a thread
/// off it.
///
/// **Load, not threads.** It is [`Scheduler::load`] — the runqueue's length
/// plus whatever is running — so parked and sleeping threads weigh nothing and
/// this is a difference in *runnable* work.
///
/// Two rather than one because the quantity includes the running thread on both
/// sides: at a threshold of one, a CPU running a thread with an empty queue
/// (load 1) would steal from one running a thread with an empty queue and a
/// wake in flight (load 2), leaving the pair exactly as unbalanced as before
/// with a migration paid for. Two is the smallest difference that a move can
/// actually reduce.
///
/// Confirmed rather than assumed: at [`REBALANCE_INTERVAL`] of 1, dropping this
/// to one moved `balancebench crowd`'s fanout not at all (2.36 either way) and
/// only added the churn the paragraph above predicts.
const REBALANCE_THRESHOLD: u64 = 2;

/// CPUs halted in [`Scheduler::run_idle`], by dense CPU index.
///
/// A wake enqueues on the *waker's* CPU (see [`Scheduler::complete_wake`]), so a
/// thread that wakes several others buries them all in one runqueue however much
/// of the machine is asleep. Spreading them is work-stealing's job, and an idle
/// CPU used to learn there was anything to steal only when its own backoff poll
/// came round — up to a 100 ms halt per interval, and the interval grows. This
/// is the set an enqueue can poke instead.
///
/// Keyed like [`smp::online_cpu_mask`]: bit N is the CPU whose LAPIC id is
/// `smp::lapic_id_for_cpu(N)`, and only the first 64 CPUs are tracked, which is
/// the same ceiling the topology tables already have. A CPU past it never sets a
/// bit, is never poked, and keeps the poll.
static IDLE_CPU_MASK: AtomicU64 = AtomicU64::new(0);

/// A CPU with no bit in [`IDLE_CPU_MASK`]: it is never poked and keeps the poll.
const NOT_TRACKED: usize = usize::MAX;

/// CPUs tracked by [`IDLE_CPU_MASK`], bounded by its width.
const IDLE_TRACKED_CPUS: usize = u64::BITS as usize;

/// Take one CPU out of the idle set, or `None` when nobody is halted.
///
/// The claim *is* the message. Clearing the bit is what tells that CPU it was
/// asked for — nothing else clears another CPU's bit — and it also stops two
/// enqueues in a row from both shouting at the same CPU while others sleep.
fn claim_idle_cpu() -> Option<usize> {
    let mut mask = IDLE_CPU_MASK.load(Ordering::Acquire);
    while mask != 0 {
        let idx = mask.trailing_zeros() as usize;
        match IDLE_CPU_MASK.compare_exchange_weak(
            mask,
            mask & !(1 << idx),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(idx),
            Err(observed) => mask = observed,
        }
    }
    None
}

pub fn init() {
    println!("Initializing scheduler");
    // SAFETY: a read of this CPU's own LAPIC ID register. `apic::init` maps
    // and enables the LAPIC before any CPU brings its scheduler up.
    let lapic_id = unsafe { get_lapic().id() };
    let sched = Box::new(Scheduler::new(lapic_id));

    let ptr: &'static mut _ = Box::leak(sched);
    get_percpu_data().scheduler.set(ptr);
    let _ = SCHEDULERS.write().insert(lapic_id, ptr);
    println!("Saved scheduler on percpu");

    // Allocate a per-CPU scheduler stack. The voluntary context-switch
    // trampoline (save_transition_switch) pivots RSP here before calling
    // the transition fn and pick_and_run, so the outgoing thread's kernel
    // stack is completely free before any waker can resume it.
    let region = vmalloc(KTHREAD_STACK_REGION_SIZE)
        .expect("vmalloc: no address space for a scheduler stack");
    memory_mapper()
        .map_memory(
            region,
            KTHREAD_STACK_SIZE,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        )
        .expect("failed to map scheduler stack");
    let stack_top = region.as_u64() + KTHREAD_STACK_SIZE;
    get_percpu_data().scheduler_stack_top.set(stack_top);

    // Enable apic timer
    set_apic_timer_and_enable(Duration::from_millis(100));
}

#[inline(always)]
pub fn sched() -> &'static Scheduler {
    // SAFETY: `init` leaks a `Box<Scheduler>` into this CPU's slot and nothing
    // ever clears it, so once set the pointer is `'static` and non-null. A
    // migration between the GS-base read and the load answers with the other
    // CPU's scheduler, which is still a live `'static` one -- the risk here is
    // reading the wrong CPU's, never an invalid pointer. Callers that can run
    // before their CPU's `init` use `try_sched`.
    unsafe {
        get_percpu_data()
            .scheduler
            .get()
            .as_ref()
            .unwrap_unchecked()
    }
}

/// Like `sched()` but returns `None` before the per-CPU scheduler is installed.
/// Used by paths that run very early in boot (e.g. the lock-order tracker from
/// inside the frame allocator / heap init).
#[inline(always)]
pub fn try_sched() -> Option<&'static Scheduler> {
    // SAFETY: the slot holds either null or the leaked `'static` scheduler that
    // `init` put there, and `as_ref` turns the first of those into `None`.
    unsafe { get_percpu_data().scheduler.get().as_ref() }
}
pub static SCHEDULERS: RwLock<heapless::LinearMap<u32, &'static Scheduler, 128>> =
    RwLock::new(heapless::LinearMap::new());

fn sched_for_cpu(cpu: u32) -> &'static Scheduler {
    SCHEDULERS.read().get(&cpu).expect("cpu sched")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakePriority {
    Normal,
    Interrupt,
}

impl WakePriority {
    /// The service this wake asks for on the thread's behalf.
    ///
    /// An interrupt-priority wake asks for *less*, which is an earlier virtual
    /// deadline and so a sooner turn. The priority buckets this replaces said
    /// the same thing by lending the thread two levels, which also handed it a
    /// larger share of the CPU for as long as it stayed runnable — a latency
    /// request that quietly became a bandwidth one. A smaller request expires
    /// on its own: the next deadline comes from the thread's own slice.
    #[inline]
    fn request_ns(self, thread: &Thread) -> u64 {
        match self {
            WakePriority::Interrupt => LATENCY_SLICE.as_nanos() as u64,
            WakePriority::Normal => thread.request_ns(),
        }
    }
}

pub struct Scheduler {
    pub cpu: u32,

    /// This CPU's bit position in [`IDLE_CPU_MASK`], resolved once at
    /// registration. LAPIC ids are not contiguous on real hardware, so `cpu`
    /// cannot index a bitmask; this is the dense index the topology tables in
    /// `smp` already assign, and [`NOT_TRACKED`] marks a CPU past the 64 they
    /// hold.
    cpu_index: usize,

    // Ready queue. Simple round-robin with priority buckets is enough.
    // Keep it small and predictable.
    rq: Mutex<RunQueue>,

    // Current running thread id for this CPU.
    pub current: AtomicU64, // 0 means idle

    sleepers: Mutex<BinaryHeap<SleepEntry, Min, 1024>>,

    pub earliest_deadline: AtomicU64,

    /// When this CPU's one-shot APIC timer is currently set to fire, or 0 when
    /// nothing is armed because it already has.
    ///
    /// Only this CPU reads or writes it, always with interrupts off; the
    /// atomic is for the shared-reference API, not for sharing.
    armed_expiry: AtomicU64,

    /// When this CPU owes the profiler its next sample, or 0 for "at the next
    /// opportunity". Same ownership as `armed_expiry`: this CPU only.
    next_sample: AtomicU64,

    /// How many threads this CPU's runqueue holds, republished from the queue
    /// itself every time it is touched. See [`Scheduler::load`].
    queued: AtomicU64,

    has_work: AtomicBool,
    steal_count: AtomicU64,
    /// The subset of [`Scheduler::steal_count`] that [`Scheduler::try_rebalance`]
    /// took, as opposed to an idle CPU pulling work it had none of.
    ///
    /// Separate because the two answer different questions and only one of them
    /// has a dial. An idle steal is triggered by a poke and takes what it finds;
    /// a periodic one is the only thing that can move work between two CPUs that
    /// are *both* busy, and whether it ever fires is what
    /// [`REBALANCE_THRESHOLD`] and [`REBALANCE_INTERVAL`] decide.
    rebalance_count: AtomicU64,
    steal_scan_start: AtomicU32,
    rebalance_tick: AtomicU32,

    /// Context switches this CPU has performed.
    ///
    /// The price side of every latency decision the scheduler makes, and the
    /// only place the guest can see it: a shorter slice or a keener preemption
    /// rule buys its shorter wait here and nowhere else. `programs/latbench`
    /// reports it as a delta over a known workload, through `/proc/sched`.
    ///
    /// Last, and deliberately: it is written on every switch and read only by
    /// a `/proc` reader, so it belongs behind the fields the switch path reads
    /// rather than in the middle of them.
    switches: AtomicU64,
}

impl Scheduler {
    fn enqueue_ready(sc: &Scheduler, thread: &Arc<Thread>, priority: WakePriority) {
        without_interrupts(|| {
            debug_assert!(
                !thread.rq_link.is_linked(),
                "enqueue_ready: thread {} already linked on runqueue",
                thread.id.0
            );
            let state = thread.state();
            debug_assert!(
                state == State::Ready,
                "enqueue_ready: thread {} in state {:?}, expected Ready",
                thread.id.0,
                state
            );
            let request = priority.request_ns(thread);
            sc.with_rq(|rq| {
                rq.enqueue_waking(thread.clone(), request);
            });
            sc.has_work.store(true, Ordering::Release);
            trace_event!(Enqueue {
                cpu: sc.cpu,
                tid: thread.id.0
            });
            sc.mark_running_thread_need_resched();
            // Two different duties, and they need different conditions.
            // `wake_if_idle` wakes the CPU this thread was just put on, which
            // matters most when that CPU has nothing else -- exactly the case
            // `poke_idle_cpu` declines. `poke_idle_cpu` recruits a *second*
            // CPU to steal, which is only worth an IPI when there is surplus.
            sc.wake_if_idle();
            sc.poke_idle_cpu();
        })
    }

    /// Poke this CPU because a thread was just enqueued on it, if it has
    /// published itself idle.
    ///
    /// `poke_idle_cpu` cannot serve here: it declines when `load() < 2`, and
    /// a single thread woken onto an idle CPU is the case that needs the IPI
    /// most. `load()`'s own contract says as much -- a stale count costs a
    /// balance decision a slightly worse placement, but costs a wakeup a
    /// thread nobody runs.
    ///
    /// The fence pairs with the one in `run_idle` between `publish_idle` and
    /// its re-check of `queued`. Between them they make the two orders
    /// exclusive: either this sees the idle bit and sends the IPI, or the
    /// idling CPU sees the enqueue and does not halt. Without it both sides
    /// can read stale and the thread waits for the 100 ms idle fallback timer.
    fn wake_if_idle(&self) {
        if self.cpu_index == NOT_TRACKED {
            return;
        }
        core::sync::atomic::fence(Ordering::SeqCst);
        let bit = 1u64 << self.cpu_index;
        if IDLE_CPU_MASK.fetch_and(!bit, Ordering::AcqRel) & bit == 0 {
            // Not idle: it is running something and will reschedule itself.
            return;
        }
        if self.cpu != get_percpu_data().lapic_id.get() {
            self.send_reschedule_ipi(self.cpu);
        }
    }

    fn complete_wake(&self, thread: &Arc<Thread>, priority: WakePriority) {
        let probe = sched_prof::now_ns();
        without_interrupts(|| {
            // Enqueue on the waker's CPU for cache locality. Safe because
            // save_transition_switch pivots to the per-CPU scheduler stack
            // before publishing the thread, so the thread's kernel stack is
            // free when any CPU resumes it.
            //
            // Locality loses to affinity: a thread pinned away from the waker
            // would sit in a runqueue whose CPU never picks it, and only a
            // steal by an allowed CPU would rescue it.
            let target: &Scheduler = if self.thread_can_run_here(thread) {
                self
            } else {
                pick_sched_for(thread).unwrap_or(self)
            };
            // Update thread.cpu so wake_thread_slow's Ready arm IPIs the
            // correct CPU.
            thread.cpu.store(target.cpu, Ordering::Release);
            thread.state.store(State::Ready as u8, Ordering::Release);
            Self::enqueue_ready(target, thread, priority);
        });
        sched_prof::record(Stage::WakeEnqueue, probe);
    }

    pub fn new(cpu: u32) -> Self {
        // Runs on the CPU it describes, and after `smp` has registered it, so
        // the index this resolves to is that CPU's own.
        let cpu_index = smp::current_cpu_index();
        debug_assert_eq!(
            smp::lapic_id_for_cpu(cpu_index),
            cpu,
            "scheduler cpu index {cpu_index} does not name lapic {cpu}"
        );
        Self {
            cpu,
            cpu_index: if cpu_index < IDLE_TRACKED_CPUS {
                cpu_index
            } else {
                NOT_TRACKED
            },
            rq: Mutex::new(RunQueue::new()),
            current: AtomicU64::new(0),
            sleepers: Mutex::new(BinaryHeap::new()),
            queued: AtomicU64::new(0),
            earliest_deadline: AtomicU64::new(u64::MAX),
            armed_expiry: AtomicU64::new(0),
            next_sample: AtomicU64::new(0),
            has_work: AtomicBool::new(false),
            steal_count: AtomicU64::new(0),
            rebalance_count: AtomicU64::new(0),
            steal_scan_start: AtomicU32::new(0),
            rebalance_tick: AtomicU32::new(0),
            switches: AtomicU64::new(0),
        }
    }

    /// Run `f` against this CPU's runqueue and republish `queued` from the
    /// queue afterwards.
    ///
    /// Every access to `rq` goes through this or [`Scheduler::with_try_rq`],
    /// which is what makes `queued` a fact about the queue rather than a tally
    /// somebody has to remember to adjust. A steal needs no bookkeeping at all
    /// under that rule: the pop lowers the victim and the enqueue raises the
    /// thief, because both go through here.
    fn with_rq<R>(&self, f: impl FnOnce(&mut RunQueue) -> R) -> R {
        let mut rq = self.rq.lock();
        let out = f(&mut rq);
        self.queued.store(rq.total_len() as u64, Ordering::Release);
        out
    }

    /// [`Scheduler::with_rq`] against a runqueue this CPU does not own, giving
    /// up rather than waiting. `None` means the lock was held.
    fn with_try_rq<R>(&self, f: impl FnOnce(&mut RunQueue) -> R) -> Option<R> {
        let mut rq = self.rq.try_lock()?;
        let out = f(&mut rq);
        self.queued.store(rq.total_len() as u64, Ordering::Release);
        Some(out)
    }

    /// Runnable work on this CPU: what is waiting in the runqueue, plus the
    /// thread running now.
    ///
    /// This is what placement and rebalancing balance, and it deliberately
    /// counts *runnable* threads rather than the threads that call this CPU
    /// home. A parked or sleeping thread is in no runqueue and is not running,
    /// so it weighs nothing — which is the point. Counting membership instead
    /// lets a CPU whose threads are all parked look like the busiest one in the
    /// machine and repel every new thread from it.
    ///
    /// Read without a lock and therefore already stale by the time it is used.
    /// That is fine here in a way it is not for a wakeup: a balance decision
    /// taken against a count one tick old is a slightly worse placement, not a
    /// thread nobody runs.
    pub fn load(&self) -> u64 {
        self.queued.load(Ordering::Acquire) + (self.current.load(Ordering::Acquire) != 0) as u64
    }

    /// Threads waiting in this CPU's runqueue, without the one running.
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Acquire)
    }

    /// This CPU's virtual clock: the point a thread on it has been served
    /// exactly its share up to. Only comparable against itself — two CPUs'
    /// clocks advance independently, which is why a migrated thread is
    /// re-placed rather than carried over.
    pub fn vtime(&self) -> u64 {
        self.rq.lock().vtime()
    }

    /// Threads this CPU has taken from another's runqueue.
    pub fn steals(&self) -> u64 {
        self.steal_count.load(Ordering::Relaxed)
    }

    /// The subset of [`Scheduler::steals`] taken by periodic rebalancing, which
    /// is the only path that can move work between two busy CPUs.
    pub fn rebalances(&self) -> u64 {
        self.rebalance_count.load(Ordering::Relaxed)
    }

    /// Context switches this CPU has performed.
    pub fn switches(&self) -> u64 {
        self.switches.load(Ordering::Relaxed)
    }

    /// The thread this CPU's scheduler last published as running, or `None`
    /// when the CPU is idle.
    ///
    /// Scheduler-internal: it answers "what is *this scheduler's* CPU running",
    /// which is only the caller's own identity when `self` was resolved in a
    /// context that cannot migrate. Everything outside the scheduler wants the
    /// free `current_thread_id`.
    fn running_tid(&self) -> Option<ThreadId> {
        let tid = self.current.load(Ordering::Acquire);
        if tid == 0 {
            return None;
        }
        Some(ThreadId(tid))
    }

    /// First half of a timer tick, run on the interrupted thread's stack.
    ///
    /// Returns the per-CPU scheduler stack for the caller to pivot to, or 0 to
    /// stay put. A non-zero return means the outgoing thread's context has been
    /// saved and `tick_finish` is about to publish it — enqueue it, or go idle
    /// and let a stealer take it. Publishing is the moment another CPU may
    /// resume that thread, and it resumes on its own kernel stack, which is the
    /// stack this tick is standing on. So the CPU has to leave first: the
    /// voluntary path does the same pivot in `save_transition_switch`, and for
    /// the same reason.
    ///
    /// A tick that arrives while this CPU is already idle needs no pivot. It is
    /// on the scheduler stack, and no thread owns that.
    pub fn tick_prepare(&self, context: *mut CpuContext) -> u64 {
        without_interrupts(|| {
            // Before anything else this tick does, so the frame the sample
            // reads is the interrupted one and no scheduler frame has been
            // pushed on top of it.
            self.maybe_sample(context);

            // The one-shot counted down to zero and stopped, so whatever this
            // CPU last armed is gone and the next request must reach the
            // hardware rather than trusting the record of it.
            self.timer_fired();
            self.wake_sleepers();
            self.try_rebalance();

            // Idle with an empty runqueue: nothing to do but re-arm and halt
            // again. Recursing into run_idle here would nest a second idle
            // loop under the first (on_tick -> pick_and_run -> run_idle ->
            // enable IRQs -> on_tick -> ...).
            let idle = self.current.load(Ordering::Acquire) == 0;
            if idle && !self.has_work.load(Ordering::Acquire) {
                return 0;
            }
            if idle {
                // Work arrived while halted. `tick_finish` pops it; the frame
                // is already on this CPU's own stack.
                return 0;
            }

            self.expire_timeslice();

            // A spin lock is held here. Switching away would leave every CPU
            // waiting on it spinning until this thread is scheduled again.
            // `NEED_RESCHED` stays set, so the next tick performs the switch.
            if !preempt_enabled() {
                return 0;
            }
            let Some(cur) = current_thread() else {
                return 0;
            };
            if cur.flags.load(Ordering::Acquire) & Flags::NEED_RESCHED.bits() == 0 {
                return 0;
            }
            cur.flags
                .fetch_and(!Flags::NEED_RESCHED.bits(), Ordering::AcqRel);

            // Save context BEFORE enqueue so work-stealers see valid ctx.
            // Without this, a stealer could pop the thread and read stale
            // register state (the ctx from the thread's last save, not the
            // current interrupt frame).
            self.save_current_thread(context);
            get_percpu_data().scheduler_stack_top.get()
        })
    }

    /// Second half of a timer tick. `pivoted` is what `tick_prepare` returned,
    /// so `context` points into the per-CPU scheduler stack when it is true.
    pub fn tick_finish(&self, context: *mut CpuContext, pivoted: bool) {
        without_interrupts(|| {
            if pivoted {
                // Off the outgoing thread's stack now, so it is safe to let
                // another CPU have it.
                if let Some(cur) = current_thread()
                    && cur.cas_state(State::Running, State::Ready)
                {
                    debug_assert!(
                        !cur.rq_link.is_linked(),
                        "tick_finish: thread {} already linked before re-enqueue",
                        cur.id.0
                    );
                    self.with_rq(|rq| rq.enqueue(cur.clone()));
                    self.has_work.store(true, Ordering::Release);
                    // A preemption is an enqueue like any other: two threads
                    // taking turns on one CPU are two runnable threads, and
                    // nothing else tells a halted CPU that the pair exists.
                    self.poke_idle_cpu();
                }
                self.pick_and_run(context);
                return;
            }

            if self.current.load(Ordering::Acquire) == 0 {
                if self.has_work.load(Ordering::Acquire) {
                    self.pick_and_run(context);
                }
                return;
            }

            // Neither preempted nor went idle (single thread running, no work
            // to steal), so nothing else re-armed the one-shot APIC timer and
            // it would stay dead. context_switch_to arms it when switching
            // threads and run_idle arms it while halted.
            //
            // This is also where a tick that fired early lands, since
            // `context_switch_to` skips the write whenever an earlier timer is
            // already pending: the running thread keeps the rest of the slice
            // it was given, and the next stretch of it is armed here.
            let now = Instant::now();
            let ed = self.earliest_deadline.load(Ordering::Acquire);
            // Re-arm to the sooner of: what is left of the running thread's
            // slice, or the earliest sleeper deadline.
            let running_until = current_thread()
                .map(|cur| cur.slice_deadline.load(Ordering::Acquire))
                .filter(|deadline| *deadline > now.as_nanos())
                .map(Instant::from_nanos);
            let mut next = running_until.unwrap_or(now + BASE_SLICE);
            if ed != u64::MAX && ed != 0 {
                let dl = Instant::from_nanos(ed);
                if dl < next {
                    next = dl;
                }
            }
            self.arm_timer_until(now, next);
        })
    }

    /// Enqueue every sleeper whose deadline has passed, and drop the entries of
    /// threads that are no longer sleeping.
    ///
    /// A sleeper goes into the heap of the CPU it was running on and comes back
    /// out onto that CPU's runqueue, so a thread that sleeps in a loop keeps
    /// whichever CPU it first slept on for as long as it lives — unlike a park,
    /// which lands on the waker's CPU and so follows the work. Nothing else
    /// moves it: work-stealing only reaches threads that are already queued.
    /// Going through [`Scheduler::enqueue_ready`] is what makes the expiry
    /// visible to a halted CPU, which is the one thing that can take the
    /// sleeper somewhere else.
    fn wake_sleepers(&self) {
        let now = Instant::now().as_nanos();
        let mut sl = self.sleepers.lock();
        // Drain stale and expired entries from the heap.
        // Stale = not Sleeping (already woken, died, etc.).
        // We drain all stale entries at the top first, then process expired ones,
        // continuing to skip stale entries encountered along the way.
        while let Some(top) = sl.peek() {
            // Drain stale entries regardless of deadline.
            if top.thread.state() != State::Sleeping {
                sl.pop();
                continue;
            }
            // First non-stale entry with a future deadline: we're done.
            if top.deadline > now {
                break;
            }
            let t = sl.pop().unwrap().thread;
            if t.try_wake() {
                t.state.store(State::Ready as u8, Ordering::Release);
                Self::enqueue_ready(self, &t, WakePriority::Normal);
            }
        }
        if let Some(next_sleep) = sl.peek() {
            self.earliest_deadline
                .store(next_sleep.deadline, Ordering::Release);
        } else {
            self.earliest_deadline.store(u64::MAX, Ordering::Release);
        }
    }

    fn get_thread_by_id(&self, id: ThreadId) -> Option<Arc<Thread>> {
        get_thread_by_id(id)
    }

    /// Enter the idle loop. Returns true if a work-steal context switch
    /// was performed (caller must return immediately). Returns false when
    /// local work appeared and the caller should pop from its own runqueue.
    fn run_idle(&self, context: *mut CpuContext) -> bool {
        // Mark CPU idle
        self.current.store(0, Ordering::Release);
        // SAFETY: `run_idle` is reached from the scheduler with interrupts off,
        // so the store lands on the CPU whose GS base named the slot, and no
        // `with_current_thread` borrow is outstanding across it.
        unsafe { get_percpu_data().set_current_thread(None) };

        // Switch to the kernel page table so we don't idle with a stale
        // user-process CR3 whose PML4 may be freed by another CPU.
        switch_to_kernel_page();

        self.has_work.store(false, Ordering::Release);
        enable();

        let mut idle_ticks: u32 = 0;
        let mut steal_backoff: u32 = 0;
        let mut poked = false;

        loop {
            // Break out if any work is available on our own runqueue.
            if self.has_work.load(Ordering::Acquire) {
                break;
            }
            #[cfg(feature = "stall-dump")]
            crate::debug::stall::poll();

            // Poll for stealable work on an exponential backoff, so an idle CPU
            // that finds nothing keeps halting rather than spinning — unless it
            // was asked for, which the backoff must not sit on. The poll is the
            // backstop for a claim that raced, not the mechanism.
            let steal_interval = 1u32 << cmp::min(steal_backoff, 4);
            if poked || idle_ticks.is_multiple_of(steal_interval) {
                disable();
                if self.try_steal_and_run(context) {
                    return true;
                }
                // Being asked is evidence the machine has work, so a CPU that
                // was poked and found nothing starts its backoff over rather
                // than letting the next ask land inside a 16-tick sleep.
                steal_backoff = if poked {
                    0
                } else {
                    steal_backoff.saturating_add(1)
                };
                enable();
            }

            without_interrupts(|| {
                let now = Instant::now();
                let ed = self.earliest_deadline.load(Ordering::Acquire);
                let next = if ed != u64::MAX && ed != 0 {
                    Instant::from_nanos(ed)
                } else {
                    now + Duration::from_millis(100)
                };
                self.arm_timer_until(now, next);
            });

            // Halt until next interrupt (timer, IPI, device). `sti; hlt` is one
            // unit against a claim that lands in between: an IPI raised while
            // interrupts were off is delivered after the `hlt` begins rather
            // than before it, so the wakeup cannot be missed.
            self.publish_idle();
            // Re-check *after* publishing, and against `queued` rather than
            // `has_work`: the flag was cleared on the way in here and an
            // enqueue that raced that clear has already lost it, whereas the
            // runqueue count is authoritative. Skipping this is a 100 ms
            // stall -- the fallback timer above is the only thing that would
            // then notice the work.
            core::sync::atomic::fence(Ordering::SeqCst);
            if self.queued() > 0 {
                self.has_work.store(true, Ordering::Release);
                self.take_idle();
                continue;
            }
            x86_64::instructions::interrupts::enable_and_hlt();
            poked = !self.take_idle();

            idle_ticks += 1;
        }

        disable();
        false
    }

    /// Program the one-shot APIC timer to fire no later than `deadline`, and
    /// skip the hardware write when what is already armed will do.
    ///
    /// The write is the single most expensive thing on the switch path — one
    /// x2APIC store to `IA32_TSC_TMICT` that a hypervisor traps and answers by
    /// re-arming a host timer, measured at 1024 ns of a 1270 ns switch — and
    /// `context_switch_to` used to make it every time to push the incoming
    /// thread's slice out. It rarely bought anything: a timer already set to
    /// fire *earlier* than the new deadline satisfies it, because firing early
    /// is not a failure. `expire_timeslice` compares each thread against its
    /// own recorded deadline and lets an early tick pass, and `tick_finish`
    /// re-arms when a tick decided not to switch, so the thread still gets the
    /// whole slice; it just takes one extra tick to notice. What must never
    /// happen is the timer firing *late*, and that is exactly what the upper
    /// bound below refuses to skip.
    ///
    /// Yielding in a loop therefore costs one interrupt per timeslice instead
    /// of one trap per switch. This is what a tickless kernel's clock-event
    /// layer does, and why Linux ships `HRTICK` — an hrtimer armed at the exact
    /// slice end — turned off.
    fn arm_timer_until(&self, now: Instant, deadline: Instant) {
        let deadline = self.clamp_to_sample(now, deadline);
        let armed = self.armed_expiry.load(Ordering::Relaxed);
        if armed > now.as_nanos() && armed <= deadline.as_nanos() {
            return;
        }
        // Record what the hardware was actually given, not what was asked for:
        // a deadline inside the floor fires later than `deadline`, and
        // believing otherwise would let the next request skip a write it needs.
        let armed = set_apic_timer(deadline.duration_since(now));
        self.armed_expiry.store(
            now.as_nanos().saturating_add(armed.as_nanos() as u64),
            Ordering::Relaxed,
        );
    }

    /// Record that the one-shot has fired and left itself disarmed, so the
    /// next request programs the hardware rather than trusting a dead timer.
    fn timer_fired(&self) {
        self.armed_expiry.store(0, Ordering::Relaxed);
    }

    /// Bring `deadline` forward to this CPU's next profile sample.
    ///
    /// The profiler needs the tick to arrive on *its* period, and this kernel
    /// has one timer per CPU already spoken for. Rather than a second clock
    /// source, a sample deadline is simply another thing the one-shot must not
    /// fire after — which is what this function already exists to express, so
    /// the skip-the-write rule above keeps working unchanged.
    fn clamp_to_sample(&self, now: Instant, deadline: Instant) -> Instant {
        if !profile::enabled() {
            return deadline;
        }
        let due = self.next_sample.load(Ordering::Relaxed).max(now.as_nanos());
        if due < deadline.as_nanos() {
            Instant::from_nanos(due)
        } else {
            deadline
        }
    }

    /// Take a profile sample if this CPU's period has elapsed.
    ///
    /// The next deadline is measured from now rather than advanced by a
    /// period: a CPU halted through several periods owes one sample, not the
    /// backlog, and charging it the backlog would spend the whole ring on a
    /// machine that was idle.
    fn maybe_sample(&self, context: *mut CpuContext) {
        if !profile::enabled() {
            return;
        }
        let now = Instant::now().as_nanos();
        if now < self.next_sample.load(Ordering::Relaxed) {
            return;
        }
        profile::take_sample(context);
        self.next_sample
            .store(now.saturating_add(profile::period_ns()), Ordering::Relaxed);
    }

    /// Request a reschedule once the running thread has used its timeslice.
    ///
    /// `context_switch_to` arms the APIC timer to `slice_deadline`, so the tick
    /// that observes an elapsed deadline ends the slice. Enqueue and wake are
    /// the only other sources of a preemption request, and neither fires for a
    /// thread that simply keeps running, so without this a CPU-bound thread
    /// holds its CPU until something else happens to become runnable there.
    fn expire_timeslice(&self) {
        let Some(cur) = current_thread() else {
            return;
        };
        let deadline = cur.slice_deadline.load(Ordering::Acquire);
        if deadline != 0 && Instant::now().as_nanos() >= deadline {
            cur.mark_need_resched();
        }
    }

    fn pick_and_run(&self, context: *mut CpuContext) {
        // NOTE: save_current_thread is NOT called here. It is called by
        // maybe_preempt (timer/IPI preemption path) BEFORE enqueue, and by
        // do_save_current_thread (voluntary path via save_transition_switch)
        // BEFORE the transition fn runs. Calling save here would create a
        // double-save race: between the enqueue and this second save, a
        // stealer could grab the thread and start running it. The second
        // save would then overwrite the stolen thread's ctx with THIS CPU's
        // interrupt frame, corrupting it.
        //
        // The outgoing thread's clock is already charged by then — both save
        // paths call `end_run` first — so this is the last point at which what
        // it was owed can still be read against the queue it is leaving.
        self.record_outgoing_lag();
        loop {
            let probe = sched_prof::now_ns();
            let next = self.with_rq(|rq| {
                let item = rq.pick_next();
                self.has_work.store(!rq.is_empty(), Ordering::Release);
                item
            });
            sched_prof::record(Stage::Pick, probe);

            match next {
                Some(t) => {
                    debug_assert!(
                        !t.rq_link.is_linked(),
                        "pick_and_run: thread {} still linked after pop",
                        t.id.0
                    );
                    if t.cas_state(State::Ready, State::Running) {
                        // SAFETY: `context` is this CPU's live interrupt frame,
                        // checked on entry; `t` has just been popped off this
                        // CPU's runqueue and won the Ready -> Running CAS, so no
                        // other CPU can also be switching to it. Interrupts are
                        // off for the whole of `pick_and_run`.
                        unsafe { self.context_switch_to(t, context) };
                        return;
                    } else {
                        let state = t.state();
                        debug_assert!(
                            state != State::Dying,
                            "pick_and_run: popped Dying thread {}",
                            t.id.0
                        );
                        continue; // invalid state, try again
                    }
                }
                None => {
                    if self.run_idle(context) {
                        // Work-steal context switch already done.
                        return;
                    }
                    // Local work appeared, loop and pop from our rq.
                    continue;
                }
            }
        }
    }

    fn thread_can_run_here(&self, t: &Thread) -> bool {
        t.allows_cpu(self.cpu)
    }

    /// Ask whatever this scheduler's CPU is running to reschedule.
    ///
    /// The waker's own CPU is the common case by construction: `complete_wake`
    /// enqueues on it for cache locality, and `spawn_thread`, `wake_sleepers`
    /// and the steal paths all mark the CPU they are running on. That thread is
    /// already in this CPU's own slot, so reaching it is a field read; going
    /// through the registry instead is an `RwLock` read, a `BTreeMap` walk and
    /// an `Arc` clone, on a path a wake takes every time.
    ///
    /// The remote case keeps the lookup, and every caller that has one also
    /// sends a reschedule IPI, which is what actually makes that CPU look.
    fn mark_running_thread_need_resched(&self) {
        without_interrupts(|| {
            let cpu = get_percpu_data();
            if self.cpu == cpu.lapic_id.get() {
                cpu.with_current_thread(|t| t.mark_need_resched());
                return;
            }
            if let Some(current_tid) = self.running_tid()
                && let Some(current_thread) = self.get_thread_by_id(current_tid)
            {
                current_thread.mark_need_resched();
            }
        })
    }

    /// Remember what the outgoing thread was owed before another takes the CPU.
    ///
    /// Only for a thread that is leaving the runnable set: one that is `Ready`
    /// has been re-enqueued and keeps the clock it already has, and re-recording
    /// it there would hand a thread its own lag back on every preemption. See
    /// [`RunQueue::record_lag`].
    /// Reached through the per-CPU slot rather than `current_thread`, which
    /// hands back an `Arc`: this runs on every switch, and the common answer is
    /// "still runnable, nothing to record", so a refcount pair either side of
    /// that answer is the whole cost of asking.
    fn record_outgoing_lag(&self) {
        get_percpu_data().with_current_thread(|cur| {
            if matches!(cur.state(), State::Ready | State::Running | State::Waking) {
                return;
            }
            self.with_rq(|rq| rq.record_lag(cur));
        });
    }

    /// Panic if `ctx` could not have come from `thread`.
    ///
    /// A saved frame returning to ring 0 must resume on that thread's own
    /// kernel stack; anything else becomes the CPU's `RSP` at the next `iretq`
    /// and faults somewhere with no trace of who wrote it. `where_` names the
    /// side that noticed, so a bad frame at restore-but-not-save means a
    /// concurrent writer between the two.
    #[cfg(debug_assertions)]
    fn validate_ctx(thread: &Thread, ctx: &CpuContext, where_: &str) {
        let frame = &ctx.interrupt_stack_frame;
        let rsp = frame.stack_pointer.as_u64();
        if frame.code_segment.rpl() == PrivilegeLevel::Ring3 {
            return;
        }
        let top = thread.kstack_top;
        let bottom = top.saturating_sub(KTHREAD_STACK_SIZE);
        assert!(
            rsp == 0 || (rsp > bottom && rsp <= top),
            "{}: thread {} (name={}) has kernel RSP {:#x} outside its stack {:#x}..{:#x}, \
             rip={:#x} cs={:#x} context_saved={}",
            where_,
            thread.id.0,
            thread.name,
            rsp,
            bottom,
            top,
            frame.instruction_pointer.as_u64(),
            frame.code_segment.0,
            thread.context_saved.load(Ordering::Relaxed),
        );
    }

    fn save_current_thread(&self, context: *mut CpuContext) {
        if let Some(current) = current_thread() {
            // If the thread was stolen and is now running on another CPU,
            // don't overwrite its ctx -- the new CPU owns it. Without this
            // check, a "double save" could write THIS CPU's interrupt frame
            // (belonging to a different thread) into the stolen thread's ctx,
            // corrupting it. The stealer's context_switch_to updates thread.cpu
            // before reading ctx, so this check is sufficient.
            if current.cpu.load(Ordering::Acquire) != self.cpu {
                return;
            }
            let end_ns = Instant::now().as_nanos();
            current.end_run(end_ns);
            // SAFETY: `context` is the interrupt frame the entry stub pushed on
            // this CPU's stack, validated by `check_context` on the way in, so
            // it is a live aligned `CpuContext`. `current.fpu` is an
            // `UnsafeCell` touched only from the CPU its thread is on, with
            // interrupts off; the `current.cpu` check just above proved that is
            // still this CPU, so no stealer is writing it concurrently.
            unsafe {
                #[cfg(debug_assertions)]
                Self::validate_ctx(&current, &*context, "save_current_thread");
                *current.ctx.lock() = (*context).clone();
                if current.user.is_some() {
                    let fpu = &mut *current.fpu.get();
                    if !current.fpu_init.load(Ordering::Relaxed) {
                        init_fpu_state(fpu);
                        current.fpu_init.store(true, Ordering::Relaxed);
                    } else {
                        save_fpu_state(fpu);
                    }
                }
            }

            let fs_base = read_fs_base();
            current.tls_base.store(fs_base.as_u64(), Ordering::Release);
            current.context_saved.store(true, Ordering::Release);
            trace_event!(Save {
                cpu: self.cpu,
                tid: current.id.0,
                // SAFETY: the frame `check_context` validated on entry.
                rip: unsafe {
                    (*context)
                        .interrupt_stack_frame
                        .instruction_pointer
                        .as_u64()
                },
            });
        }
    }

    /// Install `next` as this CPU's running thread by overwriting the
    /// interrupt frame at `context` with its saved one.
    ///
    /// # Safety
    /// `context` must point at the interrupt frame this CPU is about to
    /// `iretq` from, and the caller must run with interrupts disabled on the
    /// CPU this `Scheduler` belongs to. `next` must have been claimed by this
    /// CPU -- won out of a runqueue or a state transition -- so that no other
    /// CPU is switching to it at the same time, and the current thread's
    /// context must already have been saved.
    unsafe fn context_switch_to(&self, next: Arc<Thread>, context: *mut CpuContext) {
        debug_assert!(
            !next.rq_link.is_linked(),
            "context_switch_to: thread {} rq_link still linked",
            next.id.0
        );
        debug_assert_eq!(
            next.state(),
            State::Running,
            "context_switch_to: thread {} not Running",
            next.id.0
        );
        #[cfg(feature = "stall-dump")]
        crate::debug::stall::note_switch(next.id.0);

        // Set as current. Update cpu FIRST so the old CPU's
        // save_current_thread sees the migration and bails out.
        #[cfg(feature = "trace")]
        {
            let old_tid = self.current.load(Ordering::Relaxed);
            trace_event!(Switch {
                cpu: self.cpu,
                from_tid: old_tid,
                to_tid: next.id.0,
                to_rip: next
                    .ctx
                    .lock()
                    .interrupt_stack_frame
                    .instruction_pointer
                    .as_u64(),
            });
        }
        let entry = sched_prof::now_ns();
        self.switches.fetch_add(1, Ordering::Relaxed);
        self.current.store(next.id.0, Ordering::Release);
        // SAFETY: `context_switch_to` runs with interrupts off, so the store
        // lands on the CPU whose GS base named the slot, and the previous
        // thread's entry was dropped before this replaces it.
        unsafe { get_percpu_data().set_current_thread(Some(next.clone())) };
        next.cpu.store(self.cpu, Ordering::Release);
        next.context_saved.store(false, Ordering::Release);

        let now = Instant::now();
        next.begin_run(now.as_nanos());
        // The slice is what this thread asked for, not a quantum the
        // scheduler hands out: see `thread::runqueue`.
        let mut deadline = now + Duration::from_nanos(next.request_ns());

        let earliest_deadline = self.earliest_deadline.load(Ordering::Acquire);

        if earliest_deadline < deadline.as_nanos() {
            deadline = Instant::from_nanos(earliest_deadline);
        } else {
            self.earliest_deadline
                .store(deadline.as_nanos(), Ordering::Release);
        }

        next.slice_deadline
            .store(deadline.as_nanos(), Ordering::Release);
        let probe = sched_prof::record(Stage::Publish, entry);
        self.arm_timer_until(now, deadline);
        let probe = sched_prof::record(Stage::Timer, probe);

        if context.is_null() {
            panic!("cw: null context ptr");
        }

        if (context as u64) < 0xFFFF_0000_0000_0000u64 {
            panic!("cw: Low context address {context:p}");
        }

        if !context.is_aligned() {
            panic!("cw: Misaligned context: {context:p}");
        }
        let ctx_snapshot = next.ctx.lock().clone();
        debug_assert!(
            {
                let rip = ctx_snapshot
                    .interrupt_stack_frame
                    .instruction_pointer
                    .as_u64();
                rip == 0 || rip >= 0x1000
            },
            "context_switch_to: thread {} (name={}) has bad RIP, context_saved={}",
            next.id.0,
            next.name,
            next.context_saved.load(Ordering::Relaxed),
        );
        #[cfg(debug_assertions)]
        Self::validate_ctx(&next, &ctx_snapshot, "context_switch_to");
        let user_rsp = ctx_snapshot.interrupt_stack_frame.stack_pointer.as_u64();
        // SAFETY: `context` points at the interrupt frame this CPU is about to
        // `iretq` from, validated by `check_context` at every entry. Writing the
        // next thread's saved frame over it is what performs the switch.
        unsafe { *context = ctx_snapshot };
        let probe = sched_prof::record(Stage::RestoreCtx, probe);

        // Switch address space
        next.switch_to_page();
        let probe = sched_prof::record(Stage::Page, probe);

        let next_fs_base = next.tls_base.load(Ordering::Acquire);

        write_fs_base(VirtAddr::new(next_fs_base));
        let probe = sched_prof::record(Stage::RestoreTls, probe);

        // The registers may already hold this thread's FPU state, in which case
        // reloading it is 500 bytes of `fxrstor` for nothing — the largest
        // single item in a switch. The kernel is built `+soft-float` with every
        // SSE feature off, so kernel code cannot disturb what a user thread
        // left in those registers however long it runs in between; the only
        // thing that overwrites them is another restore here.
        //
        // Both halves of the claim have to agree. This CPU's `fpu_owner` alone
        // would miss the thread having run on another CPU and changed its
        // registers there; the thread's `fpu_cpu` alone would miss this CPU
        // having loaded somebody else's state since.
        if next.user.is_some() {
            let percpu = get_percpu_data();
            let owned_here = percpu.fpu_owner.get() == next.id.0
                && next.fpu_cpu.load(Ordering::Acquire) == self.cpu;
            if !owned_here {
                // SAFETY: `next.fpu` is an `UnsafeCell` reached only from the
                // CPU its thread is being placed on, with interrupts off, and
                // `owned_here` just proved no other CPU still holds that
                // thread's register state. The buffer is the one this thread's
                // state was last saved into, which is what the two helpers
                // expect.
                unsafe {
                    let fpu = &mut *next.fpu.get();
                    if !next.fpu_init.load(Ordering::Relaxed) {
                        init_fpu_state(fpu);
                        next.fpu_init.store(true, Ordering::Relaxed);
                    } else {
                        restore_fpu_state(fpu);
                    }
                }
                percpu.fpu_owner.set(next.id.0);
                next.fpu_cpu.store(self.cpu, Ordering::Release);
            }
        }
        sched_prof::record(Stage::RestoreFpu, probe);

        let cpu = get_percpu_data();
        // Set RSP0 - validate it's in kernel space
        let kstack = next.kstack_top;
        if kstack < 0xFFFF_0000_0000_0000 {
            panic!(
                "Invalid kstack_top for thread {}: 0x{:x} (name: {})",
                next.id.0, kstack, next.name
            );
        }
        // SAFETY: `tss_mut` wants the caller on the CPU the TSS belongs to with
        // no other borrow of it live; `context_switch_to` runs with interrupts
        // off and takes the reference only for this store. RSP0 is next read on
        // a ring 3 -> ring 0 entry, which cannot happen before this returns.
        unsafe { cpu.tss_mut().privilege_stack_table[0] = VirtAddr::new(kstack) };

        // set kernel gs stack
        cpu.kernel_rsp.set(next.kstack_top);
        cpu.user_rsp.set(user_rsp);
        sched_prof::record(Stage::Switch, entry);
        // return handles context switch
    }

    /// Join the set of CPUs an enqueue may poke.
    ///
    /// Published only across the halt and taken back immediately after, because
    /// the bit means *halted*: a CPU that is awake and already looking for work
    /// has nothing to be told, and telling it would spend a wakeup on nobody.
    fn publish_idle(&self) {
        if self.cpu_index != NOT_TRACKED {
            IDLE_CPU_MASK.fetch_or(1 << self.cpu_index, Ordering::Release);
        }
    }

    /// Leave the idle set, reporting whether this CPU was still in it.
    ///
    /// `false` means somebody claimed it while it slept, which is the whole
    /// message: a CPU with more work than it can run wants this one to come and
    /// steal. Nothing else clears another CPU's bit.
    fn take_idle(&self) -> bool {
        if self.cpu_index == NOT_TRACKED {
            return true;
        }
        let bit = 1u64 << self.cpu_index;
        IDLE_CPU_MASK.fetch_and(!bit, Ordering::AcqRel) & bit != 0
    }

    /// Wake one halted CPU to come and take some of this runqueue.
    ///
    /// The threshold is [`Scheduler::try_steal`]'s own rule read from the other
    /// side: it refuses to leave a CPU with nothing to run, so a CPU carrying
    /// one runnable thread has nothing to offer and poking anybody for it only
    /// spends a wakeup.
    ///
    /// The quantity is [`Scheduler::load`] rather than the queue alone, because
    /// a CPU running one thread with a second queued has a thread to spare
    /// exactly as much as one with two queued does — and it is the commoner
    /// shape by far, since the first thing a CPU does with a queue of two is
    /// run one of them.
    ///
    /// Costs three atomic loads when the machine is busy, since a load below
    /// the threshold or a mask of zero ends it before any lookup.
    fn poke_idle_cpu(&self) {
        if self.load() < 2 {
            return;
        }
        let Some(idx) = claim_idle_cpu() else {
            return;
        };
        let target = smp::lapic_id_for_cpu(idx);
        // Claiming this CPU itself is not a mistake to undo: it is on its way
        // out of the halt already, and `take_idle` hands it the same message an
        // IPI would have carried.
        if target != get_percpu_data().lapic_id.get() {
            self.send_reschedule_ipi(target);
        }
    }

    pub fn send_reschedule_ipi(&self, target_cpu: u32) {
        without_interrupts(|| {
            // SAFETY: this CPU's own LAPIC drives the ICR, and
            // `without_interrupts` keeps the write on the CPU whose GS base
            // named it. `target_cpu` is a LAPIC id taken from `SCHEDULERS`, so
            // the IPI has a real destination.
            unsafe { get_lapic().send_ipi(InterruptIndex::Reschedule as u8, target_cpu) };
        });
    }

    // External messaging to sched

    #[inline]
    pub fn spawn_thread(&self, thread: Arc<Thread>) {
        // A thread this CPU may not run goes to one that may, rather than onto
        // a runqueue whose CPU will never pick it. `pick_sched_for` only
        // returns a CPU the affinity allows, so this recurses at most once; a
        // mask naming no registered CPU falls through and runs here, because
        // losing the thread is worse than ignoring the pin.
        if !self.thread_can_run_here(&thread)
            && let Some(target) = pick_sched_for(&thread)
        {
            target.spawn_thread(thread);
            return;
        }
        without_interrupts(|| {
            debug_assert!(
                !thread.rq_link.is_linked(),
                "spawn_thread: thread {} already linked",
                thread.id.0
            );
            let cur_state = thread.state();
            debug_assert_eq!(
                cur_state,
                State::Ready,
                "spawn_thread: thread {} must be Ready, is {:?}",
                thread.id.0,
                cur_state
            );
            thread.state.store(State::Ready as u8, Ordering::Release);
            thread.cpu.store(self.cpu, Ordering::Release);
            let request = thread.request_ns();
            self.with_rq(|rq| rq.enqueue_waking(thread.clone(), request));
            self.has_work.store(true, Ordering::Release);
            self.mark_running_thread_need_resched();

            if self.cpu != get_percpu_data().lapic_id.get() {
                sched().send_reschedule_ipi(self.cpu);
            }
        })
    }

    /// Attempt to steal a thread from another CPU's runqueue.
    /// Only pops the thread; bookkeeping (thread_count) is handled by
    /// the caller after confirming the thread can be run.
    fn try_steal(&self) -> Option<Arc<Thread>> {
        let keys: heapless::Vec<u32, 128> = {
            let schedulers = SCHEDULERS.read();
            schedulers.keys().copied().collect()
        };
        let count = keys.len();
        if count <= 1 {
            return None;
        }

        let start = self.steal_scan_start.fetch_add(1, Ordering::Relaxed) as usize % count;

        for i in 0..count {
            let idx = (start + i) % count;
            let victim_cpu = keys[idx];
            if victim_cpu == self.cpu {
                continue;
            }

            let victim = sched_for_cpu(victim_cpu);

            // Fast check: skip if victim has no work.
            if !victim.has_work.load(Ordering::Acquire) {
                continue;
            }

            // A thread running on the victim counts against the rule below,
            // and is read before the queue so a CPU that goes idle in between
            // costs a refused steal rather than a stranded thread.
            let victim_running = victim.current.load(Ordering::Acquire) != 0;

            // Give up on a victim whose runqueue is busy rather than waiting
            // for it; there are other CPUs to try.
            let stolen = victim.with_try_rq(|rq| {
                // Don't leave the victim with nothing to run: taking a CPU's
                // only runnable thread just moves it, and two idle CPUs would
                // pass it back and forth. What it *runs* counts, so a CPU
                // running one thread with another queued gives the queued one
                // up — otherwise a pair that lands together shares one CPU for
                // as long as it lives while the rest of the machine halts.
                let queued = rq.total_len();
                if queued == 0 || (queued == 1 && !victim_running) {
                    return None;
                }

                // The least urgent thread this CPU is allowed to run, chosen
                // in place. A thread whose context is not saved yet is not
                // stealable: it was enqueued (by a yield or a park abort)
                // before `context_switch` wrote its registers, so resuming it
                // elsewhere would resume stale state.
                rq.steal_victim(|thread| {
                    thread.context_saved.load(Ordering::Acquire) && self.thread_can_run_here(thread)
                })
            });

            // Don't touch victim.has_work here -- the victim updates it on its
            // own next tick. The victim's load has already fallen by one: the
            // pop went through `with_try_rq`.
            if let Some(Some(thread)) = stolen {
                return Some(thread);
            }
        }

        None
    }

    /// Periodic rebalancing: if another CPU carries significantly more runnable
    /// work than us, steal one thread and enqueue it locally. Called from
    /// on_tick.
    fn try_rebalance(&self) {
        let tick = self.rebalance_tick.fetch_add(1, Ordering::Relaxed);
        if !tick.is_multiple_of(REBALANCE_INTERVAL) {
            return;
        }

        // Snapshot every CPU's load in one short locked pass.
        let mut loads: heapless::Vec<(u32, u64), 128> = heapless::Vec::new();
        {
            let schedulers = SCHEDULERS.read();
            for (&cpu_id, &sched) in schedulers.iter() {
                let _ = loads.push((cpu_id, sched.load()));
            }
        }
        // Lock dropped.

        let self_load = loads
            .iter()
            .find(|(c, _)| *c == self.cpu)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let max_load = loads.iter().map(|(_, n)| *n).max().unwrap_or(0);

        if max_load.saturating_sub(self_load) < REBALANCE_THRESHOLD {
            return;
        }

        let Some(thread) = self.try_steal() else {
            return;
        };

        // Thread is in Ready state on the victim's runqueue.
        // Migrate it to our runqueue without changing state. The victim is
        // named before the move, since `thread.cpu` is about to become ours.
        trace_event!(Rebalance {
            thief_cpu: self.cpu,
            victim_cpu: thread.cpu.load(Ordering::Acquire),
            tid: thread.id.0,
        });
        thread.cpu.store(self.cpu, Ordering::Release);

        let request = thread.request_ns();
        self.with_rq(|rq| rq.enqueue_waking(thread.clone(), request));
        self.has_work.store(true, Ordering::Release);

        self.steal_count.fetch_add(1, Ordering::Relaxed);
        self.rebalance_count.fetch_add(1, Ordering::Relaxed);

        self.mark_running_thread_need_resched();
    }

    /// Attempt to steal a thread from another CPU and switch to it.
    /// Called from run_idle with interrupts disabled.
    /// Returns true if a thread was stolen and context_switch_to was called.
    /// Returns false if no steal occurred.
    fn try_steal_and_run(&self, context: *mut CpuContext) -> bool {
        let Some(thread) = self.try_steal() else {
            return false;
        };

        // The thread was in Ready state on the victim's runqueue.
        // Transition to Running on this CPU.
        if !thread.cas_state(State::Ready, State::Running) {
            // CAS failed: thread state changed between pop and CAS
            // (e.g. a waker moved it to Waking). Re-enqueue on its
            // home CPU so it isn't lost.
            let home_cpu = thread.cpu.load(Ordering::Acquire);
            let home = sched_for_cpu(home_cpu);
            thread.state.store(State::Ready as u8, Ordering::Release);
            let request = thread.request_ns();
            home.with_rq(|rq| rq.enqueue_waking(thread, request));
            home.has_work.store(true, Ordering::Release);
            return false;
        }

        trace_event!(Steal {
            thief_cpu: self.cpu,
            victim_cpu: thread.cpu.load(Ordering::Acquire),
            tid: thread.id.0,
        });
        self.steal_count.fetch_add(1, Ordering::Relaxed);

        // This path runs the thread without ever enqueuing it here, so it is
        // the one migration that has to place the thread by hand. Its virtual
        // clock is the victim CPU's and says nothing about this one: carried
        // over unplaced, a thread stolen from a busy CPU would arrive far
        // behind an idle CPU's `V` and hold it against everything that follows.
        let request = thread.request_ns();
        self.with_rq(|rq| rq.place(&thread, request));

        // context_switch_to overwrites the interrupt frame in-place.
        // SAFETY: `context` is this CPU's live interrupt frame, and the thread
        // was won by the state transition above, so this CPU is the only one
        // switching to it. Interrupts are off for the whole of the steal.
        unsafe { self.context_switch_to(thread, context) };
        true
    }

    /// Wake the thread identified by `handle` (thread-context variant).
    ///
    /// Safety properties:
    /// - `Weak::upgrade` is allocator-free: pure atomic CAS on the ArcInner
    ///   strong count. Returns `None` if the thread has already exited.
    /// - The temporary `Arc<Thread>` dropped at function exit decrements the
    ///   strong count. Because `THREADS` holds the canonical strong ref until
    ///   the reaper removes the thread, the strong count after our decrement is
    ///   always ≥ 1 while the thread is alive — no deallocation occurs here.
    /// - The wake-pending token protocol is unchanged: `do_wake` calls
    ///   `signal_wake` before probing state, preserving the park/wake invariant.
    ///
    /// Self-wake is skipped via `Weak::ptr_eq` *before* upgrade to avoid an
    /// unnecessary refcount bump on the common fast path.
    ///
    /// Returns `true` if a live thread was woken, `false` if the handle was
    /// dangling (thread exited) or the self-skip fired.
    pub fn wake_thread(&self, handle: &Weak<Thread>, priority: WakePriority) -> bool {
        // Self-skip: compare control-block pointers before paying for upgrade.
        if let Some(current_weak) = current_thread_weak()
            && Weak::ptr_eq(handle, &current_weak)
        {
            return false;
        }
        if let Some(thread) = handle.upgrade() {
            self.do_wake(thread, priority);
            true
        } else {
            false
        }
    }

    /// Wake the thread identified by `handle` from an IRQ handler.
    ///
    /// Identical to `wake_thread` but without the self-skip check —
    /// IRQ handlers are not associated with a specific thread identity and the
    /// self-skip check would require `current_thread()` which is not meaningful
    /// in that context.
    ///
    /// Safety properties are identical to `wake_thread`: `Weak::upgrade`
    /// is allocator-free and the temporary `Arc` drop cannot free the Thread
    /// because `THREADS` holds the strong ref until reaper removal.
    pub fn wake_thread_irq(&self, handle: &Weak<Thread>, priority: WakePriority) -> bool {
        if let Some(thread) = handle.upgrade() {
            self.do_wake(thread, priority);
            true
        } else {
            false
        }
    }

    /// Single-pass wake protocol shared by `wake_thread` and `wake_thread_irq`.
    ///
    /// Correctness rests on the wake-pending token: we publish it BEFORE
    /// probing state, so a parker that CASes Running→Parked after our store
    /// is guaranteed to observe `wake_pending=true` in `consume_wake_pending`
    /// and revert to Running. No retry loop, no spin: wakes are delivered
    /// in a single pass and the token closes every race window. Bare
    /// `thread_park` callers see at most one spurious wake per cycle.
    ///
    /// Safe from any context — `without_interrupts` is a cheap no-op when
    /// IRQs are already disabled.
    fn do_wake(&self, thread: Arc<Thread>, priority: WakePriority) {
        let probe = sched_prof::now_ns();
        // Publish wake intent. Pairs with `consume_wake_pending` in
        // transition_park / transition_park_while / transition_sleep.
        thread.signal_wake();

        without_interrupts(|| {
            // Fast path: thread is already Sleeping/Parked. Claim it via
            // try_wake (Sleeping/Parked → Waking) and enqueue it as Ready.
            if thread.try_wake() {
                self.complete_wake(&thread, priority);
                return;
            }

            // Otherwise the thread is Running, Ready, Waking, or Dying.
            // Token is already set, so a Running thread that subsequently
            // parks will abort. Nudge the target CPU so the wake takes
            // effect promptly.
            let state = State::from(thread.state.load(Ordering::Acquire));
            let cpu = thread.cpu.load(Ordering::Acquire);
            match state {
                State::Ready | State::Waking => {
                    sched_for_cpu(cpu).mark_running_thread_need_resched();
                }
                State::Running => {
                    thread.mark_need_resched();
                }
                State::Sleeping | State::Parked => {
                    // Lost the try_wake CAS race to a concurrent waker.
                    // They will (or already did) deliver via complete_wake.
                    return;
                }
                State::Dying => return,
            }
            if cpu != self.cpu {
                self.send_reschedule_ipi(cpu);
            }
        });
        sched_prof::record(Stage::Wake, probe);
    }
}

/// The thread running on *this* CPU, or `None` when the CPU is idle.
///
/// Which thread is current belongs to the CPU executing right now, never to a
/// `&Scheduler` value. A syscall runs with interrupts enabled, so its caller can
/// be preempted between any two instructions and resume on another CPU; a
/// `&Scheduler` obtained before that names the CPU the caller has left, whose
/// `current` now belongs to a different thread entirely. Interrupts are off for
/// the read so the per-CPU slot cannot change underneath it, and the returned
/// `Arc` stays valid afterwards however the thread migrates.
pub fn current_thread() -> Option<Arc<Thread>> {
    without_interrupts(|| get_percpu_data().current_thread())
}

/// Whether the thread running on *this* CPU has been marked for termination.
///
/// The flag is set by the signal that killed it; the death itself waits for
/// the syscall return boundary, where the thread provably holds nothing. Every
/// wait that blocks indefinitely on something only a peer can supply has to
/// consult this, or the thread never reaches that boundary and the process
/// cannot be killed at all — see `WaitQueue::wait_until_killable`.
///
/// Reads the per-CPU slot in place rather than through `current_thread`, which
/// hands back an `Arc`: this runs on every predicate evaluation of such a
/// wait, and the refcount pair buys nothing.
pub fn current_thread_killed() -> bool {
    without_interrupts(|| {
        get_percpu_data()
            .with_current_thread(|t| t.killed.load(Ordering::Acquire))
            .unwrap_or(false)
    })
}

/// `ThreadId` of the thread running on *this* CPU. See `current_thread`.
pub fn current_thread_id() -> Option<ThreadId> {
    without_interrupts(|| get_percpu_data().with_current_thread(|t| t.id))
}

/// A `Weak<Thread>` for the thread running on *this* CPU.
///
/// `Arc::downgrade` is a refcount bump only — allocator-free, so this is safe in
/// any context including IRQ handlers.
pub fn current_thread_weak() -> Option<Weak<Thread>> {
    current_thread().as_ref().map(Arc::downgrade)
}

/// `UserThreadInfo` of the thread running on *this* CPU.
///
/// Panics if the caller is not a user thread. Every caller is a syscall handler,
/// which by construction runs on behalf of one, and the id is read from this CPU
/// rather than from a scheduler that may belong to another.
pub fn current_thread_info() -> Arc<IrqSpinlock<UserThreadInfo>> {
    without_interrupts(|| {
        let cpu = get_percpu_data();
        let tid = cpu
            .with_current_thread(|t| t.id)
            .expect("current_thread_info: no thread running on this CPU");
        // The registry is a shared `RwLock` over a map, and a syscall reaches
        // here several times over — once for the errno it clears on entry, once
        // per arm that wants the fd table or the working directory, and once
        // more on the way out if the call failed. Answering from this CPU's own
        // slot is what keeps that from being a lookup each time; the thread
        // running here cannot change while interrupts are off.
        if let Some(info) = cpu.cached_thread_info(tid) {
            return info;
        }
        let info = THREADS
            .get_info(tid)
            .unwrap_or_else(|| panic!("current_thread_info: no UserThreadInfo for tid {}", tid.0));
        // SAFETY: inside `without_interrupts`, so the slot written belongs to
        // the CPU whose GS base was read -- `cache_thread_info`'s requirement.
        unsafe { cpu.cache_thread_info(tid, info.clone()) };
        info
    })
}

#[inline]
pub fn thread_yield() {
    debug_assert_preemptible("thread_yield");
    // SAFETY: `save_transition_switch` wants interrupts disabled, which
    // `without_interrupts` supplies, and a transition it can call on the
    // caller's stack before the switch. `transition_yield` reads no argument,
    // so the null `arg` is never dereferenced.
    without_interrupts(|| unsafe {
        save_transition_switch(transition_yield, core::ptr::null_mut());
    });
}

pub fn thread_park() {
    debug_assert_preemptible("thread_park");
    let Some(_cur) = current_thread() else {
        return;
    };
    // SAFETY: as in `thread_yield` -- interrupts off, and `transition_park`
    // reads no argument.
    without_interrupts(|| unsafe {
        save_transition_switch(transition_park, core::ptr::null_mut());
    });
}

/// Park the current thread while `should_park` returns true.
///
/// Sets state to Parked *before* calling the closure inside
/// `transition_park_while`, so any concurrent `try_wake()` sees Parked
/// and succeeds. This closes the lost-wakeup window that exists with
/// the bare `thread_park()`.
///
/// Contract: **may return spuriously** — a stale wake-pending token
/// consumed during the transition will short-circuit the park even when
/// the condition still says park. Callers MUST loop on the actual
/// condition. This matches Rust std's `Thread::park` semantics. The
/// reason: looping inside this function would re-park without
/// re-enrolling the thread on a wait queue, breaking wait-queue
/// protocols where the producer pops the waiter exactly once.
pub fn thread_park_while<F: FnMut() -> bool>(mut should_park: F) {
    debug_assert_preemptible("thread_park_while");
    let Some(_cur) = current_thread() else {
        return;
    };

    // Wrapper to call the Rust closure through a C function pointer.
    extern "C" fn check_wrapper<F: FnMut() -> bool>(ctx: *mut u8) -> bool {
        // SAFETY: `ctx` is the `&mut should_park` installed in `check_ctx`
        // beside this very instantiation of `check_wrapper::<F>`, so the type
        // matches. It points into `thread_park_while`'s frame, which stays live
        // until the park returns, and the transition runs on that thread with
        // nothing else able to reach the closure.
        let f = unsafe { &mut *(ctx as *mut F) };
        f()
    }

    let mut ctx = ParkWhileCtx {
        check_fn: check_wrapper::<F>,
        check_ctx: &mut should_park as *mut F as *mut u8,
    };
    // SAFETY: as in `thread_yield`, and `ctx` lives in this frame -- which the
    // parking thread keeps for the whole park -- and is of the type
    // `transition_park_while` reads it back as.
    without_interrupts(|| unsafe {
        save_transition_switch(
            transition_park_while,
            &mut ctx as *mut ParkWhileCtx as *mut u8,
        );
    });
}

#[inline]
pub fn thread_sleep(dt: Duration) {
    debug_assert_preemptible("thread_sleep");
    let Some(_cur) = current_thread() else {
        return;
    };
    let now = Instant::now();
    let deadline_ns = (now + dt).as_nanos();
    let mut ctx = SleepCtx { deadline_ns };
    // SAFETY: as in `thread_park_while` -- interrupts off, and `ctx` lives in
    // this frame and is of the type `transition_sleep` reads it back as.
    without_interrupts(|| unsafe {
        save_transition_switch(transition_sleep, &mut ctx as *mut SleepCtx as *mut u8);
    });
}

pub fn thread_exit(code: i32) -> ! {
    let tid = current_thread_id().expect("thread_exit: no thread running on this CPU");

    // Every path that ends a thread funnels through here, so this is the one
    // place the "no guard live where a thread can die" rule can be checked.
    crate::debug::lock_order::assert_no_guards_held("thread_exit");
    if let Some(t) = current_thread() {
        t.assert_no_borrowed_dma("thread_exit");
    }

    // Log lifetime stats before the without_interrupts fast path (log! allocates).
    if let Some(t) = get_thread_by_id(tid) {
        // Publishes the death to a tracer and, if this thread was the tracer,
        // ends the session so nothing keeps writing into an undrained ring.
        crate::syscalls::trace::on_thread_exit(&t, code);

        // Same for a profiler that died holding the session: nothing else
        // would ever free the ring or stop the sampling.
        profile::release_if_owner(tid.0);

        // Tell the creator a child is gone. Sent here rather than from
        // `record_thread_exit`, which runs with interrupts possibly disabled
        // and may not touch the thread registry. The default action is Ignore,
        // so this costs a lookup and changes nothing until somebody installs a
        // handler for it.
        let parent = t.parent.load(Ordering::Acquire);
        if parent != 0 && t.user.is_some() {
            crate::thread::thread::kill_process_with_signal(parent, crate::thread::signal::SIGCHLD);
        }

        let created = t.created_at_ns.load(Ordering::Acquire);
        if created != 0 {
            let wall_ns = Instant::now().as_nanos().saturating_sub(created);
            let cpu_ns = t.cpu_time_ns();
            let faults = t.demand_faults.load(Ordering::Relaxed);
            crate::log_debug!(
                "exit: code={} wall={}.{:03}ms cpu={}.{:03}ms faults={}",
                code,
                wall_ns / 1_000_000,
                (wall_ns / 1_000) % 1_000,
                cpu_ns / 1_000_000,
                (cpu_ns / 1_000) % 1_000,
                faults
            );
        }
    }

    // Mark Dying here, but do NOT hand the thread to the reaper yet: this is
    // still running on that thread's kernel stack, and `Thread::free` unmaps
    // it. `switch_away` pivots to the per-CPU scheduler stack first and
    // `reap_and_schedule` posts it from there. Heavy cleanup (free, unmap)
    // happens in the reaper with interrupts enabled.
    // SAFETY: `switch_away` wants interrupts off and the caller never to be
    // resumed. The thread is marked Dying here and `thread_exit` returns `!`,
    // so nothing after the call can run on this stack.
    without_interrupts(|| unsafe {
        if let Some(t) = get_thread_by_id(tid) {
            t.exit_code.store(code, Ordering::Release);
            t.state.store(State::Dying as u8, Ordering::Release);
        }
        switch_away();
    });
    loop {
        enable_and_hlt();
    }
}

/// Validate the frame pointer the interrupt trampoline handed us.
fn check_context(context: *mut CpuContext, who: &str) {
    if context.is_null() {
        panic!("{who}: null context ptr");
    }
    if (context as u64) < 0xFFFF_0000_0000_0000u64 {
        panic!("{who}: low context address {context:p}");
    }
    if !context.is_aligned() {
        panic!("{who}: misaligned context: {context:p}");
    }
}

/*
    User -> User:       Update RSP0 to new process's kernel stack
    User -> Kernel:     RSP0 doesn't matter
    Kernel -> User:     Must update RSP0 to user's kernel stack
    Kernel -> Kernel:   RSP0 doesn't matter
*/

/// Terminate the current thread if it has been marked killed.
///
/// Only sound where the thread holds nothing: there is no unwinding, so a
/// thread that dies with a lock guard live leaks it permanently. The two places
/// that qualify are the syscall return boundary and a timer tick that
/// interrupted ring 3.
pub fn exit_if_killed() {
    if let Some(thread) = current_thread()
        && thread.killed.load(Ordering::Acquire)
    {
        let code = thread.exit_code.load(Ordering::Acquire);
        drop(thread);
        thread_exit(code);
    }
}

/// Suspend the current thread if a stop signal is outstanding, returning when
/// `SIGCONT` clears it.
///
/// Called from the same boundaries as [`exit_if_killed`], and for the same
/// reason: a thread can only be suspended where it provably holds nothing, and
/// a syscall return or a tick out of ring 3 is where that is true. A stop
/// delivered mid-syscall therefore takes effect when the call finishes rather
/// than in the middle of it, which is also what keeps a suspended process from
/// holding a filesystem lock for as long as the user leaves it suspended.
pub fn stop_if_signalled() {
    let Some(thread) = current_thread() else {
        return;
    };
    if !thread.stop_requested.load(Ordering::Acquire) {
        return;
    }

    thread.stopped.store(true, Ordering::Release);
    // A shell parked in an untraced `waitpid` on this process is waiting for
    // exactly this: stopping ends that wait the same way exiting does.
    crate::thread::thread::EXITED_THREADS.wake_waiter(thread.id);
    let watched = thread.clone();
    // A kill outranks a stop: SIGKILL must reach a suspended process, so the
    // park ends for that too and the caller's `exit_if_killed` finishes it.
    //
    // `thread_park_while` may return without having parked at all — the wake
    // that carried the signal here leaves its wake-pending token set, and the
    // transition consumes that token and declines to park. Only the loop makes
    // the suspension actually happen; a single call turns Ctrl+Z on a sleeping
    // process into a no-op that resumes the syscall.
    while watched.stop_requested.load(Ordering::Acquire) && !watched.killed.load(Ordering::Acquire)
    {
        thread_park_while(|| {
            watched.stop_requested.load(Ordering::Acquire)
                && !watched.killed.load(Ordering::Acquire)
        });
    }
    thread.stopped.store(false, Ordering::Release);
}

/// First half of a timer tick. See `Scheduler::tick_prepare`.
pub fn tick_prepare(context: *mut CpuContext) -> u64 {
    check_context(context, "tick_prepare");
    // Proof of life for anyone waiting on this CPU to answer something.
    crate::smp::note_cpu_alive();
    // A thread spinning in user code reaches no syscall boundary, so the tick
    // is the only place it can observe a kill. Ring 3 in the interrupted frame
    // proves it holds no kernel lock guard, which is what makes exiting here
    // safe; a tick that caught it inside the kernel leaves it to the syscall
    // boundary.
    // SAFETY: `check_context` above proved `context` is a live, aligned,
    // kernel-address `CpuContext` -- the frame this tick interrupted.
    if unsafe { (*context).is_from_userspace() } {
        stop_if_signalled();
        exit_if_killed();
    }
    sched().tick_prepare(context)
}

/// Second half of a timer tick, on the scheduler stack if `pivoted`.
pub fn tick_finish(context: *mut CpuContext, pivoted: bool) {
    check_context(context, "tick_finish");
    sched().tick_finish(context, pivoted);
}

// ---------------------------------------------------------------------------
// Voluntary context switch — save-before-publish
// ---------------------------------------------------------------------------

/// Copy the synthetic CpuContext built by save_transition_switch into the
/// current thread's saved context, save FPU/TLS, and record time accounting.
/// Called from save_transition_switch (naked asm) with interrupts disabled.
/// Returns the per-CPU scheduler stack top so the asm trampoline can pivot RSP.
extern "C" fn do_save_current_thread(context: *mut CpuContext) -> u64 {
    let Some(current) = current_thread() else {
        let sched_stack = get_percpu_data().scheduler_stack_top.get();
        debug_assert!(sched_stack != 0, "scheduler stack not initialized");
        return sched_stack;
    };
    let end_ns = Instant::now().as_nanos();
    current.end_run(end_ns);
    let probe = sched_prof::now_ns();
    // SAFETY: `context` is the synthetic frame `save_transition_switch` built on
    // the calling thread's own stack and passed straight down, so it is live and
    // aligned for the length of this call.
    unsafe {
        *current.ctx.lock() = (*context).clone();
    }
    let probe = sched_prof::record(Stage::SaveCtx, probe);
    // SAFETY: `current.fpu` is an `UnsafeCell` touched only from the CPU its
    // thread is running on, and this runs on that CPU with interrupts off.
    unsafe {
        if current.user.is_some() {
            let fpu = &mut *current.fpu.get();
            if !current.fpu_init.load(Ordering::Relaxed) {
                init_fpu_state(fpu);
                current.fpu_init.store(true, Ordering::Relaxed);
            } else {
                save_fpu_state(fpu);
            }
        }
    }
    let probe = sched_prof::record(Stage::SaveFpu, probe);
    let fs_base = read_fs_base();
    current.tls_base.store(fs_base.as_u64(), Ordering::Release);
    sched_prof::record(Stage::SaveTls, probe);
    current.context_saved.store(true, Ordering::Release);
    let sched_stack = get_percpu_data().scheduler_stack_top.get();
    debug_assert!(sched_stack != 0, "scheduler stack not initialized");
    trace_event!(Save {
        cpu: sched().cpu,
        tid: current.id.0,
        // SAFETY: the frame `save_transition_switch` built and passed down.
        rip: unsafe {
            (*context)
                .interrupt_stack_frame
                .instruction_pointer
                .as_u64()
        },
    });
    sched_stack
}

/// Entry point for voluntary context switches.  Context is already saved by
/// save_transition_switch before this is called.  Runs wake_sleepers and
/// pick_and_run, then returns the (possibly replaced) context pointer.
extern "C" fn schedule_voluntary(context: *mut CpuContext) -> *mut CpuContext {
    if context.is_null() {
        panic!("schedule_voluntary: null context ptr");
    }
    if (context as u64) < 0xFFFF_0000_0000_0000u64 {
        panic!("schedule_voluntary: low context address {context:p}");
    }
    if !context.is_aligned() {
        panic!("schedule_voluntary: misaligned context: {context:p}");
    }
    let sched = sched();
    without_interrupts(|| {
        let probe = sched_prof::now_ns();
        sched.wake_sleepers();
        sched_prof::record(Stage::WakeSleepers, probe);
        sched.pick_and_run(context);
    });
    context
}

/// Switch away from the current thread without saving context (used by
/// thread_exit where the thread is about to be destroyed).
/// Builds a throwaway CpuContext frame on the stack, calls schedule_voluntary,
/// then iretq-restores the next thread.
///
/// # Safety
/// Must be called with interrupts disabled, on a thread that is never to be
/// resumed: the context is deliberately not saved, so returning to the caller
/// would resume from a frame nobody wrote. The caller must hold no lock guard
/// and no borrow of per-CPU state, since this never unwinds back to it.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_away() {
    core::arch::naked_asm!(
        // Leave the dying thread's kernel stack before anything publishes it.
        // The frame below, and every Rust frame under it, would otherwise sit
        // on memory the reaper is entitled to unmap the moment it sees the
        // thread. Nothing on this stack is needed again — the thread is Dying.
        "sub rsp, 8",
        "and rsp, -16",
        "cld",
        "call {sched_stack}",
        "mov rsp, rax",

        "sub rsp, 160",
        "mov rdi, rsp",
        "sub rsp, 8",
        "and rsp, -16",
        "cld",
        "call {reap_and_sched}",
        restore_context_and_iretq!(),
        sched_stack = sym scheduler_stack_top,
        reap_and_sched = sym reap_and_schedule,
    );
}

/// Top of this CPU's scheduler stack, for the naked trampolines to pivot to.
extern "C" fn scheduler_stack_top() -> u64 {
    let top = get_percpu_data().scheduler_stack_top.get();
    debug_assert!(top != 0, "scheduler stack not initialized");
    top
}

/// Hand the dying thread to the reaper and pick the next one.
///
/// Runs on the per-CPU scheduler stack, so the thread's own stack is already
/// free — which is the whole point of the pivot in `switch_away`.
extern "C" fn reap_and_schedule(context: *mut CpuContext) -> *mut CpuContext {
    check_context(context, "reap_and_schedule");
    let sc = sched();
    without_interrupts(|| {
        if let Some(t) = current_thread() {
            sc.current.store(0, Ordering::Release);
            // SAFETY: inside `without_interrupts`, so the store lands on the CPU
            // whose GS base named the slot, and `t` is a clone rather than a
            // borrow, so no `with_current_thread` reference is outstanding.
            unsafe { get_percpu_data().set_current_thread(None) };
            reaper_enqueue(t);
        }
        sc.wake_sleepers();
        sc.pick_and_run(context);
    });
    context
}

// ---------------------------------------------------------------------------
// Transition functions — called from save_transition_switch after save
// ---------------------------------------------------------------------------

extern "C" fn transition_yield(_arg: *mut u8) -> bool {
    let probe = sched_prof::now_ns();
    let sched = sched();
    let Some(cur) = current_thread() else {
        return true;
    };
    if cur.cas_state(State::Running, State::Ready) {
        sched.with_rq(|rq| rq.enqueue(cur.clone()));
        sched.has_work.store(true, Ordering::Release);
    }
    cur.flags
        .fetch_and(!Flags::NEED_RESCHED.bits(), Ordering::AcqRel);
    sched_prof::record(Stage::Transition, probe);
    true
}

extern "C" fn transition_park(_arg: *mut u8) -> bool {
    let probe = sched_prof::now_ns();
    let Some(cur) = current_thread() else {
        return true;
    };

    if !cur.cas_state(State::Running, State::Parked) {
        // Not Running (e.g., concurrent transition to Dying). Bail without
        // switching; the caller will observe the unexpected state on return.
        return false;
    }

    // Token check. Wakers publish `wake_pending` BEFORE probing state, so
    // any wake delivered after our caller's last check sees Parked here.
    if cur.consume_wake_pending() {
        if cur.cas_state(State::Parked, State::Running) {
            // Reverted cleanly: no waker reached us via try_wake. Skip the
            // context switch entirely.
            sched_prof::record(Stage::Transition, probe);
            return false;
        }
        // CAS lost: a waker beat us to try_wake (Parked -> Waking) and
        // complete_wake will set us Ready in some runqueue. We must switch
        // so the scheduler can pick us back up properly.
        sched_prof::record(Stage::Transition, probe);
        return true;
    }

    sched_prof::record(Stage::Transition, probe);
    true
}

/// Context struct passed through the `arg` pointer to `transition_park_while`.
#[repr(C)]
struct ParkWhileCtx {
    /// Returns true if the thread should park.
    ///
    /// Called only while the thread is still Running: `check_ctx` points at a
    /// closure on the thread's own kernel stack, which stops being readable
    /// the moment a waker can resume the thread elsewhere.
    check_fn: extern "C" fn(*mut u8) -> bool,
    check_ctx: *mut u8,
}

extern "C" fn transition_park_while(arg: *mut u8) -> bool {
    let probe = sched_prof::now_ns();
    // SAFETY: `arg` is the `ParkWhileCtx` `thread_park_while` placed in its own
    // frame and handed to `save_transition_switch`, which passes it through
    // unchanged. That frame belongs to the parking thread and stays live until
    // the park returns.
    let ctx = unsafe { &*(arg as *const ParkWhileCtx) };
    let Some(cur) = current_thread() else {
        return true;
    };

    // The condition is evaluated while the thread is still Running, and that
    // ordering is load-bearing rather than incidental. `ctx`, the closure it
    // points at and every variable that closure borrows all live on the
    // parking thread's kernel stack; this CPU has pivoted off that stack but
    // the thread still owns it. Publishing Parked first would let a waker on
    // another CPU claim the thread and resume it on its saved context, whose
    // RSP points back into those frames, and this read would then return
    // whatever the resumed thread wrote over them.
    //
    // Nothing below touches `ctx`.
    let should_park = (ctx.check_fn)(ctx.check_ctx);

    // Transition Running -> Parked.
    if !cur.cas_state(State::Running, State::Parked) {
        return false; // Not running (Dying, etc.) — don't switch.
    }

    // Check the wake-pending token AND the user condition. We bail (don't
    // park) if either says so. The token covers wakes that arrived after the
    // caller's last condition check but before our CAS to Parked --- including
    // the window the check above now sits in, where a waker finds the thread
    // Running and so leaves a token rather than changing state.
    let token = cur.consume_wake_pending();

    if token || !should_park {
        sched_prof::record(Stage::Transition, probe);
        if cur.cas_state(State::Parked, State::Running) {
            return false; // Reverted cleanly; no switch.
        }
        // CAS lost: a waker reached try_wake first (Parked -> Waking) and
        // complete_wake set us Ready in some runqueue. Switch so the
        // scheduler can pick us back up.
        return true;
    }

    sched_prof::record(Stage::Transition, probe);
    true
}

/// Context struct passed through the `arg` pointer to `transition_sleep`.
#[repr(C)]
struct SleepCtx {
    deadline_ns: u64,
}

extern "C" fn transition_sleep(arg: *mut u8) -> bool {
    // SAFETY: `arg` is the `SleepCtx` `thread_sleep` placed in its own frame and
    // handed to `save_transition_switch`, which passes it through unchanged; the
    // deadline is read out of it below before the thread publishes Sleeping.
    let ctx = unsafe { &*(arg as *const SleepCtx) };
    let sched = sched();
    let Some(cur) = current_thread() else {
        return true;
    };

    // Read the deadline out of `ctx` before publishing Sleeping: `ctx` lives on
    // the sleeping thread's kernel stack, and once another CPU can resume the
    // thread that stack is no longer ours to read.
    let deadline_ns = ctx.deadline_ns;

    if !cur.cas_state(State::Running, State::Sleeping) {
        // Interrupts are disabled and we own the Running state -- CAS cannot fail.
        unreachable!("transition_sleep: CAS Running->Sleeping failed");
    }

    // Token check: a wake delivered between the caller's setup and our CAS
    // to Sleeping should abort the sleep, just like park.
    if cur.consume_wake_pending() {
        if cur.cas_state(State::Sleeping, State::Running) {
            return false;
        }
        return true;
    }

    cur.sleep_deadline.store(deadline_ns, Ordering::Release);

    let mut sleepers = sched.sleepers.lock();
    let sleep_entry = SleepEntry {
        deadline: deadline_ns,
        thread: cur.clone(),
    };
    if sleepers.push(sleep_entry).is_err() {
        // Heap full — revert to Running so the thread isn't stuck forever.
        cur.state.store(State::Running as u8, Ordering::Release);
        return false;
    }
    let current_earliest = sched.earliest_deadline.load(Ordering::Acquire);
    if deadline_ns < current_earliest {
        sched
            .earliest_deadline
            .store(deadline_ns, Ordering::Release);
    }
    true
}

/// Combined save-transition-switch for voluntary context switches.
///
/// Saves the calling thread's context to thread.ctx, then calls
/// `transition(arg)`.  If transition returns true the function switches to
/// another thread and does not return to the caller until the thread is
/// resumed, at which point it returns `true`.  If transition returns false
/// the function returns `false` immediately without switching.
///
/// # Safety
/// Must be called with interrupts disabled. `transition` must be callable on
/// the caller's own kernel stack, and `arg` must either be null or point at
/// live storage of the type `transition` reads it back as -- storage that stays
/// valid for as long as the thread is away, which in practice means the
/// caller's frame, since the caller is suspended for exactly that long.
#[unsafe(naked)]
pub unsafe extern "C" fn save_transition_switch(
    transition: extern "C" fn(arg: *mut u8) -> bool,
    arg: *mut u8,
) -> bool {
    core::arch::naked_asm!(
        // rdi = transition fn ptr, rsi = arg
        // Allocate 160 bytes for the CpuContext frame.
        // Layout (offsets from rsp after sub):
        //   [rsp +   0] = r15  (offset  0)
        //   [rsp +   8] = r14  (offset  8)
        //   [rsp +  16] = r13  (offset 16)
        //   [rsp +  24] = r12  (offset 24)
        //   [rsp +  32] = r11  (offset 32)
        //   [rsp +  40] = r10  (offset 40)
        //   [rsp +  48] = r9   (offset 48)
        //   [rsp +  56] = r8   (offset 56)
        //   [rsp +  64] = rdi  (offset 64) <- transition fn
        //   [rsp +  72] = rsi  (offset 72) <- arg
        //   [rsp +  80] = rbp  (offset 80)
        //   [rsp +  88] = rbx  (offset 88)
        //   [rsp +  96] = rdx  (offset 96)
        //   [rsp + 104] = rcx  (offset 104)
        //   [rsp + 112] = rax  (offset 112)
        //   [rsp + 120] = RIP  (offset 120)  -> .Lresume
        //   [rsp + 128] = CS   (offset 128)  -> 0x08
        //   [rsp + 136] = RFLAGS (offset 136)
        //   [rsp + 144] = RSP  (offset 144)  -> frame base + 160 (ret addr location)
        //   [rsp + 152] = SS   (offset 152)  -> 0x10
        // Return address from `call save_transition_switch` lives at [rsp + 160].
        "sub rsp, 160",

        // Save all GPRs with original values BEFORE using r12 as scratch.
        "mov [rsp +   0], r15",
        "mov [rsp +   8], r14",
        "mov [rsp +  16], r13",
        "mov [rsp +  24], r12",    // original r12 saved here
        "mov [rsp +  32], r11",
        "mov [rsp +  40], r10",
        "mov [rsp +  48], r9",
        "mov [rsp +  56], r8",
        "mov [rsp +  64], rdi",    // transition fn ptr
        "mov [rsp +  72], rsi",    // arg
        "mov [rsp +  80], rbp",
        "mov [rsp +  88], rbx",
        "mov [rsp +  96], rdx",
        "mov [rsp + 104], rcx",
        "mov [rsp + 112], rax",

        // Use r12 as stable frame base pointer (callee-saved, preserved by Rust calls).
        "mov r12, rsp",

        // Build synthetic interrupt frame at [r12 + 120].
        // RIP = .Lresume (the resume trampoline)
        "lea rax, [rip + .Lresume]",
        "mov [r12 + 120], rax",
        // CS = 0x08
        "mov eax, {KCS}",
        "mov [r12 + 128], rax",
        // RFLAGS = current flags
        "pushfq",
        "pop rax",
        "mov [r12 + 136], rax",
        // RSP = r12 + 160 (where the return address lives; iretq restores RSP there,
        // then .Lresume: ret pops the return address and returns to the caller)
        "lea rax, [r12 + 160]",
        "mov [r12 + 144], rax",
        // SS = 0x10
        "mov eax, {KSS}",
        "mov [r12 + 152], rax",

        // Call do_save_current_thread(context = r12).
        // Returns scheduler stack top in rax.
        "mov rdi, r12",
        "lea rsp, [r12 - 8]",
        "and rsp, -16",
        "cld",
        "call {do_save}",

        // rax = scheduler stack top (returned by do_save_current_thread).
        // Pivot RSP to the per-CPU scheduler stack. The thread's kernel stack
        // is now free — no further reads from [rsp] below this point.
        // r12 still points to the CpuContext frame on the old stack;
        // it is callee-saved and survives all subsequent calls.
        "mov rsp, rax",

        // Load transition fn + arg from the old stack frame via r12.
        // Safe: the thread is still Running (transition hasn't changed state),
        // so no other CPU can touch its stack yet.
        //
        // The same rule binds the transition fn itself, and it is the one
        // thing to get right when writing another one: `arg` points into the
        // thread's kernel stack, so everything it needs must be read BEFORE it
        // publishes a state a waker can act on. Past that point another CPU may
        // resume the thread on its saved context --- whose RSP points back into
        // these frames --- and any later read returns whatever the resumed
        // thread wrote over them.
        "mov rax, [r12 + 64]",   // transition fn ptr (saved rdi)
        "mov rdi, [r12 + 72]",   // arg (saved rsi)
        "sub rsp, 8",
        "and rsp, -16",
        "cld",
        "call rax",               // transition(arg) on scheduler stack

        // rax = return value of transition (bool: 0 = abort, 1 = switch)
        "test al, al",
        "jz .Lresume_nosave",

        // Transition returned true: build a throwaway 160-byte CpuContext on
        // the scheduler stack (not the thread's stack). schedule_voluntary
        // overwrites all 160 bytes with the next thread's saved context before
        // the pops+iretq, so the initial content here doesn't matter.
        "sub rsp, 160",
        "mov rdi, rsp",
        "sub rsp, 8",
        "and rsp, -16",
        "cld",
        "call {schedule_vol}",

        // schedule_voluntary returns context pointer in rax. Restore from it.
        restore_context_and_iretq!(),

        // Transition returned false: state never changed from Running.
        // The old thread stack is exclusively owned by this CPU; safe to
        // switch back to it.
        ".Lresume_nosave:",
        "lea rsp, [r12 + 160]",  // rsp = frame base + 160 = return address location
        "mov r12, [r12 + 24]",   // restore original r12 (frame is still readable here)
        "xor eax, eax",
        "ret",

        // Resume trampoline: jumped to by iretq when THIS thread is rescheduled.
        // RSP = frame base + 160 (return address location). Return true to caller.
        ".Lresume:",
        "mov eax, 1",
        "ret",

        KCS = const 0x08u64,
        KSS = const 0x10u64,
        do_save = sym do_save_current_thread,
        schedule_vol = sym schedule_voluntary,
    );
}

// ---------------------------------------------------------------------------
// Reaper — deferred cleanup of dead threads
// ---------------------------------------------------------------------------

/// Queue of dead threads awaiting cleanup. Lock-free, allocation-free on push.
static REAPER_QUEUE: Once<ArrayQueue<Arc<Thread>>> = Once::new();
static REAPER_HANDLE: Once<Weak<Thread>> = Once::new();
/// ThreadId of the reaper kthread, or 0 before it starts.
pub static REAPER_TID: AtomicU64 = AtomicU64::new(0);

/// Returns true if the calling thread is the reaper kthread.
///
/// Used by debug_assert guards in blocking `Drop` implementations to catch
/// regressions where blocking work is inadvertently re-introduced on the
/// reaper path. Compiled out in release builds.
#[inline]
pub fn current_thread_is_reaper() -> bool {
    let current = current_thread_id().map(|t| t.0).unwrap_or(0);
    current != 0 && current == REAPER_TID.load(Ordering::Acquire)
}

/// Initialize the reaper subsystem. Call once from the BSP after scheduler init.
pub fn init_reaper() {
    REAPER_QUEUE.call_once(|| ArrayQueue::new(256));

    let tid =
        crate::thread::util::queue_spawn_kthread_named("reaper", reaper_thread as *const () as u64);
    REAPER_HANDLE.call_once(|| {
        crate::thread::thread::get_thread_weak(tid)
            .expect("reaper kthread vanished before call_once")
    });
    REAPER_TID.store(tid.0, Ordering::Release);
    println!("Reaper thread started (tid={})", tid.0);
}

fn reaper_queue() -> &'static ArrayQueue<Arc<Thread>> {
    REAPER_QUEUE.call_once(|| ArrayQueue::new(256))
}

extern "C" fn reaper_thread() -> ! {
    loop {
        thread_park_while(|| reaper_queue().is_empty());

        while let Some(t) = reaper_queue().pop() {
            let tid = t.id;
            let code = t.exit_code.load(Ordering::Acquire);
            let parent = t.parent.load(Ordering::Acquire);
            t.free();
            record_thread_exit(tid, code, parent);
            let _ = THREADS.remove(tid);
            let _ = THREADS.remove_info(tid);
            // Registry walk and allocation: fine here, forbidden on the exit
            // path that queued this thread.
            crate::thread::thread::adopt_orphans_of(tid);
        }
    }
}

/// Push a dead thread onto the reaper queue for deferred cleanup.
/// Called from thread_exit with interrupts disabled — must not allocate.
fn reaper_enqueue(thread: Arc<Thread>) {
    debug_assert_eq!(thread.state(), State::Dying);
    if reaper_queue().push(thread).is_err() {
        // Queue full — should not happen with 256 slots, but don't lose the thread.
        // The thread's resources will leak. Log it.
        println!("WARNING: reaper queue full, thread cleanup leaked");
    }
    // Wake the reaper thread. `reaper_enqueue` is called from `thread_exit`
    // with interrupts already disabled, so we use the IRQ-safe variant.
    if let Some(handle) = REAPER_HANDLE.get() {
        sched().wake_thread_irq(handle, WakePriority::Normal);
    }
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SleepEntry {
    deadline: u64,
    thread: Arc<Thread>,
}

// Natural ordering: smallest deadline first (used with BinaryHeap<_, Min>).
impl Ord for SleepEntry {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.deadline.cmp(&other.deadline)
    }
}
impl PartialOrd for SleepEntry {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for SleepEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && Arc::ptr_eq(&self.thread, &other.thread)
    }
}
impl Eq for SleepEntry {}

// Note: heap allocs are fine because they are mapped before any user thread is created.
// In the future consider syncing pages.
#[inline]
pub fn switch_to_kernel_page() {
    let kernel_cr3 = boot_info().cr3;
    if Cr3::read().0.start_address() != kernel_cr3.0.start_address() {
        // SAFETY: `boot_info().cr3` is the kernel's own PML4, live for as long
        // as the system is, so the code executing here and the stack under it
        // stay mapped across the write. It drops every non-global translation;
        // kernel-half mappings are marked GLOBAL
        // (`memory::mark_kernel_mappings_global`) and survive, which is the
        // point: they are identical in the space being left and the one being
        // entered.
        unsafe { Cr3::write(kernel_cr3.0, kernel_cr3.1) };
    }
}
