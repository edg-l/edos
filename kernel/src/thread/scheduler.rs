use core::{
    cmp,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use alloc::{boxed::Box, sync::Arc};
use crossbeam_queue::ArrayQueue;
use heapless::{BinaryHeap, Deque, binary_heap::Max};
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr,
    instructions::interrupts::{disable, enable, enable_and_hlt, without_interrupts},
    registers::control::Cr3,
};

use crate::{
    apic::{get_lapic, set_apic_timer, set_apic_timer_and_enable},
    boot::boot_info,
    drivers::fpu::{init_fpu_state, restore_fpu_state, save_fpu_state},
    interrupts::InterruptIndex,
    log, println,
    smp::tlb_flush_all_including_global,
    thread::{
        UserThreadInfo,
        context::CpuContext,
        thread::{Flags, State, THREADS, Thread, ThreadId, get_thread_by_id},
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
    get_percpu_data().scheduler = ptr;
    let _ = SCHEDULERS.write().insert(lapic_id, ptr);
    println!("Saved scheduler on percpu");
    // Enable apic timer
    set_apic_timer_and_enable(Duration::from_millis(100));
}

pub fn sched() -> &'static Scheduler {
    unsafe {
        get_percpu_data()
            .scheduler
            .as_mut()
            .expect("failed to get sched()")
    }
}
pub static SCHEDULERS: RwLock<heapless::LinearMap<u32, &'static Scheduler, 128>> =
    RwLock::new(heapless::LinearMap::new());

fn sched_for_cpu(cpu: u32) -> &'static Scheduler {
    SCHEDULERS.read().get(&cpu).expect("cpu sched")
}

pub struct Scheduler {
    pub cpu: u32,

    // Ready queue. Simple round-robin with priority buckets is enough.
    // Keep it small and predictable.
    pub rq: Mutex<heapless::Deque<Arc<Thread>, 1024>>,

    // Current running thread id for this CPU.
    pub current: AtomicU64, // 0 means idle

    // Time accounting
    pub default_timeslice: Duration,

    sleepers: Mutex<BinaryHeap<SleepEntry, Max, 1024>>,

    pub earliest_deadline: AtomicU64,

    pub thread_count: AtomicU64,

    has_work: AtomicBool,
}

