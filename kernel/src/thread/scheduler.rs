use core::{alloc::Layout, time::Duration};

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use crossbeam_queue::ArrayQueue;
use heapless::{LinearMap, index_map::FnvIndexMap};
use spin::RwLock;
use x86_64::{
    VirtAddr,
    instructions::{
        hlt,
        interrupts::{disable, enable, without_interrupts},
    },
    registers::control::Cr3,
};

use crate::{
    acpi::current_cpu_index,
    apic::get_lapic,
    boot::boot_info,
    drivers::fpu::{init_fpu_state, restore_fpu_state, save_fpu_state},
    interrupts::InterruptIndex,
    logs::ThreadLogger,
    println,
    syscalls::{Errno, set_gs_kernel_stack},
    thread::{
        KernelThread, ThreadId, ThreadState, context::CpuContext, user::UserThread,
        util::queue_spawn_kthread_named,
    },
    timer::Instant,
    util::per_cpu::get_percpu_data,
};

pub static ALIVE_THREADS: RwLock<FnvIndexMap<ThreadId, u64, 1024>> =
    RwLock::new(FnvIndexMap::new());

/// Returns the scheduler id this thread lives on.
pub fn thread_exists(tid: &ThreadId) -> Option<u64> {
    ALIVE_THREADS.read().get(tid).copied()
}

pub struct Scheduler {
    thread_queue: ArrayQueue<ThreadId>,
    thread_priority_queue: ArrayQueue<ThreadId>,
    pub kthread_spawn_queue: ArrayQueue<KernelThread>,
    pub thread_spawn_queue: ArrayQueue<UserThread>,
    pub storage: RwLock<Storage>,
}

pub struct Storage {
    current_thread_id: Option<ThreadId>,
    pub kthreads: heapless::LinearMap<u64, KernelThread, 64>,
    pub threads: heapless::LinearMap<u64, UserThread, 256>,
}

pub fn init() {
    println!("Initializing scheduler");
    let sched = Box::new(Scheduler {
        thread_queue: ArrayQueue::new(1024),
        thread_priority_queue: ArrayQueue::new(256),
        kthread_spawn_queue: ArrayQueue::new(64),
        thread_spawn_queue: ArrayQueue::new(64),
        storage: RwLock::new(Storage {
            current_thread_id: None,
            kthreads: LinearMap::new(),
            threads: LinearMap::new(),
        }),
    });

    let ptr = Box::leak(sched);
    get_percpu_data().scheduler = ptr;
    println!("Saved scheduler on percpu");

    queue_spawn_kthread_named("tcleaner", thread_cleaner as u64);
}

pub extern "C" fn thread_cleaner() -> ! {
    let mut to_remove = Vec::with_capacity(4);

    loop {
        let sched = sched();

        sched.modify_storage(|storage| {
            for thread in storage.threads.values_mut() {
                if let ThreadState::Exited(_) = thread.state {
                    to_remove.push(thread.id.clone());
                }
            }

            for thread in storage.kthreads.values_mut() {
                if let ThreadState::Exited(_) = thread.state {
                    to_remove.push(thread.id.clone());
                }
            }

            for t in &to_remove {
                if t.kernel {
                    let t = storage.kthreads.remove(&t.id);
                    if let Some(t) = t {
                        t.free();
                    }
                } else {
                    let t = storage.threads.remove(&t.id);
                    if let Some(mut t) = t {
                        t.free();
                    }
                }
            }
        });

        without_interrupts(|| {
            let mut lock = ALIVE_THREADS.write();

            for t in &to_remove {
                lock.remove(t);
            }

            to_remove.clear();
        });

        sched.thread_yield();
    }
}

pub fn get_sched_cpu(cpu: usize) -> &'static Scheduler {
    todo!()
}

