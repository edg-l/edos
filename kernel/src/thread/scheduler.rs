use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use alloc::{boxed::Box, sync::Arc};
use crossbeam_queue::ArrayQueue;
use heapless::{LinearMap, index_map::FnvIndexMap};
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr,
    instructions::interrupts::{enable, enable_and_hlt, without_interrupts},
    registers::control::Cr3,
};

use crate::{
    apic::get_lapic,
    boot::boot_info,
    drivers::fpu::{init_fpu_state, restore_fpu_state, save_fpu_state},
    interrupts::InterruptIndex,
    logs::ThreadLogger,
    println,
    smp::tlb_flush_all_including_global,
    thread::{Thread, ThreadState, UserThreadInfo, context::CpuContext},
    timer::Instant,
    util::per_cpu::get_percpu_data,
};

// tid -> lapic that owns it
pub static ALIVE_THREADS: RwLock<FnvIndexMap<u64, u32, 1024>> = RwLock::new(FnvIndexMap::new());
pub static EXITED_THREADS: RwLock<FnvIndexMap<u64, i32, 1024>> = RwLock::new(FnvIndexMap::new());

pub static SCHEDULERS: RwLock<heapless::LinearMap<u32, &'static Scheduler, 128>> =
    RwLock::new(heapless::LinearMap::new());

/// Returns the scheduler id this thread lives on.
#[allow(unused)]
pub fn thread_exists(tid: u64) -> Option<u32> {
    ALIVE_THREADS.read().get(&tid).copied()
}

pub fn thread_sched(id: u64) -> Option<&'static Scheduler> {
    let sched_id = thread_exists(id)?;
    SCHEDULERS.read().get(&sched_id).cloned()
}

#[derive(Debug)]
pub enum SchedCmd {
    Wake(u64, bool),
    //Migrate(ThreadId, u64), // u64 = cpu
    //SetPrio(ThreadId, u8),
    WaitTimeout(u64, Instant, Duration),
    Wait(u64),
    Exit(u64, i32),
}

pub struct Scheduler {
    thread_queue: ArrayQueue<u64>,
    thread_priority_queue: ArrayQueue<u64>,
    pub cmd_queue: ArrayQueue<SchedCmd>,
    pub thread_spawn_queue: ArrayQueue<(Thread, Option<UserThreadInfo>)>,
    pub storage: Storage,
    pub current_tid: Option<u64>,
    pub current_logger: Option<Arc<ThreadLogger>>,
    pub lapic_id: u32,
    pub thread_count: AtomicU64,
    pub idling: bool,
    pub last_tick: Instant,
}

pub struct Storage {
    pub threads: heapless::LinearMap<u64, Thread, 256>,
    pub thread_info: heapless::LinearMap<u64, Arc<Mutex<UserThreadInfo>>, 256>,
}

pub fn init() {
    println!("Initializing scheduler");
    // TODO: refactor queue, so it isnt limited to 65k? maybe iterate on the storage threads
    // and use the queue as a priority queue, or that a threadid that is just a u32 or u16 and the queue is just of u16,
    // this would need to add a different pid for user threads alongside thread id and a mapping.
    let lapic_id = unsafe { get_lapic().id() };
    let sched = Box::new(Scheduler {
        thread_queue: ArrayQueue::new(500),
        thread_priority_queue: ArrayQueue::new(500),
        thread_spawn_queue: ArrayQueue::new(64),
        cmd_queue: ArrayQueue::new(128),
        storage: Storage {
            threads: LinearMap::new(),
            thread_info: LinearMap::new(),
        },
        current_tid: None,
        current_logger: None,
        lapic_id,
        thread_count: AtomicU64::new(0),
        idling: false,
        last_tick: Instant::now(),
    });

    let ptr: &'static mut _ = Box::leak(sched);
    get_percpu_data().scheduler = ptr;
    let _ = SCHEDULERS.write().insert(lapic_id, ptr);
    println!("Saved scheduler on percpu");
}

