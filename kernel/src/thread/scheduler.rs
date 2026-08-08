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
    registers::{control::Cr3, model_specific::FsBase},
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
    println,
    thread::{
        UserThreadInfo,
        context::CpuContext,
        irqlock::IrqSpinlock,
        preempt::{debug_assert_preemptible, preempt_enabled},
        runqueue::RunQueue,
        thread::{Flags, State, THREADS, Thread, ThreadId, get_thread_by_id, record_thread_exit},
        util::pick_sched_for,
    },
    timer::Instant,
    util::per_cpu::get_percpu_data,
};

/// Rebalance every N timer ticks (~50ms at 5ms timeslice).
const REBALANCE_INTERVAL: u32 = 10;
/// Only steal if the busiest CPU has this many more threads than us.
const REBALANCE_THRESHOLD: u64 = 2;

pub fn init() {
    println!("Initializing scheduler");
    // TODO: refactor queue, so it isnt limited to 65k? maybe iterate on the storage threads
    // and use the queue as a priority queue, or that a threadid that is just a u32 or u16 and the queue is just of u16,
    // this would need to add a different pid for user threads alongside thread id and a mapping.
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
    let region = vmalloc(KTHREAD_STACK_REGION_SIZE);
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
    #[inline]
    const fn is_boosted(self) -> bool {
        matches!(self, WakePriority::Interrupt)
    }
}

pub struct Scheduler {
    pub cpu: u32,

    // Ready queue. Simple round-robin with priority buckets is enough.
    // Keep it small and predictable.
    rq: Mutex<RunQueue>,

    // Current running thread id for this CPU.
    pub current: AtomicU64, // 0 means idle

    // Time accounting
    pub default_timeslice: Duration,

    sleepers: Mutex<BinaryHeap<SleepEntry, Min, 1024>>,

    pub earliest_deadline: AtomicU64,

    pub thread_count: AtomicU64,