pub fn sched() -> &'static Scheduler {
    unsafe { get_percpu_data().scheduler.as_mut().unwrap_unchecked() }
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
        let sched = cpu.scheduler.as_ref().unwrap_unchecked();
        let storage = &mut *sched.storage.write();
        sched.process_spawn_queue(storage);

        // Push current thread to queue, it's state will be processed then.
        if let Some(current_id) = storage.current_thread_id.clone() {
            storage.current_thread_id = None;
            if current_id.kernel {
                // coming from kernel task
                if let Some(kthread) = storage.kthreads.get_mut(&current_id.id) {
                    kthread.context = (*context).clone();
                    sched.thread_queue.push(current_id);
                }
            } else {
                // coming from user
                if let Some(thread) = storage.threads.get_mut(&current_id.id) {
                    thread.context = (*context).clone();

                    if !thread.fpu_init {
                        init_fpu_state(&mut thread.fpu);
                        thread.fpu_init = true;
                    } else {
                        save_fpu_state(&mut thread.fpu);
                    }

                    sched.thread_queue.push(current_id);
                }
            }
        }

        sched.schedule_next(storage);

        if let Some(current_id) = storage.current_thread_id.clone() {
            // serial_println!("Next id {:?}", current_id);

            if current_id.kernel {
                if let Some(kthread) = storage.kthreads.get(&current_id.id) {
                    // going to kernel space.
                    // for now, always switch to kernel page, just in case.
                    switch_to_kernel_page();
                    *context = kthread.context.clone();
                    return context;
                }
            } else if let Some(thread) = storage.threads.get_mut(&current_id.id) {
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

        context
    }
}

