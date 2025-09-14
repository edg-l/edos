use core::time::Duration;

use alloc::{boxed::Box, sync::Arc};
use crossbeam_queue::ArrayQueue;
use heapless::{LinearMap, index_map::FnvIndexMap};
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr,
    instructions::{
        hlt,
        interrupts::{enable_and_hlt, without_interrupts},
    },
    registers::control::Cr3,
};

use crate::{
    apic::get_lapic, boot::boot_info, drivers::fpu::{init_fpu_state, restore_fpu_state, save_fpu_state}, interrupts::InterruptIndex, logs::ThreadLogger, println, smp::tlb_flush_all_including_global, syscalls::set_gs_kernel_stack, thread::{
        context::CpuContext, user::{UserThread, UserThreadInfo}, KernelThread, ThreadId, ThreadState
    }, timer::Instant, util::per_cpu::get_percpu_data
};

// tid -> lapic that owns it
pub static ALIVE_THREADS: RwLock<FnvIndexMap<ThreadId, u32, 1024>> =
    RwLock::new(FnvIndexMap::new());

/// Returns the scheduler id this thread lives on.
#[allow(unused)]
pub fn thread_exists(tid: &ThreadId) -> Option<u32> {
    ALIVE_THREADS.read().get(tid).copied()
}

#[derive(Debug)]
pub enum SchedCmd {
    Wake(ThreadId, bool),
    //Migrate(ThreadId, u64), // u64 = cpu
    //SetPrio(ThreadId, u8),
    WaitTimeout(ThreadId, Instant, Duration),
    Wait(ThreadId),
    Exit(ThreadId, i32),
}

pub struct Scheduler {
    thread_queue: ArrayQueue<ThreadId>,
    thread_priority_queue: ArrayQueue<ThreadId>,
    pub cmd_queue: ArrayQueue<SchedCmd>,
    pub kthread_spawn_queue: ArrayQueue<KernelThread>,
    pub thread_spawn_queue: ArrayQueue<(UserThread, UserThreadInfo)>,
    pub storage: Storage,
    pub current_tid: Option<ThreadId>,
    pub current_logger: Option<Arc<ThreadLogger>>,
    pub lapic_id: u32,
}

pub struct Storage {
    pub kthreads: heapless::LinearMap<u64, KernelThread, 64>,
    pub threads: heapless::LinearMap<u64, UserThread, 256>,
    pub thread_info: heapless::LinearMap<u64, Arc<Mutex<UserThreadInfo>>, 256>,
}

pub fn init() {
    println!("Initializing scheduler");
    let sched = Box::new(Scheduler {
        thread_queue: ArrayQueue::new(1024),
        thread_priority_queue: ArrayQueue::new(256),
        kthread_spawn_queue: ArrayQueue::new(64),
        thread_spawn_queue: ArrayQueue::new(64),
        cmd_queue: ArrayQueue::new(128),
        storage: Storage {
            kthreads: LinearMap::new(),
            threads: LinearMap::new(),
            thread_info: LinearMap::new(),
        },
        current_tid: None,
        current_logger: None,
        lapic_id: unsafe { get_lapic().id() },
    });

    let ptr = Box::leak(sched);
    get_percpu_data().scheduler = ptr;
    println!("Saved scheduler on percpu");
}

pub fn sched() -> &'static Scheduler {
    unsafe { get_percpu_data().scheduler.as_mut().unwrap() }
}