    has_work: AtomicBool,
    steal_count: AtomicU64,
    steal_scan_start: AtomicU32,
    rebalance_tick: AtomicU32,
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
            let mut rq = sc.rq.lock();
            rq.enqueue(thread.clone(), thread.priority(), priority.is_boosted());
            sc.has_work.store(true, Ordering::Release);
            trace_event!(Enqueue {
                cpu: sc.cpu,
                tid: thread.id.0
            });
            drop(rq);
            sc.mark_running_thread_need_resched();
        })
    }

    fn complete_wake(&self, thread: &Arc<Thread>, priority: WakePriority) {
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
    }

    pub fn new(cpu: u32) -> Self {
        Self {
            cpu,
            rq: Mutex::new(RunQueue::new()),
            current: AtomicU64::new(0),
            default_timeslice: Duration::from_millis(5),
            sleepers: Mutex::new(BinaryHeap::new()),
            thread_count: AtomicU64::new(0),
            earliest_deadline: AtomicU64::new(u64::MAX),
            has_work: AtomicBool::new(false),
            steal_count: AtomicU64::new(0),
            steal_scan_start: AtomicU32::new(0),
            rebalance_tick: AtomicU32::new(0),
        }
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
                    let mut rq = self.rq.lock();
                    let priority = cur.priority();
                    rq.enqueue(cur.clone(), priority, false);
                    self.has_work.store(true, Ordering::Release);
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
            let now = Instant::now();
            let ed = self.earliest_deadline.load(Ordering::Acquire);
            // Re-arm to the sooner of: a default timeslice, or the
            // earliest sleeper deadline.
            let mut next = now + self.default_timeslice;
            if ed != u64::MAX && ed != 0 {
                let dl = Instant::from_tick(ed);
                if dl < next {
                    next = dl;
                }
            }
            let dur = next.duration_since(now);
            // Clamp to at least 1us to avoid zero-length timers.
            set_apic_timer(if dur.is_zero() {
                Duration::from_micros(1)
            } else {
                dur
            });
        })
    }

    fn wake_sleepers(&self) {
        let now = Instant::now().tick();
        let mut sl = self.sleepers.lock();
        // Drain stale and expired entries from the heap.
        // Stale = not Sleeping (already woken, died, etc.).
        // We drain all stale entries at the top first, then process expired ones,
        // continuing to skip stale entries encountered along the way.
        loop {
            let Some(top) = sl.peek() else { break };
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
                debug_assert!(
                    !t.rq_link.is_linked(),
                    "wake_sleepers: thread {} already linked",
                    t.id.0
                );
                t.state.store(State::Ready as u8, Ordering::Release);
                let mut rq = self.rq.lock();
                let priority = t.priority();
                rq.enqueue(t, priority, false);
                self.has_work.store(true, Ordering::Release);
                drop(rq);
                self.mark_running_thread_need_resched();
            }
        }
        if let Some(next_sleep) = sl.peek() {
            self.earliest_deadline
                .store(next_sleep.deadline, Ordering::Release);
        } else {
            self.earliest_deadline.store(u64::MAX, Ordering::Release);
        }
    }

    #[allow(dead_code)]
    fn dump_all_threads(&self) {
        let threads = THREADS.list();
        let now = Instant::now();
        println!("=== Thread dump ({} threads) ===", threads.len());
        for t in &threads {
            let state = t.state();
            let cpu = t.cpu.load(Ordering::Relaxed);
            let is_user = if t.user.is_some() { "user" } else { "kern" };
            let sleep_dl = t.sleep_deadline.load(Ordering::Relaxed);
            if state == State::Sleeping && sleep_dl != 0 {
                let dl = Instant::from_tick(sleep_dl);
                let overdue = if now.tick() > sleep_dl {
                    let elapsed = now.duration_since(dl);
                    alloc::format!(
                        " OVERDUE {}.{:03}s",
                        elapsed.as_secs(),
                        elapsed.subsec_millis()
                    )
                } else {
                    let remaining = dl.duration_since(now);
                    alloc::format!(" due in {}ms", remaining.as_millis())
                };
                println!(
                    "  tid={:<4} cpu={} {:4} {:?} {}",
                    t.id.0, cpu, is_user, state, overdue
                );
            } else {
                println!(
                    "  tid={:<4} cpu={} {:4} {:?} name={}",
                    t.id.0, cpu, is_user, state, t.name
                );
            }
        }
        // Dump ALL CPUs' sleepers heap stats and earliest_deadline.
        let schedulers = SCHEDULERS.read();
        for (&cpu_id, &sched) in schedulers.iter() {
            let sl = sched.sleepers.lock();
            let ed = sched.earliest_deadline.load(Ordering::Relaxed);
            let current = sched.current.load(Ordering::Relaxed);
            let has_work = sched.has_work.load(Ordering::Relaxed);
            println!(
                "  cpu {} sleepers={}, earliest_deadline={}, current={}, has_work={}",
                cpu_id,
                sl.len(),
                if ed == u64::MAX {
                    "NONE".into()
                } else {
                    alloc::format!("{}", ed)
                },
                current,
                has_work
            );
        }
        println!("=== End thread dump ===");
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
        unsafe { get_percpu_data().set_current_thread(None) };

        // Switch to the kernel page table so we don't idle with a stale
        // user-process CR3 whose PML4 may be freed by another CPU.
        switch_to_kernel_page();

        self.has_work.store(false, Ordering::Release);
        enable();

        let mut idle_ticks: u32 = 0;
        let mut steal_backoff: u32 = 0;

        loop {
            // Break out if any work is available on our own runqueue.
            if self.has_work.load(Ordering::Acquire) {
                break;
            }

            // Poll for stealable work on an exponential backoff, so an idle CPU
            // that finds nothing keeps halting rather than spinning.
            let steal_interval = 1u32 << cmp::min(steal_backoff, 4);
            if idle_ticks % steal_interval == 0 {
                disable();
                if self.try_steal_and_run(context) {
                    return true;
                }
                steal_backoff = steal_backoff.saturating_add(1);
                enable();
            }

            without_interrupts(|| {
                let ed = self.earliest_deadline.load(Ordering::Acquire);
                let dur = if ed != u64::MAX && ed != 0 {
                    let now = Instant::now();
                    if ed <= now.tick() {
                        Duration::from_micros(1)
                    } else {
                        let dl = Instant::from_tick(ed);
                        dl.duration_since(now)
                    }
                } else {
                    Duration::from_millis(100)
                };
                set_apic_timer(dur);
            });

            // Halt until next interrupt (timer, IPI, device)
            x86_64::instructions::interrupts::enable_and_hlt();

            idle_ticks += 1;
        }

        disable();
        false
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
        if deadline != 0 && Instant::now().tick() >= deadline {
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
        loop {
            let next = {
                let mut rq = self.rq.lock();
                let item = rq.pop_next();
                self.has_work.store(!rq.is_empty(), Ordering::Release);
                item
            };

            match next {
                Some(t) => {
                    debug_assert!(
                        !t.rq_link.is_linked(),
                        "pick_and_run: thread {} still linked after pop",
                        t.id.0
                    );
                    if t.cas_state(State::Ready, State::Running) {
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

    fn mark_running_thread_need_resched(&self) {
        if let Some(current_tid) = self.running_tid() {
            if let Some(current_thread) = self.get_thread_by_id(current_tid) {
                current_thread.mark_need_resched();
            }
        }
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
            let end_tick = Instant::now().tick();
            current.end_run(end_tick);
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

            let fs_base = FsBase::read();
            current.tls_base.store(fs_base.as_u64(), Ordering::Release);
            current.context_saved.store(true, Ordering::Release);
            trace_event!(Save {
                cpu: self.cpu,
                tid: current.id.0,
                rip: unsafe {
                    (*context)
                        .interrupt_stack_frame
                        .instruction_pointer
                        .as_u64()
                },
            });
        }
    }

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
        self.current.store(next.id.0, Ordering::Release);
        unsafe { get_percpu_data().set_current_thread(Some(next.clone())) };
        next.cpu.store(self.cpu, Ordering::Release);
        next.context_saved.store(false, Ordering::Release);

        let now = Instant::now();
        next.begin_run(now.tick());
        let mut deadline = now + self.default_timeslice;

        let earliest_deadline = self.earliest_deadline.load(Ordering::Acquire);

        if earliest_deadline < deadline.tick() {
            deadline = Instant::from_tick(earliest_deadline);
        } else {
            self.earliest_deadline
                .store(deadline.tick(), Ordering::Release);
        }

        next.slice_deadline
            .store(deadline.tick(), Ordering::Release);
        set_apic_timer(deadline.duration_since(now));

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
        unsafe { *context = ctx_snapshot };

        // Switch address space
        next.switch_to_page();

        let next_fs_base = next.tls_base.load(Ordering::Acquire);

        FsBase::write(VirtAddr::new(next_fs_base));

        if next.user.is_some() {
            unsafe {
                let fpu = &mut *next.fpu.get();
                if !next.fpu_init.load(Ordering::Relaxed) {
                    init_fpu_state(fpu);
                    next.fpu_init.store(true, Ordering::Relaxed);
                } else {
                    restore_fpu_state(fpu);
                }
            }
        }

        let cpu = get_percpu_data();
        // Set RSP0 - validate it's in kernel space
        let kstack = next.kstack_top;
        if kstack < 0xFFFF_0000_0000_0000 {
            panic!(
                "Invalid kstack_top for thread {}: 0x{:x} (name: {})",
                next.id.0, kstack, next.name
            );
        }
        unsafe { cpu.tss_mut().privilege_stack_table[0] = VirtAddr::new(kstack) };

        // set kernel gs stack
        cpu.kernel_rsp.set(next.kstack_top);
        cpu.user_rsp.set(user_rsp);
        // return handles context switch
    }

    pub fn send_reschedule_ipi(&self, target_cpu: u32) {
        without_interrupts(|| {
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
        if !self.thread_can_run_here(&thread) {
            if let Some(target) = pick_sched_for(&thread) {
                target.spawn_thread(thread);
                return;
            }
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
            self.thread_count.fetch_add(1, Ordering::AcqRel);
            thread.state.store(State::Ready as u8, Ordering::Release);
            thread.cpu.store(self.cpu, Ordering::Release);
            let mut rq = self.rq.lock();
            rq.enqueue(thread.clone(), thread.priority(), false);
            self.has_work.store(true, Ordering::Release);
            drop(rq);
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

            // Try to lock victim's runqueue without blocking.
            let Some(mut rq) = victim.rq.try_lock() else {
                continue;
            };

            // Don't steal the victim's only thread.
            if rq.total_len() < 2 {
                continue;
            }

            // pop_back_any takes the lowest-priority tail thread.
            if let Some(thread) = rq.pop_back_any() {
                // Skip threads whose context hasn't been saved yet.
                // This happens when a thread is enqueued (e.g. yield,
                // park abort) before context_switch saves its registers.
                if !thread.context_saved.load(Ordering::Acquire) {
                    trace_event!(StealSkip {
                        cpu: self.cpu,
                        tid: thread.id.0
                    });
                    let prio = thread.priority();
                    rq.enqueue(thread, prio, false);
                    continue;
                }
                if !self.thread_can_run_here(&thread) {
                    // Thread can't run on this CPU -- push it back.
                    let prio = thread.priority();
                    rq.enqueue(thread, prio, false);
                    continue;
                }
                // Don't touch victim.has_work or thread_count here --
                // the victim updates has_work on its own next tick,
                // and thread_count is adjusted by the caller after CAS.
                drop(rq);
                return Some(thread);
            }
        }

        None
    }

    /// Periodic rebalancing: if another CPU has significantly more threads
    /// than us, steal one and enqueue it locally. Called from on_tick.
    fn try_rebalance(&self) {
        let tick = self.rebalance_tick.fetch_add(1, Ordering::Relaxed);
        if tick % REBALANCE_INTERVAL != 0 {
            return;
        }

        // Snapshot thread counts from all CPUs in one short locked pass.
        let mut counts: heapless::Vec<(u32, u64), 128> = heapless::Vec::new();
        {
            let schedulers = SCHEDULERS.read();
            for (&cpu_id, &sched) in schedulers.iter() {
                let _ = counts.push((cpu_id, sched.thread_count.load(Ordering::Relaxed)));
            }
        }
        // Lock dropped.

        let self_count = counts
            .iter()
            .find(|(c, _)| *c == self.cpu)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let max_count = counts.iter().map(|(_, n)| *n).max().unwrap_or(0);

        if max_count.saturating_sub(self_count) < REBALANCE_THRESHOLD {
            return;
        }

        let Some(thread) = self.try_steal() else {
            return;
        };

        // Thread is in Ready state on the victim's runqueue.
        // Migrate it to our runqueue without changing state.
        let victim_cpu = thread.cpu.load(Ordering::Acquire);
        thread.cpu.store(self.cpu, Ordering::Release);

        let prio = thread.priority();
        {
            let mut rq = self.rq.lock();
            rq.enqueue(thread.clone(), prio, false);
        }
        self.has_work.store(true, Ordering::Release);

        sched_for_cpu(victim_cpu)
            .thread_count
            .fetch_sub(1, Ordering::Relaxed);
        self.thread_count.fetch_add(1, Ordering::Relaxed);
        self.steal_count.fetch_add(1, Ordering::Relaxed);

        trace_event!(Rebalance {
            thief_cpu: self.cpu,
            victim_cpu: victim_cpu,
            tid: thread.id.0,
        });

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
            let mut rq = home.rq.lock();
            let prio = thread.priority();
            rq.enqueue(thread, prio, false);
            home.has_work.store(true, Ordering::Release);
            return false;
        }

        // Adjust thread_count now that the steal is committed.
        let victim_cpu = thread.cpu.load(Ordering::Acquire);
        trace_event!(Steal {
            thief_cpu: self.cpu,
            victim_cpu: victim_cpu,
            tid: thread.id.0,
        });
        sched_for_cpu(victim_cpu)
            .thread_count
            .fetch_sub(1, Ordering::Relaxed);
        self.thread_count.fetch_add(1, Ordering::Relaxed);
        self.steal_count.fetch_add(1, Ordering::Relaxed);

        // context_switch_to overwrites the interrupt frame in-place.
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
        if let Some(current_weak) = current_thread_weak() {
            if Weak::ptr_eq(handle, &current_weak) {
                return false;
            }
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
    let tid = current_thread_id().expect("current_thread_info: no thread running on this CPU");
    THREADS
        .get_info(tid)
        .unwrap_or_else(|| panic!("current_thread_info: no UserThreadInfo for tid {}", tid.0))
}

#[inline]
pub fn thread_yield() {
    debug_assert_preemptible("thread_yield");
    without_interrupts(|| unsafe {
        save_transition_switch(transition_yield, core::ptr::null_mut());
    });
}

pub fn thread_park() {
    debug_assert_preemptible("thread_park");
    let Some(_cur) = current_thread() else {
        return;
    };
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
        let f = unsafe { &mut *(ctx as *mut F) };
        f()
    }

    let mut ctx = ParkWhileCtx {
        check_fn: check_wrapper::<F>,
        check_ctx: &mut should_park as *mut F as *mut u8,
    };
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
    let deadline_tick = (now + dt).tick();
    let mut ctx = SleepCtx { deadline_tick };
    without_interrupts(|| unsafe {
        save_transition_switch(transition_sleep, &mut ctx as *mut SleepCtx as *mut u8);
    });
}

pub fn thread_exit(code: i32) -> ! {
    let tid = current_thread_id().expect("thread_exit: no thread running on this CPU");

    // Log lifetime stats before the without_interrupts fast path (log! allocates).
    if let Some(t) = get_thread_by_id(tid) {
        let created = t.created_at_tick.load(Ordering::Acquire);
        if let Some(timer) = crate::drivers::hpet::driver::get_hpet_timer()
            && created != 0
        {
            let now = crate::timer::Instant::now().tick();
            let wall_ns = timer.ticks_to_nanos(now.saturating_sub(created));
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

pub fn exit_thread(tid: ThreadId) {
    if let Some(t) = THREADS.remove(tid) {
        debug_assert!(
            !t.rq_link.is_linked(),
            "exit_thread: thread {} still linked on runqueue",
            tid.0
        );
        t.state.store(State::Dying as u8, Ordering::Release);
        t.free();
        let code = t.exit_code.load(Ordering::Acquire);
        record_thread_exit(tid, code);
        let _ = THREADS.remove_info(tid);
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

/// First half of a timer tick. See `Scheduler::tick_prepare`.
pub fn tick_prepare(context: *mut CpuContext) -> u64 {
    check_context(context, "tick_prepare");
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
    let end_tick = Instant::now().tick();
    current.end_run(end_tick);
    unsafe {
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
    let fs_base = FsBase::read();
    current.tls_base.store(fs_base.as_u64(), Ordering::Release);
    current.context_saved.store(true, Ordering::Release);
    let sched_stack = get_percpu_data().scheduler_stack_top.get();
    debug_assert!(sched_stack != 0, "scheduler stack not initialized");
    trace_event!(Save {
        cpu: sched().cpu,
        tid: current.id.0,
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
        sched.wake_sleepers();
        sched.pick_and_run(context);
    });
    context
}

/// Switch away from the current thread without saving context (used by
/// thread_exit where the thread is about to be destroyed).
/// Builds a throwaway CpuContext frame on the stack, calls schedule_voluntary,
/// then iretq-restores the next thread.
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
        "mov rsp, rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rbp",
        "pop rbx",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
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
            unsafe { get_percpu_data().set_current_thread(None) };
            sc.thread_count.fetch_sub(1, Ordering::Relaxed);
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
    let sched = sched();
    let Some(cur) = current_thread() else {
        return true;
    };
    if cur.cas_state(State::Running, State::Ready) {
        let mut rq = sched.rq.lock();
        rq.enqueue(cur.clone(), cur.priority(), false);
        sched.has_work.store(true, Ordering::Release);
    }
    cur.flags
        .fetch_and(!Flags::NEED_RESCHED.bits(), Ordering::AcqRel);
    true
}

extern "C" fn transition_park(_arg: *mut u8) -> bool {
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
            return false;
        }
        // CAS lost: a waker beat us to try_wake (Parked -> Waking) and
        // complete_wake will set us Ready in some runqueue. We must switch
        // so the scheduler can pick us back up properly.
        return true;
    }

    true
}

/// Context struct passed through the `arg` pointer to `transition_park_while`.
#[repr(C)]
struct ParkWhileCtx {
    /// Returns true if the thread should park.
    check_fn: extern "C" fn(*mut u8) -> bool,
    check_ctx: *mut u8,
}

extern "C" fn transition_park_while(arg: *mut u8) -> bool {
    let ctx = unsafe { &*(arg as *const ParkWhileCtx) };
    let Some(cur) = current_thread() else {
        return true;
    };

    // Transition Running -> Parked.
    if !cur.cas_state(State::Running, State::Parked) {
        return false; // Not running (Dying, etc.) — don't switch.
    }

    // Check the wake-pending token AND the user condition. We bail (don't
    // park) if either says so. The token covers wakes that arrived after
    // the caller's last condition check but before our CAS to Parked; the
    // closure covers state changes the producer made without going through
    // a wake (or wakes whose target was just transitioned to Parked, where
    // try_wake will succeed independently).
    let token = cur.consume_wake_pending();
    let should_park = (ctx.check_fn)(ctx.check_ctx);

    if token || !should_park {
        if cur.cas_state(State::Parked, State::Running) {
            return false; // Reverted cleanly; no switch.
        }
        // CAS lost: a waker reached try_wake first (Parked -> Waking) and
        // complete_wake set us Ready in some runqueue. Switch so the
        // scheduler can pick us back up.
        return true;
    }

    true
}

/// Context struct passed through the `arg` pointer to `transition_sleep`.
#[repr(C)]
struct SleepCtx {
    deadline_tick: u64,
}

extern "C" fn transition_sleep(arg: *mut u8) -> bool {
    let ctx = unsafe { &*(arg as *const SleepCtx) };
    let sched = sched();
    let Some(cur) = current_thread() else {
        return true;
    };

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

    let deadline_tick = ctx.deadline_tick;
    cur.sleep_deadline.store(deadline_tick, Ordering::Release);

    let mut sleepers = sched.sleepers.lock();
    let sleep_entry = SleepEntry {
        deadline: deadline_tick,
        thread: cur.clone(),
    };
    if sleepers.push(sleep_entry).is_err() {
        // Heap full — revert to Running so the thread isn't stuck forever.
        cur.state.store(State::Running as u8, Ordering::Release);
        return false;
    }
    let current_earliest = sched.earliest_deadline.load(Ordering::Acquire);
    if deadline_tick < current_earliest {
        sched
            .earliest_deadline
            .store(deadline_tick, Ordering::Release);
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
/// Must be called with interrupts disabled.
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
        "mov rsp, rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rbp",
        "pop rbx",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",

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
            t.free();
            record_thread_exit(tid, code);
            let _ = THREADS.remove(tid);
            let _ = THREADS.remove_info(tid);
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
        // Cr3::write flushes all non-global TLB entries. Kernel mappings
        // don't use GLOBAL, so they are all refreshed by this write.
        unsafe { Cr3::write(kernel_cr3.0, kernel_cr3.1) };
    }
}
