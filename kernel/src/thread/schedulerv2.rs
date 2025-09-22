use core::{
    cmp,
    sync::atomic::{AtomicU64, Ordering},
};

use alloc::sync::Arc;
use crossbeam_queue::ArrayQueue;
use heapless::{BinaryHeap, Deque, LinearMap, binary_heap::Min};
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr,
    instructions::interrupts::{enable_and_hlt, without_interrupts},
};

use crate::{
    apic::get_lapic,
    drivers::fpu::{init_fpu_state, restore_fpu_state, save_fpu_state},
    interrupts::InterruptIndex,
    thread::{
        context::CpuContext,
        threadv2::{Flags, State, THREADS, Thread, ThreadId, get_thread_by_id},
    },
    timer::Instant,
    util::per_cpu::get_percpu_data,
};

pub fn sched() -> &'static Scheduler {
    unsafe { todo!() }
}

#[derive(Debug)]
pub enum SchedCmd {
    New(Arc<Thread>),
    Wake(ThreadId, /*high_prio:*/ bool),
    SleepUntil(ThreadId, u64),
    Park(ThreadId),
    Exit(ThreadId),
    SetAffinity(ThreadId, u32),
    SetPriority(ThreadId, u8),
    Yield(ThreadId),
}

pub struct Scheduler {
    pub cpu: u32,

    // Ready queue. Simple round-robin with priority buckets is enough.
    // Keep it small and predictable.
    pub rq: Mutex<heapless::Deque<Arc<Thread>, 1024>>,

    // Current running thread id for this CPU.
    pub current: AtomicU64, // 0 means idle

    // Command queue visible to syscalls/IRQs on any CPU.
    pub cmds: Arc<ArrayQueue<SchedCmd>>,

    // Time accounting
    pub default_timeslice: u32,

    sleepers: Mutex<BinaryHeap<SleepEntry, Min, 1024>>,
}

impl Scheduler {
    pub fn new(cpu: u32) -> Self {
        Self {
            cpu,
            rq: Mutex::new(Deque::new()),
            current: AtomicU64::new(0),
            cmds: Arc::new(ArrayQueue::new(1024)),
            default_timeslice: 8, // ticks
            sleepers: Mutex::new(BinaryHeap::new()),
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
        let tid = self.current.load(Ordering::Acquire);
        if tid == 0 {
            return None;
        }
        // Provide a lookup: per-CPU "running" pointer or global map.
        // Keep a per-CPU pointer for O(1).
        self.get_thread_by_id(ThreadId(tid))
    }

    pub fn on_tick(&self, context: *mut CpuContext) {
        self.drain_cmds();

        // Decrement timeslice for current thread and flag preempt if expired.
        if let Some(cur) = self.current_thread() {
            let left = cur
                .timeslice_ticks
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1))
                .ok();
            if left == Some(0) {
                cur.flags
                    .fetch_or(Flags::NEED_RESCHED.bits(), Ordering::AcqRel);
            }
        }

