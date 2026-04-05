use core::{
    cmp,
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    time::Duration,
};

use alloc::{boxed::Box, sync::Arc};
use crossbeam_queue::ArrayQueue;
use heapless::{BinaryHeap, binary_heap::Min};
use spin::{Mutex, Once, RwLock};
use x86_64::{
    VirtAddr,
    instructions::interrupts::{disable, enable, enable_and_hlt, without_interrupts},
    registers::{control::Cr3, model_specific::FsBase},
};

use crate::{
    apic::{get_lapic, set_apic_timer, set_apic_timer_and_enable},
    boot::boot_info,
    drivers::fpu::{init_fpu_state, restore_fpu_state, save_fpu_state},
    interrupts::InterruptIndex,
    println,
    smp::tlb_flush_all_including_global,
    thread::{
        UserThreadInfo,
        context::CpuContext,
        irqlock::IrqSpinlock,
        runqueue::RunQueue,
        thread::{Flags, State, THREADS, Thread, ThreadId, get_thread_by_id, record_thread_exit},
    },
    timer::Instant,
    util::per_cpu::get_percpu_data,
};

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
            drop(rq);
            sc.mark_running_thread_need_resched();
        })
    }

    fn complete_wake(&self, thread: &Arc<Thread>, priority: WakePriority) {
        without_interrupts(|| {
            // SAFETY INVARIANT: Must enqueue on the thread's last CPU.
            // thread_park_while's abort path relies on this -- if the thread
            // were enqueued on a different CPU, that CPU could pop and run it
            // while the original CPU still executes the abort path, causing
            // two CPUs to run the same thread simultaneously.
            let cpu = thread.cpu.load(Ordering::Acquire);
            let sc = sched_for_cpu(cpu);
            thread.state.store(State::Ready as u8, Ordering::Release);
            Self::enqueue_ready(sc, thread, priority);
            if cpu != self.cpu {
                self.send_reschedule_ipi(cpu);
            }
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
        }
    }

    pub fn current_thread_id(&self) -> Option<ThreadId> {
        let tid = self.current.load(Ordering::Acquire);
        if tid == 0 {
            return None;
        }
        Some(ThreadId(tid))
    }

    pub fn current_thread(&self) -> Option<Arc<Thread>> {
        get_percpu_data().current_thread()
    }

    pub fn on_tick(&self, context: *mut CpuContext) {
        without_interrupts(|| {
            self.wake_sleepers();
            // When idle (current==0) and no work queued, skip maybe_preempt
            // to prevent recursive run_idle (on_tick -> pick_and_run -> run_idle
            // -> enable IRQs -> on_tick -> pick_and_run -> run_idle -> ...).
            // When idle WITH work, we must call maybe_preempt so pick_and_run
            // can pop the thread from the runqueue.
            let idle = self.current.load(Ordering::Acquire) == 0;
            if !idle || self.has_work.load(Ordering::Acquire) {
                self.maybe_preempt(context);
            }
        })
    }

    fn wake_sleepers(&self) {
        let now = Instant::now().tick();
        let mut sl = self.sleepers.lock();
        // Drain stale entries (dead or already woken) from the top of the heap.
        while let Some(top) = sl.peek() {
            if top.thread.state() != State::Sleeping {
                sl.pop();
                continue;
            }
            break;
        }
        while let Some(top) = sl.peek() {
            if top.deadline > now {
                break;
            }
            let t = sl.pop().unwrap().thread;
            if t.state() == State::Dying {
                continue;
            }
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

        self.has_work.store(false, Ordering::Release);
        enable();

        let mut idle_ticks: u32 = 0;
        let mut steal_backoff: u32 = 0;

        loop {
            // Break out if any work is available on our own runqueue.
            if self.has_work.load(Ordering::Acquire) {
                break;
            }

            // Try to steal work from another CPU before halting.
            // Exponential backoff: attempt every 1, 2, 4, 8, 16 ticks.
            let steal_interval = 1u32 << cmp::min(steal_backoff, 4);
            if idle_ticks % steal_interval == 0 {
                let mut stole = false;
                without_interrupts(|| {
                    if self.try_steal_and_run(context) {
                        stole = true;
                    } else {
                        steal_backoff = steal_backoff.saturating_add(1);
                    }
                });
                if stole {
                    // context_switch_to already ran: context now points
                    // to the stolen thread's frame. Caller must return
                    // immediately so the interrupt handler iretq's into it.
                    disable();
                    return true;
                }
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
            if idle_ticks == 50 {
                println!(
                    "WARNING: cpu {} idle for ~5s, thread_count={}, steals={}",
                    self.cpu,
                    self.thread_count.load(Ordering::Relaxed),
                    self.steal_count.load(Ordering::Relaxed),
                );
            }
        }

        disable();
        false
    }

    pub fn maybe_preempt(&self, context: *mut CpuContext) {
        let Some(cur) = self.current_thread() else {
            self.pick_and_run(context); // was idle
            return;
        };

        // Fast check without locking the runqueue.
        let need = cur.flags.load(Ordering::Acquire) & Flags::NEED_RESCHED.bits() != 0;
        // ingnore need resched for now
        if !need {
            return;
        }

        // Clear request and pick another.
        cur.flags
            .fetch_and(!Flags::NEED_RESCHED.bits(), Ordering::AcqRel);

        // Requeue current if still runnable.
        let state_now: State = cur.state.load(Ordering::Acquire).into();

        #[allow(clippy::single_match)]
        match state_now {
            State::Running => {
                // If another CPU already marked it Parked/Sleeping/Dying, don't requeue
                if cur.cas_state(State::Running, State::Ready) {
                    debug_assert!(
                        !cur.rq_link.is_linked(),
                        "maybe_preempt: thread {} already linked before re-enqueue",
                        cur.id.0
                    );
                    let mut rq = self.rq.lock();
                    rq.enqueue(cur.clone(), cur.priority(), false);
                    self.has_work.store(true, Ordering::Release);
                }
            }
            // If it was already moved elsewhere (Parked, Sleeping, Dying), skip requeue
            _ => {}
        }

        self.pick_and_run(context);
    }

    fn pick_and_run(&self, context: *mut CpuContext) {
        self.save_current_thread(context);
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

    fn thread_can_run_here(&self, _t: &Thread) -> bool {
        true
        //let mask = t.cpu_affinity.load(Ordering::Acquire);
        //mask == 0 || (mask & (1u32 << self.cpu)) != 0
    }

    fn mark_running_thread_need_resched(&self) {
        if let Some(current_tid) = self.current_thread_id() {
            if let Some(current_thread) = self.get_thread_by_id(current_tid) {
                current_thread.mark_need_resched();
            }
        }
    }

    fn save_current_thread(&self, context: *mut CpuContext) {
        if let Some(current) = self.current_thread() {
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

        // Set as current
        self.current.store(next.id.0, Ordering::Release);
        unsafe { get_percpu_data().set_current_thread(Some(next.clone())) };
        next.cpu.store(self.cpu, Ordering::Release);

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
        without_interrupts(|| {
            debug_assert!(
                !thread.rq_link.is_linked(),
                "spawn_thread: thread {} already linked",
                thread.id.0
            );
            self.thread_count.fetch_add(1, Ordering::AcqRel);
            thread.state.store(State::Ready as u8, Ordering::Release);
            thread.cpu.store(self.cpu, Ordering::Release);
            if self.thread_can_run_here(&thread) {
                let mut rq = self.rq.lock();
                rq.enqueue(thread.clone(), thread.priority(), false);
                self.has_work.store(true, Ordering::Release);
                drop(rq);
                self.mark_running_thread_need_resched();
            } else {
                // will be queued on its target cpu by that cpu’s scheduler
            }

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
                let affinity = thread.cpu_affinity.load(Ordering::Acquire);
                if affinity != 0 && (affinity & (1u32 << self.cpu)) == 0 {
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
        sched_for_cpu(victim_cpu)
            .thread_count
            .fetch_sub(1, Ordering::Relaxed);
        self.thread_count.fetch_add(1, Ordering::Relaxed);
        self.steal_count.fetch_add(1, Ordering::Relaxed);

        // context_switch_to overwrites the interrupt frame in-place.
        unsafe { self.context_switch_to(thread, context) };
        true
    }

    #[inline]
    pub fn thread_yield(&self) {
        without_interrupts(|| {
            let Some(cur) = self.current_thread() else {
                return;
            };
            if cur.cas_state(State::Running, State::Ready) {
                cur.mark_need_resched();
                let mut rq = self.rq.lock();
                rq.enqueue(cur.clone(), cur.priority(), false);
                self.has_work.store(true, Ordering::Release);
            }

            unsafe { context_switch() };
        })
    }

    pub fn thread_park(&self) {
        let Some(cur) = self.current_thread() else {
            return;
        };

        loop {
            let state = cur.state.load(Ordering::Acquire);
            if state == State::Running as u8 {
                if cur
                    .state
                    .compare_exchange(
                        State::Running as u8,
                        State::Parked as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    break;
                }
            } else {
                break; // not Running (Dying, Waking, etc.) — bail out
            }
        }

        without_interrupts(|| unsafe {
            cur.mark_need_resched();
            context_switch();
        })
    }

    /// Park the current thread while `should_park` returns true.
    ///
    /// Sets state to Parked *before* calling the closure, so any concurrent
    /// `try_wake()` sees Parked and succeeds. This closes the lost-wakeup
    /// window that exists with `thread_park()`.
    pub fn thread_park_while<F: FnMut() -> bool>(&self, mut should_park: F) {
        let Some(cur) = self.current_thread() else {
            return;
        };

        loop {
            debug_assert!(
                !cur.rq_link.is_linked(),
                "thread_park_while: thread {} rq_link linked at loop start",
                cur.id.0
            );

            // 1. Transition Running -> Parked
            if !cur.cas_state(State::Running, State::Parked) {
                return; // Dying, Waking, etc.
            }

            // 2. Check condition with state already Parked.
            //    Any wakeup arriving now will succeed via try_wake().
            if !should_park() {
                // Condition is false — revert to Running.
                if cur.cas_state(State::Parked, State::Running) {
                    return;
                }
                // CAS failed: a waker set Parked -> Waking -> Ready and
                // enqueued us on our last CPU (this CPU). Context-switch
                // so the scheduler properly pops and unlinks us from the
                // runqueue.
                // SAFETY: depends on complete_wake enqueueing on the
                // thread's last CPU (see invariant comment there). If
                // complete_wake is ever changed to enqueue elsewhere,
                // this path must be redesigned to prevent two CPUs from
                // running the same thread.
                without_interrupts(|| unsafe {
                    cur.mark_need_resched();
                    context_switch();
                });
                // Woken: scheduler popped us, set Running, unlinked rq_link.
                // Loop back to re-check condition.
                continue;
            }

            // 3. Condition is true — actually sleep.
            without_interrupts(|| unsafe {
                cur.mark_need_resched();
                context_switch();
            });

            // 4. Woken up (state is Running again). Loop to re-check condition.
        }
    }

    #[inline]
    pub fn thread_sleep(&self, dt: Duration) {
        let Some(cur) = self.current_thread() else {
            return;
        };

        loop {
            let state = cur.state.load(Ordering::Acquire);
            if state == State::Running as u8 {
                if cur
                    .state
                    .compare_exchange(
                        State::Running as u8,
                        State::Sleeping as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    break;
                }
            } else {
                break; // only running threads can sleep
            }
        }

        without_interrupts(|| unsafe {
            let now = Instant::now();
            let deadline_tick = (now + dt).tick();
            cur.sleep_deadline.store(deadline_tick, Ordering::Release);

            {
                let mut sleepers = self.sleepers.lock();
                let sleep_entry = SleepEntry {
                    deadline: deadline_tick,
                    thread: cur.clone(),
                };
                if sleepers.push(sleep_entry).is_err() {
                    // Heap full — revert to Running so the thread isn't stuck forever.
                    cur.state.store(State::Running as u8, Ordering::Release);
                    return;
                }
                // Update earliest deadline while still holding the sleepers lock,
                // so a timer interrupt on another CPU can't miss this deadline.
                let current_earliest = self.earliest_deadline.load(Ordering::Acquire);
                if deadline_tick < current_earliest {
                    self.earliest_deadline
                        .store(deadline_tick, Ordering::Release);
                }
            }

            cur.mark_need_resched();
            context_switch();
        })
    }

    // Careful over proritizing, it can starve threads, specially in smp 1
    pub fn wake_thread(&self, tid: ThreadId, priority: WakePriority) {
        if Some(tid) == self.current_thread_id() {
            return;
        }
        self.wake_thread_internal(tid, priority, false);
    }

    pub fn wake_thread_irq(&self, tid: ThreadId, priority: WakePriority) {
        self.wake_thread_internal(tid, priority, true);
    }

    fn wake_thread_internal(&self, tid: ThreadId, priority: WakePriority, from_irq: bool) {
        if let Some(t) = get_thread_by_id(tid) {
            if from_irq {
                self.wake_thread_from_irq(t, priority);
            } else {
                self.wake_thread_slow(t, priority);
            }
        }
    }

    fn wake_thread_slow(&self, thread: Arc<Thread>, priority: WakePriority) {
        const MAX_RETRIES: usize = 64;
        let mut retries = 0;

        loop {
            if thread.try_wake() {
                self.complete_wake(&thread, priority);
                return;
            }

            match State::from(thread.state.load(Ordering::Acquire)) {
                State::Ready | State::Waking => {
                    without_interrupts(|| {
                        let cpu = thread.cpu.load(Ordering::Acquire);
                        let sc = sched_for_cpu(cpu);
                        sc.mark_running_thread_need_resched();
                        if cpu != self.cpu {
                            self.send_reschedule_ipi(cpu);
                        }
                    });
                    return;
                }
                State::Running => {
                    without_interrupts(|| {
                        thread.mark_need_resched();
                        let cpu = thread.cpu.load(Ordering::Acquire);
                        if cpu != self.cpu {
                            self.send_reschedule_ipi(cpu);
                        }
                    });
                    retries += 1;
                    if retries >= MAX_RETRIES {
                        // IPI sent, the target CPU will preempt eventually.
                        return;
                    }
                    spin_loop();
                }
                State::Dying => return,
                _ => {
                    retries += 1;
                    if retries >= MAX_RETRIES {
                        return;
                    }
                    spin_loop();
                }
            }
        }
    }

    #[inline(never)]
    fn wake_thread_from_irq(&self, thread: Arc<Thread>, priority: WakePriority) {
        let state = State::from(thread.state.load(Ordering::Acquire));
        let cpu = thread.cpu.load(Ordering::Acquire);
        match state {
            State::Sleeping | State::Parked => {
                if thread.try_wake() {
                    self.complete_wake(&thread, priority);
                    return;
                }
            }
            State::Ready | State::Waking => {
                let sc = sched_for_cpu(cpu);
                sc.mark_running_thread_need_resched();
            }
            State::Running => {
                thread.mark_need_resched();
            }
            State::Dying => return,
        }

        if cpu != self.cpu {
            self.send_reschedule_ipi(cpu);
        }
    }

    pub fn thread_exit(&self, code: i32) -> ! {
        let tid = self.current_thread_id().unwrap();

        // Fast path with interrupts disabled: mark Dying, detach from
        // per-CPU, enqueue on reaper for deferred cleanup, context_switch.
        // Heavy cleanup (free, unmap, etc.) happens in the reaper thread
        // with interrupts enabled.
        without_interrupts(|| unsafe {
            self.current.store(0, Ordering::Release);
            get_percpu_data().set_current_thread(None);

            if let Some(t) = get_thread_by_id(tid) {
                t.exit_code.store(code, Ordering::Release);
                t.state.store(State::Dying as u8, Ordering::Release);
                self.thread_count.fetch_sub(1, Ordering::Relaxed);
                reaper_enqueue(t);
            }

            context_switch();
        });
        loop {
            enable_and_hlt();
        }
    }

    pub fn current_thread_info(&self) -> Arc<IrqSpinlock<UserThreadInfo>> {
        THREADS
            .get_info(self.current_thread_id().unwrap())
            .clone()
            .unwrap()
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

#[unsafe(no_mangle)]
pub extern "C" fn schedule(context: *mut CpuContext) -> *mut CpuContext {
    /*
        User -> User:       Update RSP0 to new process's kernel stack
        User -> Kernel:     RSP0 doesn't matter
        Kernel -> User:     Must update RSP0 to user's kernel stack
        Kernel -> Kernel:   RSP0 doesn't matter
    */

    if context.is_null() {
        panic!("null context ptr");
    }

    if (context as u64) < 0xFFFF_0000_0000_0000u64 {
        panic!("Low context address {context:p}");
    }

    if !context.is_aligned() {
        panic!("Misaligned context: {context:p}");
    }

    let cpu = get_percpu_data();

    let sched: &'static Scheduler = unsafe { cpu.scheduler.get().as_ref().unwrap() };

    sched.on_tick(context);

    context
}

// ---------------------------------------------------------------------------
// Reaper — deferred cleanup of dead threads
// ---------------------------------------------------------------------------

/// Queue of dead threads awaiting cleanup. Lock-free, allocation-free on push.
static REAPER_QUEUE: Once<ArrayQueue<Arc<Thread>>> = Once::new();
static REAPER_TID: AtomicU64 = AtomicU64::new(0);

/// Initialize the reaper subsystem. Call once from the BSP after scheduler init.
pub fn init_reaper() {
    REAPER_QUEUE.call_once(|| ArrayQueue::new(256));

    let tid =
        crate::thread::util::queue_spawn_kthread_named("reaper", reaper_thread as *const () as u64);
    REAPER_TID.store(tid.0, Ordering::Release);
    println!("Reaper thread started (tid={})", tid.0);
}

fn reaper_queue() -> &'static ArrayQueue<Arc<Thread>> {
    REAPER_QUEUE.call_once(|| ArrayQueue::new(256))
}

extern "C" fn reaper_thread() -> ! {
    loop {
        sched().thread_park_while(|| reaper_queue().is_empty());

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
    // Wake the reaper thread.
    let reaper_tid = REAPER_TID.load(Ordering::Acquire);
    if reaper_tid != 0 {
        sched().wake_thread_irq(ThreadId(reaper_tid), WakePriority::Normal);
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

// Must be called without interrupts enabled.
#[unsafe(naked)]
pub unsafe extern "C" fn context_switch() {
    core::arch::naked_asm!(
        // Layout wanted at [rsp]:
        // [ GPRs: r15..rax ] (15*8 bytes)  then  [ IF: RIP,CS,RFLAGS,RSP,SS ] (5*8 bytes)
        // Reserve space up front so we can store originals without clobbering them.
        "sub rsp, 160",                   // 120 + 40

        // ---- store original GPRs into the reserved block (no clobber) ----
        "mov [rsp +   0], r15",
        "mov [rsp +   8], r14",
        "mov [rsp +  16], r13",
        "mov [rsp +  24], r12",
        "mov [rsp +  32], r11",
        "mov [rsp +  40], r10",
        "mov [rsp +  48], r9",
        "mov [rsp +  56], r8",
        "mov [rsp +  64], rdi",
        "mov [rsp +  72], rsi",
        "mov [rsp +  80], rbp",
        "mov [rsp +  88], rbx",
        "mov [rsp +  96], rdx",
        "mov [rsp + 104], rcx",
        "mov [rsp + 112], rax",

        // ---- build synthetic interrupt frame at [rsp + 120] ----
        // RIP
        "lea rax, [rip + .Lresume]",
        "mov [rsp + 120], rax",
        // CS (use your kernel code selector constant)
        "mov eax, {KCS}",
        "mov [rsp + 128], rax",
        // RFLAGS
        "pushfq",
        "pop rax",
        "mov [rsp + 136], rax",
        // RSP (original before the 160-byte reservation) = rsp + 160
        "lea rax, [rsp + 160]",
        "mov [rsp + 144], rax",
        // SS (use your kernel data selector constant)
        "mov eax, {KSS}",
        "mov [rsp + 152], rax",

        // rdi = &CpuContext (points to r15 field)
        "mov rdi, rsp",

        // 16-byte align for call
        "sub rsp, 8",
        "and rsp, -16",
        "cld",
        "call {timer_schedule}",

        // Switch to returned context and exit like IRQ path
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

        ".Lresume:",
        "ret",

        timer_schedule = sym schedule,
        KCS = const 0x08,
        KSS = const 0x10,
    );
}

// Note: heap allocs are fine because they are mapped before any user thread is created.
// In the future consider syncing pages.
#[inline]
pub fn switch_to_kernel_page() {
    let kernel_cr3 = boot_info().cr3;
    if Cr3::read().0.start_address() != kernel_cr3.0.start_address() {
        unsafe { Cr3::write(kernel_cr3.0, kernel_cr3.1) };
    }
    tlb_flush_all_including_global();
}