#[expect(unused)]
impl Scheduler {
    /// Interrupts are disabled during the function execution.
    pub fn modify_storage<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Storage) -> R,
    {
        without_interrupts(|| {
            let mut storage = self.storage.write();
            f(&mut storage)
        })
    }

    /// Interrupts are disabled during the function execution.
    pub fn read_storage<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Storage) -> R,
    {
        without_interrupts(|| {
            let mut storage = self.storage.read();
            f(&storage)
        })
    }

    pub fn process_spawn_queue(&self, storage: &mut Storage) {
        let mut lock = ALIVE_THREADS.write();
        let cpuidx = current_cpu_index() as u64;
        while let Some(kthread) = self.kthread_spawn_queue.pop() {
            self.thread_queue.push(kthread.id.clone());
            lock.insert(kthread.id.clone(), cpuidx);
            storage.kthreads.insert(kthread.id.id, kthread);
        }

        while let Some(thread) = self.thread_spawn_queue.pop() {
            self.thread_queue.push(thread.id.clone());
            lock.insert(thread.id.clone(), cpuidx);
            storage.threads.insert(thread.id.id, thread);
        }
    }

    /// Schedules the next thread id, updating current_thread_id.
    fn schedule_next(&self, storage: &mut Storage) -> bool {
        let now = Instant::now();
        storage.current_thread_id = None;

        // TODO: dedup this code
        while let Some(id) = self.thread_priority_queue.pop() {
            if id.kernel {
                if let Some(thread) = storage.kthreads.get_mut(&id.id) {
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
                    storage.current_thread_id = Some(id);
                    return true;
                }
            } else if let Some(thread) = storage.threads.get_mut(&id.id) {
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
                storage.current_thread_id = Some(id);
                return true;
            }
        }

        let mut limit = self.thread_queue.len();

        while limit > 0
            && let Some(id) = self.thread_queue.pop()
        {
            limit -= 1;
            if id.kernel {
                if let Some(thread) = storage.kthreads.get_mut(&id.id) {
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
                    storage.current_thread_id = Some(id);
                    return true;
                }
            } else if let Some(thread) = storage.threads.get_mut(&id.id) {
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
                storage.current_thread_id = Some(id);
                return true;
            }
        }
        storage.current_thread_id = None;
        false
    }

    /// Current thread id.
    pub fn current_id(&self) -> ThreadId {
        self.read_storage(|storage| storage.current_thread_id.clone().expect("need id"))
    }

    /// Mostly called from syscalls
    pub fn current_thread<FF, RR>(&self, f: FF) -> RR
    where
        FF: FnOnce(&UserThread) -> RR,
    {
        let id = self.current_id();
        self.read_storage(move |storage: &Storage| -> RR {
            f(storage.threads.get(&id.id).unwrap())
        })
    }

    /// Mostly called from syscalls
    pub fn current_thread_mut<FF, RR>(&self, f: FF) -> RR
    where
        FF: FnOnce(&mut UserThread) -> RR,
    {
        let id = self.current_id();
        self.modify_storage(move |mut storage: &mut Storage| -> RR {
            f(storage.threads.get_mut(&id.id).unwrap())
        })
    }

    // gets a lock
    pub fn current_thread_set_errno(&self, errno: Errno) {
        self.current_thread_mut(|t| {
            t.errno = errno;
        });
    }

    pub fn current_thread_clear_errno(&self) {
        self.current_thread_mut(|t| {
            t.errno = crate::syscalls::Errno::Clear;
        });
    }

    pub fn current_id_opt(&self) -> Option<ThreadId> {
        without_interrupts(|| self.read_storage(|s| s.current_thread_id.clone()))
    }

    /// Gets the current thread logger. Locks momentarely to acquire the logger.
    pub fn get_logger(&self) -> Arc<ThreadLogger> {
        self.read_storage(|storage| match &storage.current_thread_id {
            Some(id) => {
                if id.kernel {
                    storage.kthreads.get(&id.id).unwrap().logger.clone()
                } else {
                    storage.threads.get(&id.id).unwrap().logger.clone()
                }
            }
            None => unreachable!(),
        })
    }

    // TODO: this checks only current cpu scheduler, so it returns true
    pub fn thread_exists(&self, id: ThreadId) -> bool {
        ALIVE_THREADS.read().get(&id).is_some()
    }

    /// Wake the given thread
    pub fn thread_wake(&self, id: ThreadId, priority: bool) {
        self.modify_storage(|storage| {
            if id.kernel {
                if let Some(thread) = storage.kthreads.get_mut(&id.id)
                    && thread.state != ThreadState::Ready
                {
                    thread.state = ThreadState::Ready;
                    if priority {
                        self.thread_priority_queue.push(id);
                    } else {
                        self.thread_queue.push(id);
                    }
                }
            } else if let Some(thread) = storage.threads.get_mut(&id.id)
                && thread.state != ThreadState::Ready
            {
                thread.state = ThreadState::Ready;
                if priority {
                    self.thread_priority_queue.push(id);
                } else {
                    self.thread_queue.push(id);
                }
            }
        })
    }

    /// Sets the given thread as waiting for a maximum of the given timeout.
    pub fn thread_set_wait_timeout(&self, id: ThreadId, timeout: Duration) {
        self.modify_storage(|storage| {
            let now = Instant::now();
            if id.kernel {
                if let Some(thread) = storage.kthreads.get_mut(&id.id) {
                    thread.state = ThreadState::WaitTimeout((now, timeout))
                }
            } else if let Some(thread) = storage.threads.get_mut(&id.id) {
                thread.state = ThreadState::WaitTimeout((now, timeout))
            }
        })
    }

    // Does not halt.
    pub fn thread_exit(&self, code: i32) {
        let id = self.current_id();
        self.modify_storage(|storage| {
            if id.kernel {
                if let Some(thread) = storage.kthreads.get_mut(&id.id) {
                    thread.state = ThreadState::Exited(code)
                }
            } else if let Some(thread) = storage.threads.get_mut(&id.id) {
                thread.state = ThreadState::Exited(code)
            }
            let mut lapic = get_lapic();
            unsafe {
                lapic.send_ipi_self(InterruptIndex::Timer as u8);
            }
        })
    }

    /// The caller thread gets put in a wait (and yields execution) until timeout or another thread wakes it.
    pub fn thread_wait_timeout(&self, timeout: Duration) {
        let id = self.current_id();
        self.modify_storage(|storage| {
            let now = Instant::now();
            if id.kernel {
                if let Some(thread) = storage.kthreads.get_mut(&id.id) {
                    thread.state = ThreadState::WaitTimeout((now, timeout))
                }
            } else if let Some(thread) = storage.threads.get_mut(&id.id) {
                thread.state = ThreadState::WaitTimeout((now, timeout))
            }
            let mut lapic = get_lapic();
            unsafe {
                lapic.send_ipi_self(InterruptIndex::Timer as u8);
            }
        });
        hlt();
    }

    pub fn thread_park(&self) {
        let id = self.current_id();
        self.modify_storage(|storage| {
            let now = Instant::now();
            if id.kernel {
                if let Some(thread) = storage.kthreads.get_mut(&id.id) {
                    thread.state = ThreadState::Waiting
                }
            } else if let Some(thread) = storage.threads.get_mut(&id.id) {
                thread.state = ThreadState::Waiting
            }
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
}