        self.wake_sleepers();
        self.maybe_preempt(context);
    }

    fn wake_sleepers(&self) {
        // O(n) option: scan rq + a small sleep list per CPU.
        // Minimal variant: rely on SchedCmd::Wake from timer or devices.
        // If you keep a per-CPU min-heap of sleepers, check head only.
        // Shown here as a no-op placeholder to keep the core minimal.
        let now = Instant::now().tick();
        let mut sl = self.sleepers.lock();

        while let Some(top) = sl.peek() {
            if top.deadline > now {
                break; // earliest sleeper not ready yet
            }
            let entry = sl.pop().unwrap();
            let t = entry.thread;

            t.state.store(State::Ready as u8, Ordering::Release);
            let mut rq = self.rq.lock();
            rq.push_back(t).expect("failed to push back");
        }
    }

    fn get_thread_by_id(&self, id: ThreadId) -> Option<Arc<Thread>> {
        get_thread_by_id(id)
    }

    fn run_idle(&self) {
        // Mark CPU idle. HLT until next interrupt.
        self.current.store(0, Ordering::Release);
        loop {
            enable_and_hlt();
        }
    }

    fn drain_cmds(&self) {
        // Apply all pending external changes with minimal lock time.
        while let Some(cmd) = self.cmds.pop() {
            match cmd {
                SchedCmd::New(t) => {
                    t.state.store(State::Ready as u8, Ordering::Release);
                    t.timeslice_ticks
                        .store(self.default_timeslice, Ordering::Release);
                    if self.thread_can_run_here(&t) {
                        let mut rq = self.rq.lock();
                        rq.push_back(t).expect("failed to push back");
                    } else {
                        // will be queued on its target cpu by that cpu’s scheduler
                    }
                }
                SchedCmd::Wake(tid, high) => {
                    if let Some(t) = self.get_thread_by_id(tid) {
                        let old = t.state.swap(State::Ready as u8, Ordering::AcqRel);
                        if State::from(old) != State::Ready {
                            let mut rq = self.rq.lock();
                            if high {
                                rq.push_front(t).expect("failed to push front");
                            } else {
                                rq.push_back(t).expect("failed to push back");
                            }
                        }
                    }
                }
                SchedCmd::SleepUntil(tid, dl) => {
                    if let Some(t) = self.get_thread_by_id(tid) {
                        t.state.store(State::Sleeping as u8, Ordering::Release);
                        t.sleep_deadline.store(dl, Ordering::Release);
                        let mut sl = self.sleepers.lock();
                        sl.push(SleepEntry {
                            deadline: dl,
                            thread: t,
                        })
                        .unwrap();
                    }
                }
                SchedCmd::Park(tid) => {
                    if let Some(t) = self.get_thread_by_id(tid) {
                        t.state.store(State::Parked as u8, Ordering::Release);
                    }
                }
                SchedCmd::Exit(tid) => {
                    if let Some(t) = self.get_thread_by_id(tid) {
                        t.state.store(State::Dying as u8, Ordering::Release);
                        // cleanup deferred out of IRQ
                    }
                }
                SchedCmd::SetAffinity(tid, m) => {
                    if let Some(t) = self.get_thread_by_id(tid) {
                        t.cpu_affinity.store(m, Ordering::Release);
                        t.mark_need_resched();
                    }
                }
                SchedCmd::SetPriority(tid, p) => {
                    if let Some(t) = self.get_thread_by_id(tid) {
                        t.set_priority(p);
                    }
                }
                SchedCmd::Yield(tid) => {
                    if let Some(t) = self.get_thread_by_id(tid) {
                        t.flags
                            .fetch_or(Flags::NEED_RESCHED.bits(), Ordering::AcqRel);
                    }
                }
            }
        }
    }

    pub fn maybe_preempt(&self, context: *mut CpuContext) {
        let Some(cur) = self.current_thread() else {
            self.pick_and_run(context); // was idle
            return;
        };

        // Fast check without locking the runqueue.
        let need = cur.flags.load(Ordering::Acquire) & Flags::NEED_RESCHED.bits() != 0;
        if !need {
            return;
        }

        // Clear request and pick another.
        cur.flags
            .fetch_and(!Flags::NEED_RESCHED.bits(), Ordering::AcqRel);

        // Requeue current if still runnable.
        let st = cur.state();
        if st == State::Running {
            cur.state.store(State::Ready as u8, Ordering::Release);
            let mut rq = self.rq.lock();
            rq.push_back(cur.clone()).expect("failed to push back");
        }

        self.pick_and_run(context);
    }

    fn pick_and_run(&self, context: *mut CpuContext) {
        let next = {
            let mut rq = self.rq.lock();
            // Simple RR. You can add N priority buckets if wanted.
            rq.pop_front()
        };

        match next {
            Some(t) => unsafe { self.context_switch_to(t, context) },
            None => self.run_idle(),
        }
    }

    fn thread_can_run_here(&self, t: &Thread) -> bool {
        let mask = t.cpu_affinity.load(Ordering::Acquire);
        mask == 0 || (mask & (1u32 << self.cpu)) != 0
    }

    unsafe fn context_switch_to(&self, next: Arc<Thread>, context: *mut CpuContext) {
        // Save current
        if let Some(current) = self.current_thread() {
            unsafe {
                *current.ctx.lock() = (*context).clone();
                if let Some(user) = &next.user {
                    let mut user = user.write();

                    if !user.fpu_init {
                        init_fpu_state(&mut user.fpu);
                        user.fpu_init = true;
                    } else {
                        save_fpu_state(&mut user.fpu);
                    }
                }
            }
        }

        // Set as current
        self.current.store(next.id.0, Ordering::Release);

        // Prepare next
        next.state.store(State::Running as u8, Ordering::Release);
        next.timeslice_ticks
            .store(self.default_timeslice, Ordering::Release);

        // Switch address space
        next.switch_to_page();
        unsafe { *context = next.ctx.lock().clone() };
        if let Some(user) = &next.user {
            let mut user = user.write();
            unsafe {
                if !user.fpu_init {
                    init_fpu_state(&mut user.fpu);
                    user.fpu_init = true;
                } else {
                    restore_fpu_state(&user.fpu);
                }
            }
        }

        let cpu = get_percpu_data();
        // Set RSP0
        cpu.tss.privilege_stack_table[0] = VirtAddr::new(next.kstack_top);

        // set kernel gs stack
        cpu.kernel_rsp = next.kstack_top;
        cpu.user_rsp = next.ctx.lock().interrupt_stack_frame.stack_pointer.as_u64();
        // return handles context switch
    }

    pub fn send_reschedule_ipi(&self, target_cpu: u32) {
        unsafe { get_lapic().send_ipi(InterruptIndex::Reschedule as u8, target_cpu) };
    }

    // External messaging to sched

    #[inline]
    pub fn yield_now(&self, tid: ThreadId) {
        self.cmds.push(SchedCmd::Yield(tid));
        // local CPU: set NEED_RESCHED and return to kernel exit path
        if let Some(t) = get_thread_by_id(tid) {
            t.mark_need_resched();
        }

        if Some(tid) == self.current_thread_id() {
            without_interrupts(|| unsafe {
                context_switch();
            })
        }
    }

    #[inline]
    pub fn sleep_for_ticks(&self, tid: ThreadId, dt: u64) {
        let deadline = Instant::now().tick().saturating_add(dt);
        self.cmds.push(SchedCmd::SleepUntil(tid, deadline)).unwrap();
        if let Some(t) = get_thread_by_id(tid) {
            t.mark_need_resched();
        }
    }

    #[inline]
    pub fn set_priority(&self, tid: ThreadId, prio: u8) {
        self.cmds.push(SchedCmd::SetPriority(tid, prio));
        if let Some(t) = get_thread_by_id(tid) {
            t.mark_need_resched();
        }
    }

    #[inline]
    pub fn set_affinity_mask(&self, tid: ThreadId, mask: u32) {
        self.cmds.push(SchedCmd::SetAffinity(tid, mask));
        // If the thread is on another CPU, kick it.
        if let Some(t) = get_thread_by_id(tid) {
            t.mark_need_resched();
            // Optional: send IPI to its current CPU if known.
        }
    }

    pub fn park_thread(&self, tid: ThreadId) {
        self.cmds.push(SchedCmd::Park(tid));
        if let Some(t) = get_thread_by_id(tid) {
            t.mark_need_resched();
        }

        if Some(tid) == self.current_thread_id() {
            without_interrupts(|| unsafe {
                context_switch();
            })
        }
    }

    pub fn wake_thread(&self, tid: ThreadId, high: bool) {
        self.cmds.push(SchedCmd::Wake(tid, high));
        self.send_reschedule_ipi(self.cpu); // kick target CPU if needed
    }
}

pub fn exit_thread(tid: ThreadId) {
    if let Some(t) = THREADS.get(tid) {
        t.state.store(State::Dying as u8, Ordering::Release);
        THREADS.remove(tid);
        t.free();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schedule(context: *mut CpuContext) -> *mut CpuContext {
    unsafe {
        /*
            User -> User:       Update RSP0 to new process's kernel stack
            User -> Kernel:     RSP0 doesn't matter
            Kernel -> User:     Must update RSP0 to user's kernel stack
            Kernel -> Kernel:   RSP0 doesn't matter
        */

        let cpu = get_percpu_data();
        // let sched = cpu.scheduler.as_mut().expect("failed to get scheduler");

        let sched: &'static Scheduler = todo!();

        sched.on_tick(context);

        context
    }
}

// move to file

#[derive(Debug, Clone)]
struct SleepEntry {
    deadline: u64,
    thread: Arc<Thread>,
}

// Reverse ordering so BinaryHeap becomes min-heap on deadline
impl Ord for SleepEntry {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        other.deadline.cmp(&self.deadline)
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