pub fn sched() -> &'static Scheduler {
    unsafe {
        get_percpu_data()
            .scheduler
            .as_mut()
            .expect("failed to get sched()")
    }
}

// This function will be called from assembly with pointer to saved context
#[unsafe(no_mangle)]
pub extern "C" fn timer_schedule(context: *mut CpuContext) -> *mut CpuContext {
    unsafe {
        get_lapic().end_of_interrupt();
        schedule(context)
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
        let sched = cpu.scheduler.as_mut().expect("failed to get scheduler");

        let elapsed = sched.last_tick.elapsed();

        sched.process_spawn_queue();
        sched.process_cmds();

        // Push current thread to queue, it's state will be processed then.
        if let Some(current_id) = sched.current_id_opt() {
            sched.current_tid = None;
            sched.current_logger = None;

            if let Some(thread) = sched.storage.threads.get_mut(&current_id) {
                if elapsed > Duration::from_nanos(5100000) {
                    println!("Thread {:?} took more than 5ms {:?}", thread.name, elapsed);
                }

                thread.context = (*context).clone();
                if let Some(user) = &mut thread.user {
                    if !user.fpu_init {
                        init_fpu_state(&mut user.fpu);
                        user.fpu_init = true;
                    } else {
                        save_fpu_state(&mut user.fpu);
                    }
                }
                if !thread.is_queued.swap(true, Ordering::AcqRel) {
                    sched
                        .thread_queue
                        .push(current_id)
                        .expect("failed to push current_id thread");
                }
            }
        }

        sched.schedule_next();

        if let Some(current_id) = sched.current_id_opt() {
            if let Some(thread) = sched.storage.threads.get_mut(&current_id) {
                if sched.idling {
                    println!("Stopped idling: {:?}", thread.name);
                }
                sched.idling = false;
                thread.is_queued.store(false, Ordering::SeqCst);
                *context = thread.context.clone();
                thread.switch_to_page();

                if let Some(user) = &mut thread.user {
                    if !user.fpu_init {
                        init_fpu_state(&mut user.fpu);
                        user.fpu_init = true;
                    } else {
                        restore_fpu_state(&user.fpu);
                    }
                    // Set RSP0
                    cpu.tss.privilege_stack_table[0] = VirtAddr::new(thread.initial_kstack_top);

                    // set kernel gs stack
                    cpu.kernel_rsp = thread.initial_kstack_top;
                    cpu.user_rsp = thread.context.interrupt_stack_frame.stack_pointer.as_u64();
                }
                sched.last_tick = Instant::now();
                return context;
            } else {
                println!("Thread not found!");
            }
        }

        if !sched.idling {
            println!("Entering idling");
        }
        sched.idling = true;

        sched.last_tick = Instant::now();

        // No tasks, return to idle loop.
        (*context).interrupt_stack_frame.instruction_pointer = VirtAddr::new(idle_loop as u64);
        context
    }
}

pub extern "C" fn idle_loop() -> ! {
    loop {
        enable_and_hlt();
    }
}

#[expect(unused)]
impl Scheduler {
    fn process_spawn_queue(&mut self) {
        let cpuidx = self.lapic_id;

        {
            let mut threads_alive = ALIVE_THREADS.write();

            while let Some((thread, info)) = self.thread_spawn_queue.pop() {
                thread.is_queued.store(true, Ordering::SeqCst);
                self.thread_queue.push(thread.id);
                threads_alive.insert(thread.id, cpuidx);
                if let Some(info) = info {
                    self.storage
                        .thread_info
                        .insert(thread.id, Arc::new(Mutex::new(info)));
                }

                self.storage.threads.insert(thread.id, thread);
                self.thread_count.fetch_add(1, Ordering::Release);
            }
        }
    }