impl Scheduler {
    pub fn new(cpu: u32) -> Self {
        println!("New scheduler");
        Self {
            cpu,
            rq: Mutex::new(Deque::new()),
            current: AtomicU64::new(0),
            default_timeslice: Duration::from_millis(5),
            sleepers: Mutex::new(BinaryHeap::new()),
            thread_count: AtomicU64::new(0),
            earliest_deadline: AtomicU64::new(u64::MAX),
            has_work: AtomicBool::new(false),
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
        get_percpu_data().current_thread.clone()
    }

    pub fn current_thread_external(&self) -> Option<Arc<Thread>> {
        let tid = self.current.load(Ordering::Acquire);
        if tid == 0 {
            return None;
        }
        // Provide a lookup: per-CPU "running" pointer or global map.
        // Keep a per-CPU pointer for O(1).
        self.get_thread_by_id(ThreadId(tid))
    }

    pub fn on_tick(&self, context: *mut CpuContext) {
        //self.earliest_deadline.store(u64::MAX, Ordering::Release);
        self.wake_sleepers();
        self.maybe_preempt(context);
    }

    fn wake_sleepers(&self) {
        let now = Instant::now().tick();
        let mut sl = self.sleepers.lock();
        let mut earliest = u64::MAX;
        while let Some(top) = sl.peek() {
            if top.deadline > now {
                earliest = top.deadline;
                break;
            }
            let t = sl.pop().unwrap().thread;
            if t.try_wake() {
                let mut rq = self.rq.lock();
                rq.push_back(t).unwrap();
                self.has_work.store(true, Ordering::Release);
            }
        }
        self.earliest_deadline.store(earliest, Ordering::Release);
    }

    fn get_thread_by_id(&self, id: ThreadId) -> Option<Arc<Thread>> {
        get_thread_by_id(id)
    }

    fn run_idle(&self) {
        // Mark CPU idle
        self.current.store(0, Ordering::Release);
        get_percpu_data().current_thread = None;

        self.has_work.store(false, Ordering::Release);
        enable();

        loop {
            // Break out if any work is available
            if self.has_work.load(Ordering::Acquire) {
                break;
            }

            let ed = self.earliest_deadline.load(Ordering::Acquire);
            if ed != u64::MAX && ed != 0 {
                let now = Instant::now();
                let dl = Instant::from_tick(ed);
                let dur = if ed <= now.tick() {
                    Duration::from_micros(1)
                } else {
                    dl.duration_since(now)
                };
                set_apic_timer(dur);
            } else {
                println!("Endless idle")
            }

            // Halt until next interrupt (timer, IPI, device)
            x86_64::instructions::interrupts::enable_and_hlt();
        }

        disable();
    }

    pub fn maybe_preempt(&self, context: *mut CpuContext) {
        let Some(cur) = self.current_thread() else {
            println!("Waking from idle");
            self.pick_and_run(context); // was idle
            return;
        };

        // Fast check without locking the runqueue.
        let need = cur.flags.load(Ordering::Acquire) & Flags::NEED_RESCHED.bits() != 0;
        if !need {
            println!("No resched needed for {:?}", cur.id);
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
                    let mut rq = self.rq.lock();
                    rq.push_back(cur.clone()).ok();
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
            let next = { self.rq.lock().pop_front() };

            match next {
                Some(t) => {
                    self.has_work
                        .store(!self.rq.lock().is_empty(), Ordering::Release);
                    if t.cas_state(State::Ready, State::Running) {
                        unsafe { self.context_switch_to(t, context) };
                        return;
                    } else {
                        continue; // invalid state, try again
                    }
                }
                None => {
                    self.run_idle();
                    // after idle returns, loop and try again
                    continue;
                }
            }
        }
    }

    fn thread_can_run_here(&self, t: &Thread) -> bool {
        true
        //let mask = t.cpu_affinity.load(Ordering::Acquire);
        //mask == 0 || (mask & (1u32 << self.cpu)) != 0
    }

    fn save_current_thread(&self, context: *mut CpuContext) {
        if let Some(current) = self.current_thread() {
            unsafe {
                *current.ctx.lock() = (*context).clone();
                if let Some(user) = &current.user {
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
    }

    unsafe fn context_switch_to(&self, next: Arc<Thread>, context: *mut CpuContext) {
        // Set as current
        self.current.store(next.id.0, Ordering::Release);
        get_percpu_data().current_thread = Some(next.clone());

        // Prepare next
        next.state.store(State::Running as u8, Ordering::Release);

        let now = Instant::now();
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

        unsafe { *context = next.ctx.lock().clone() };

        // Switch address space
        next.switch_to_page();
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
        println!("Sending reschedule ipi to {target_cpu}");
        without_interrupts(|| {
            unsafe { get_lapic().send_ipi(InterruptIndex::Reschedule as u8, target_cpu) };
        });
    }

    // External messaging to sched

    #[inline]
    pub fn spawn_thread(&self, thread: Arc<Thread>) {
        println!("Spawning");
        self.thread_count.fetch_add(1, Ordering::AcqRel);
        thread.state.store(State::Ready as u8, Ordering::Release);
        thread.cpu.store(self.cpu, Ordering::Release);
        if self.thread_can_run_here(&thread) {
            let mut rq = self.rq.lock();
            rq.push_back(thread).expect("failed to push back");
            self.has_work.store(true, Ordering::Release);
        } else {
            // will be queued on its target cpu by that cpu’s scheduler
        }

        if self.cpu != get_percpu_data().lapic_id {
            println!("Sending ipi due to spawn");
            sched().send_reschedule_ipi(self.cpu);
        }
    }

    #[inline]
    pub fn thread_yield(&self) {
        without_interrupts(|| unsafe {
            let Some(cur) = self.current_thread() else {
                return;
            };
            if cur.cas_state(State::Running, State::Ready) {
                self.rq.lock().push_back(cur).ok();
                self.has_work.store(true, Ordering::Release);
            }
            context_switch();
        })
    }

    pub fn thread_park(&self) {
        log!("parking");

        let Some(cur) = self.current_thread() else {
            return;
        };

        loop {
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
        }

        unsafe {
            cur.mark_need_resched();
            context_switch();
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
            cur.mark_need_resched();
            context_switch();
        })
    }

    pub fn wake_thread(&self, tid: ThreadId, high: bool) {
        if let Some(t) = get_thread_by_id(tid) {
            if !t.try_wake() {
                return;
            } // CAS handles dupe-enqueue
            let cpu = t.cpu.load(Ordering::Acquire);
            let sc = sched_for_cpu(cpu);
            without_interrupts(|| {
                {
                    let mut rq = sc.rq.lock();
                    if high {
                        rq.push_front(t.clone()).ok();
                    } else {
                        rq.push_back(t.clone()).ok();
                    }
                    sc.has_work.store(true, Ordering::Release);
                }
                t.state.store(State::Ready as u8, Ordering::Release);
                if cpu != self.cpu {
                    t.mark_need_resched();
                    self.send_reschedule_ipi(cpu);
                } else {
                    t.mark_need_resched();
                }
            })
        }
    }

    pub fn thread_exit(&self, code: i32) -> ! {
        let tid = self.current_thread_id().unwrap();
        if let Some(t) = get_thread_by_id(tid) {
            t.state.store(State::Dying as u8, Ordering::Release);
            self.thread_count.fetch_sub(1, Ordering::Relaxed);
        }
        unsafe {
            context_switch();
        }
        loop {
            enable_and_hlt();
        }
    }

    pub fn current_thread_info(&self) -> Arc<Mutex<UserThreadInfo>> {
        THREADS
            .get_info(self.current_thread_id().unwrap())
            .clone()
            .unwrap()
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

        let sched: &'static Scheduler = cpu.scheduler.as_ref().unwrap();

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