// This function will be called from assembly with pointer to saved context
#[unsafe(no_mangle)]
pub extern "C" fn timer_schedule(context: *mut CpuContext) -> *mut CpuContext {
    unsafe {
        get_lapic().end_of_interrupt();

        /*
            User -> User:       Update RSP0 to new process's kernel stack
            User -> Kernel:     RSP0 doesn't matter
            Kernel -> User:     Must update RSP0 to user's kernel stack
            Kernel -> Kernel:   RSP0 doesn't matter
        */

        let cpu = get_percpu_data();
        let sched = cpu.scheduler.as_mut().unwrap();
        sched.process_spawn_queue();
        sched.process_cmds();

        // Push current thread to queue, it's state will be processed then.
        if let Some(current_id) = sched.current_id_opt() {
            sched.current_tid = None;
            sched.current_logger = None;
            if current_id.kernel {
                // coming from kernel task
                if let Some(kthread) = sched.storage.kthreads.get_mut(&current_id.id) {
                    kthread.context = (*context).clone();
                    sched.thread_queue.push(current_id).unwrap();
                }
            } else {
                // coming from user
                if let Some(thread) = sched.storage.threads.get_mut(&current_id.id) {
                    thread.context = (*context).clone();

                    if !thread.fpu_init {
                        init_fpu_state(&mut thread.fpu);
                        thread.fpu_init = true;
                    } else {
                        save_fpu_state(&mut thread.fpu);
                    }

                    sched.thread_queue.push(current_id).unwrap();
                }
            }
        }

        sched.schedule_next();

        if let Some(current_id) = sched.current_id_opt() {
            // serial_println!("Next id {:?}", current_id);

            if current_id.kernel {
                if let Some(kthread) = sched.storage.kthreads.get(&current_id.id) {
                    // going to kernel space.
                    // for now, always switch to kernel page, just in case.
                    switch_to_kernel_page();
                    *context = kthread.context.clone();
                    return context;
                }
            } else if let Some(thread) = sched.storage.threads.get_mut(&current_id.id) {
                // Going to user space.
                // Set page table
                thread.switch_to_page();

                *context = thread.context.clone();
                if !thread.fpu_init {
                    init_fpu_state(&mut thread.fpu);
                    thread.fpu_init = true;
                } else {
                    restore_fpu_state(&thread.fpu);
                }
                // Set RSP0
                cpu.tss.privilege_stack_table[0] = VirtAddr::new(thread.kernel_stack_top);

                // set kernel gs stack
                set_gs_kernel_stack(thread.kernel_stack_top);

                return context;
            }
        }

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
        let mut lock = ALIVE_THREADS.write();
        let cpuidx = self.lapic_id;
        while let Some(kthread) = self.kthread_spawn_queue.pop() {
            self.thread_queue.push(kthread.id.clone());
            lock.insert(kthread.id.clone(), cpuidx);
            self.storage.kthreads.insert(kthread.id.id, kthread);
        }

        while let Some((thread, info)) = self.thread_spawn_queue.pop() {
            self.thread_queue.push(thread.id.clone());
            lock.insert(thread.id.clone(), cpuidx);
            self.storage
                .thread_info
                .insert(thread.id.id, Arc::new(Mutex::new(info)));
            self.storage.threads.insert(thread.id.id, thread);
        }
    }

    fn process_cmds(&mut self) {
        while let Some(cmd) = self.cmd_queue.pop() {
            match cmd {
                SchedCmd::Wake(thread_id, prio) => {
                    if thread_id.kernel {
                        if let Some(thread) = self.storage.kthreads.get_mut(&thread_id.id)
                            && thread.state != ThreadState::Ready
                            && !matches!(thread.state, ThreadState::Exited(_))
                        {
                            thread.state = ThreadState::Ready;
                            if prio {
                                self.thread_priority_queue.push(thread_id);
                            } else {
                                self.thread_queue.push(thread_id);
                            }
                        }
                    } else if let Some(thread) = self.storage.threads.get_mut(&thread_id.id)
                        && thread.state != ThreadState::Ready
                        && !matches!(thread.state, ThreadState::Exited(_))
                    {
                        thread.state = ThreadState::Ready;
                        if prio {
                            self.thread_priority_queue.push(thread_id);
                        } else {
                            self.thread_queue.push(thread_id);
                        }
                    }
                }
                SchedCmd::WaitTimeout(thread_id, instant, duration) => {
                    if thread_id.kernel {
                        if let Some(thread) = self.storage.kthreads.get_mut(&thread_id.id)
                            && !matches!(thread.state, ThreadState::Exited(_))
                        {
                            thread.state = ThreadState::WaitTimeout((instant, duration));
                        }
                    } else if let Some(thread) = self.storage.threads.get_mut(&thread_id.id)
                        && !matches!(thread.state, ThreadState::Exited(_))
                    {
                        thread.state = ThreadState::WaitTimeout((instant, duration));
                    }
                }
                SchedCmd::Wait(thread_id) => {
                    if thread_id.kernel {
                        if let Some(thread) = self.storage.kthreads.get_mut(&thread_id.id)
                            && !matches!(thread.state, ThreadState::Exited(_))
                        {
                            thread.state = ThreadState::Waiting;
                        }
                    } else if let Some(thread) = self.storage.threads.get_mut(&thread_id.id)
                        && !matches!(thread.state, ThreadState::Exited(_))
                    {
                        thread.state = ThreadState::Waiting;
                    }
                }
                SchedCmd::Exit(thread_id, code) => {
                    if thread_id.kernel {
                        if let Some(thread) = self.storage.kthreads.get_mut(&thread_id.id) {
                            thread.state = ThreadState::Exited(code);
                            thread.free();
                        }
                    } else if let Some(thread) = self.storage.threads.get_mut(&thread_id.id) {
                        thread.state = ThreadState::Exited(code);
                        let info = self.storage.thread_info.remove(&thread.id.id);
                        thread.free(info.unwrap());
                    }
                    ALIVE_THREADS.write().remove(&thread_id);
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
            if id.kernel {
                if let Some(thread) = self.storage.kthreads.get_mut(&id.id) {
                    match thread.state {
                        ThreadState::Ready => {}
                        ThreadState::Waiting => continue,
                        ThreadState::WaitTimeout((start, timeout)) => {
                            if now.duration_since(start) >= timeout {
                                thread.state = ThreadState::Ready;
                            } else {
                                self.thread_queue.push(id);
                                continue;
                            }
                        }
                        ThreadState::Exited(code) => {
                            continue;
                        }
                    }
                    self.current_tid = Some(id);
                    self.current_logger = Some(thread.logger.clone());
                    return true;
                }
            } else if let Some(thread) = self.storage.threads.get_mut(&id.id) {
                match thread.state {
                    ThreadState::Ready => {}
                    ThreadState::Waiting => continue,
                    ThreadState::WaitTimeout((start, timeout)) => {
                        if now.duration_since(start) >= timeout {
                            thread.state = ThreadState::Ready;
                        } else {
                            self.thread_queue.push(id);
                            continue;
                        }
                    }
                    ThreadState::Exited(code) => {
                        continue;
                    }
                }
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
            if id.kernel {
                if let Some(thread) = self.storage.kthreads.get_mut(&id.id) {
                    match thread.state {
                        ThreadState::Ready => {}
                        ThreadState::Waiting => continue,
                        ThreadState::WaitTimeout((start, timeout)) => {
                            if now.duration_since(start) >= timeout {
                                thread.state = ThreadState::Ready;
                            } else {
                                self.thread_queue.push(id);
                                continue;
                            }
                        }
                        ThreadState::Exited(code) => {
                            continue;
                        }
                    }
                    self.current_tid = Some(id);
                    self.current_logger = Some(thread.logger.clone());
                    return true;
                }
            } else if let Some(thread) = self.storage.threads.get_mut(&id.id) {
                match thread.state {
                    ThreadState::Ready => {}
                    ThreadState::Waiting => continue,
                    ThreadState::WaitTimeout((start, timeout)) => {
                        if now.duration_since(start) >= timeout {
                            thread.state = ThreadState::Ready;
                        } else {
                            self.thread_queue.push(id);
                            continue;
                        }
                    }
                    ThreadState::Exited(code) => {
                        continue;
                    }
                }
                self.current_tid = Some(id);
                self.current_logger = Some(thread.logger.clone());
                return true;
            }
        }
        self.current_tid = None;
        self.current_logger = None;
        false
    }

    /// Current thread id.
    pub fn current_id(&self) -> ThreadId {
        self.current_tid.clone().unwrap()
    }

    pub fn current_thread_info(&self) -> Arc<Mutex<UserThreadInfo>> {
        let id = self.current_id();
        self.storage.thread_info.get(&id.id).unwrap().clone()
    }

    pub fn current_id_opt(&self) -> Option<ThreadId> {
        self.current_tid.clone()
    }

    /// Gets the current thread logger. Locks momentarely to acquire the logger.
    pub fn get_logger(&self) -> Arc<ThreadLogger> {
        self.current_logger.clone().unwrap()
    }

    // TODO: this checks only current cpu scheduler, so it returns true
    pub fn thread_exists(&self, id: ThreadId) -> bool {
        ALIVE_THREADS.read().get(&id).is_some()
    }

    /// Wake the given thread
    pub fn thread_wake(&self, id: ThreadId, priority: bool) {
        self.cmd_queue.push(SchedCmd::Wake(id, priority));
    }

    /// Sets the given thread as waiting for a maximum of the given timeout.
    pub fn thread_set_wait_timeout(&self, id: ThreadId, timeout: Duration) {
        let now = Instant::now();
        self.cmd_queue.push(SchedCmd::WaitTimeout(id, now, timeout));
    }

    // Does not halt.
    pub fn thread_exit(&self, code: i32) {
        let id = self.current_id();
        self.cmd_queue.push(SchedCmd::Exit(id, code));
        without_interrupts(|| {
            let mut lapic = get_lapic();
            unsafe {
                lapic.send_ipi_self(InterruptIndex::Timer as u8);
            }
        });
    }

    /// The caller thread gets put in a wait (and yields execution) until timeout or another thread wakes it.
    pub fn thread_wait_timeout(&self, timeout: Duration) {
        let id = self.current_id();
        self.thread_set_wait_timeout(id, timeout);
        without_interrupts(|| {
            let mut lapic = get_lapic();
            unsafe {
                lapic.send_ipi_self(InterruptIndex::Timer as u8);
            }
        });
        hlt();
    }

    pub fn thread_park(&self) {
        let id = self.current_id();
        self.cmd_queue.push(SchedCmd::Wait(id));
        without_interrupts(|| {
            let mut lapic = get_lapic();
            unsafe {
                lapic.send_ipi_self(InterruptIndex::Timer as u8);
            }
        });
        hlt();
    }

    /// The caller thread sleeps for the given duration.
    #[inline]
    pub fn thread_sleep(&self, duration: Duration) {
        self.thread_wait_timeout(duration);
    }

    /// Cooperatively yield.
    pub fn thread_yield(&self) {
        without_interrupts(|| {
            let mut lapic = get_lapic();
            unsafe {
                lapic.send_ipi_self(InterruptIndex::Timer as u8);
            }
        });

        hlt();
    }
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