    fn process_cmds(&mut self) {
        while let Some(cmd) = self.cmd_queue.pop() {
            match cmd {
                SchedCmd::Wake(thread_id, prio) => {
                    if let Some(thread) = self.storage.threads.get_mut(&thread_id)
                        && thread.state != ThreadState::Ready
                        && !matches!(thread.state, ThreadState::Exited(_))
                    {
                        thread.state = ThreadState::Ready;
                        if !thread.is_queued.swap(true, Ordering::AcqRel) {
                            if prio {
                                self.thread_priority_queue.push(thread_id);
                            } else {
                                self.thread_queue.push(thread_id);
                            }
                        }
                    }
                }
                SchedCmd::WaitTimeout(thread_id, instant, duration) => {
                    if let Some(thread) = self.storage.threads.get_mut(&thread_id)
                        && !matches!(thread.state, ThreadState::Exited(_))
                    {
                        thread.state = ThreadState::WaitTimeout((instant, duration));
                    }
                }
                SchedCmd::Wait(thread_id) => {
                    if let Some(thread) = self.storage.threads.get_mut(&thread_id)
                        && !matches!(thread.state, ThreadState::Exited(_))
                    {
                        thread.state = ThreadState::Waiting;
                    }
                }
                SchedCmd::Exit(thread_id, code) => {
                    if let Some(thread) = self.storage.threads.get_mut(&thread_id) {
                        thread.state = ThreadState::Exited(code);
                        {
                            let mut exited = EXITED_THREADS.write();
                            let _ = exited.insert(thread_id, code);
                        }
                        let info = self.storage.thread_info.remove(&thread.id);
                        thread.free(info);

                        self.thread_count.fetch_sub(1, Ordering::Release);
                        ALIVE_THREADS.write().remove(&thread_id);
                    }
                }
            }
        }
    }

    /// Schedules the next thread id, updating current_thread_id.
    fn schedule_next(&mut self) -> bool {
        let now = Instant::now();
        self.current_tid = None;
        self.current_logger = None;

        // TODO: dedup this code
        while let Some(id) = self.thread_priority_queue.pop() {
            if let Some(thread) = self.storage.threads.get_mut(&id) {
                match thread.state {
                    ThreadState::Ready => {}
                    ThreadState::Waiting => {
                        thread.is_queued.store(false, Ordering::Release);
                        continue;
                    }
                    ThreadState::WaitTimeout((start, timeout)) => {
                        if now.duration_since(start) >= timeout {
                            thread.state = ThreadState::Ready;
                        } else {
                            self.thread_priority_queue.push(id);
                            continue;
                        }
                    }
                    ThreadState::Exited(code) => {
                        thread.is_queued.store(false, Ordering::Release);
                        continue;
                    }
                }
                thread.is_queued.store(false, Ordering::Release);
                self.current_tid = Some(id);
                self.current_logger = Some(thread.logger.clone());
                return true;
            }
        }

        let mut limit = self.thread_queue.len();

        while limit > 0
            && let Some(id) = self.thread_queue.pop()
        {
            limit -= 1;
            if let Some(thread) = self.storage.threads.get_mut(&id) {
                match thread.state {
                    ThreadState::Ready => {}
                    ThreadState::Waiting => {
                        thread.is_queued.store(false, Ordering::Release);
                        continue;
                    }
                    ThreadState::WaitTimeout((start, timeout)) => {
                        if now.duration_since(start) >= timeout {
                            thread.state = ThreadState::Ready;
                        } else {
                            self.thread_queue.push(id);
                            continue;
                        }
                    }
                    ThreadState::Exited(code) => {
                        thread.is_queued.store(false, Ordering::Release);
                        continue;
                    }
                }
                thread.is_queued.store(false, Ordering::Release);
                self.current_tid = Some(id);
                self.current_logger = Some(thread.logger.clone());
                return true;
            }
        }

        for (id, thread) in self.storage.threads.iter_mut() {
            match thread.state {
                ThreadState::Ready => {
                    println!("Found a ready thread not in queue: {:?}", thread.name);
                    self.current_tid = Some(*id);
                    self.current_logger = Some(thread.logger.clone());
                    return true;
                }
                ThreadState::Waiting => {}
                ThreadState::WaitTimeout((start, timeout)) => {
                    if now.duration_since(start) >= timeout {
                        println!(
                            "Found a WaitTimeout thread that is ready not in queue: {:?}",
                            thread.name
                        );
                        thread.state = ThreadState::Ready;
                        self.current_tid = Some(*id);
                        self.current_logger = Some(thread.logger.clone());
                    }
                }
                ThreadState::Exited(_) => {}
            }
        }

        self.current_tid = None;
        self.current_logger = None;
        false
    }

    /// Current thread id.
    pub fn current_id(&self) -> u64 {
        self.current_tid.expect("current id is none")
    }

    pub fn current_thread_info(&self) -> Arc<Mutex<UserThreadInfo>> {
        let id = self.current_id();
        self.storage.thread_info.get(&id).unwrap().clone()
    }

    pub fn current_id_opt(&self) -> Option<u64> {
        self.current_tid
    }

    /// Gets the current thread logger. Locks momentarely to acquire the logger.
    pub fn get_logger(&self) -> Arc<ThreadLogger> {
        self.current_logger.clone().unwrap()
    }

    // TODO: this checks only current cpu scheduler, so it returns true
    pub fn thread_exists(&self, id: u64) -> bool {
        thread_exists(id).is_some()
    }

    /// Wake the given thread
    pub fn thread_wake(&self, id: u64, priority: bool) {
        without_interrupts(|| {
            if let Some(sched) = thread_sched(id) {
                sched.cmd_queue.push(SchedCmd::Wake(id, priority));
                self.send_reschedule_ipi(sched.lapic_id);
            } else {
                self.cmd_queue.push(SchedCmd::Wake(id, priority));
            }
        })
    }

    /// Sets the given thread as waiting for a maximum of the given timeout.
    pub fn thread_set_wait_timeout(&self, id: u64, timeout: Duration) {
        without_interrupts(|| {
            let now = Instant::now();
            if let Some(sched) = thread_sched(id) {
                sched
                    .cmd_queue
                    .push(SchedCmd::WaitTimeout(id, now, timeout));
                self.send_reschedule_ipi(sched.lapic_id);
            } else {
                self.cmd_queue.push(SchedCmd::WaitTimeout(id, now, timeout));
            }
        })
    }

    pub fn thread_exit(&self, code: i32) {
        let id = self.current_id();
        self.cmd_queue.push(SchedCmd::Exit(id, code));
        without_interrupts(|| {
            unsafe { context_switch() };
        });
    }

    /// The caller thread gets put in a wait (and yields execution) until timeout or another thread wakes it.
    pub fn thread_wait_timeout(&self, timeout: Duration) {
        let id = self.current_id();
        without_interrupts(|| {
            let now = Instant::now();
            self.cmd_queue.push(SchedCmd::WaitTimeout(id, now, timeout));
            unsafe { context_switch() };
        });
    }

    pub fn thread_park(&self) {
        without_interrupts(|| {
            let id = self.current_id();
            self.cmd_queue.push(SchedCmd::Wait(id));
            unsafe { context_switch() };
        });
    }

    /// The caller thread sleeps for the given duration.
    #[inline]
    pub fn thread_sleep(&self, duration: Duration) {
        self.thread_wait_timeout(duration);
    }

    /// Cooperatively yield.
    pub fn thread_yield(&self) {
        without_interrupts(|| {
            unsafe { context_switch() };
        });
    }

    pub fn send_reschedule_ipi(&self, target_cpu: u32) {
        without_interrupts(|| unsafe {
            get_lapic().send_ipi(InterruptIndex::Reschedule as u8, target_cpu);
        });
    }
}

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
