use core::{alloc::Layout, time::Duration};

use alloc::{collections::btree_map::BTreeMap, vec::Vec};
use crossbeam_queue::SegQueue;
use x86_64::{
    VirtAddr,
    instructions::{hlt, interrupts::without_interrupts},
    registers::control::Cr3,
};

use crate::{
    apic::get_lapic,
    boot::boot_info,
    drivers::fpu::{self, init_fpu_state, restore_fpu_state, save_fpu_state},
    interrupts::InterruptIndex,
    println,
    syscalls::set_gs_kernel_stack,
    thread::{
        KernelThread, ThreadId, ThreadState, context::CpuContext, user::UserThread,
        util::queue_spawn_kthread_named,
    },
    timer::Instant,
    util::per_cpu::get_percpu_data,
};

#[derive(Debug, Default)]
pub struct Scheduler {
    thread_queue: SegQueue<ThreadId>,
    pub kthread_spawn_queue: SegQueue<KernelThread>,
    kthreads: BTreeMap<u64, KernelThread>,
    pub thread_spawn_queue: SegQueue<UserThread>,
    pub threads: BTreeMap<u64, UserThread>,
    current_thread_id: Option<ThreadId>,
    /// Physical addr
    pub kernel_cr3: u64,
    pub kernel_cr3_flags: u64,
}

pub fn init() {
    let ptr = unsafe { alloc::alloc::alloc(Layout::new::<Scheduler>()).cast::<Scheduler>() };
    unsafe { ptr.write(Scheduler::default()) };
    let cr3 = Cr3::read();
    unsafe {
        (*ptr).kernel_cr3 = cr3.0.start_address().as_u64();
        (*ptr).kernel_cr3_flags = cr3.1.bits();
    }
    get_percpu_data().scheduler = ptr;

    queue_spawn_kthread_named("tcleaner", thread_cleaner);
}

pub fn thread_cleaner() -> ! {
    let mut to_remove = Vec::with_capacity(4);
    loop {
        without_interrupts(|| {
            let sched = sched();

            let kernelcr3 = boot_info().cr3;

            unsafe { Cr3::write(kernelcr3.0, kernelcr3.1) };

            for thread in sched.threads.values_mut() {
                if let ThreadState::Exited(code) = thread.state {
                    println!("Thread {} exited {code}", thread.id);
                    //thread.free();
                    to_remove.push(thread.id.clone());
                }
            }

            for t in &to_remove {
                let t = sched.threads.remove(&t.id); // causes problems, removing this line doesnt page fault
                if let Some(mut t) = t {
                    t.free();
                }
            }

            to_remove.clear();

            for thread in sched.kthreads.values_mut() {
                if let ThreadState::Exited(code) = thread.state {
                    println!("KThread {} exited {code}", thread.id);
                    thread.free();
                    to_remove.push(thread.id.clone());
                }
            }

            for t in &to_remove {
                sched.kthreads.remove(&t.id);
            }

            to_remove.clear();
        });
        sched().thread_yield();
    }
}

pub fn sched() -> &'static mut Scheduler {
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
        let sched = cpu.scheduler.as_mut().unwrap_unchecked();
        sched.process_spawn_queue();

        // Push current thread to queue, it's state will be processed then.
        if let Some(current_id) = sched.current_thread_id.clone() {
            sched.current_thread_id = None;
            if current_id.kernel {
                // coming from kernel task
                if let Some(kthread) = sched.kthreads.get_mut(&current_id.id) {
                    kthread.context = (*context).clone();
                    sched.thread_queue.push(current_id);
                }
            } else {
                // coming from user
                if let Some(thread) = sched.threads.get_mut(&current_id.id) {
                    thread.context = (*context).clone();

                    if !thread.fpu_init {
                        init_fpu_state(&mut thread.fpu);
                        thread.fpu_init = true;
                    } else {
                        save_fpu_state(&mut thread.fpu);
                    }

                    println!("Context switch from user");
                    sched.thread_queue.push(current_id);
                }
            }
        }

        sched.schedule_next();

        if let Some(current_id) = sched.current_thread_id.clone() {
            // serial_println!("Next id {:?}", current_id);

            if current_id.kernel {
                if let Some(kthread) = sched.kthreads.get(&current_id.id) {
                    // going to kernel space.
                    *context = kthread.context.clone();
                    return context;
                }
            } else if let Some(thread) = sched.threads.get_mut(&current_id.id) {
                // Going to user space.
                // Set page table
                if Cr3::read().0.start_address() != thread.cr3.0.start_address() {
                    Cr3::write(thread.cr3.0, thread.cr3.1);
                }

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

        loop {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}

#[expect(unused)]
impl Scheduler {
    pub fn process_spawn_queue(&mut self) {
        while let Some(kthread) = self.kthread_spawn_queue.pop() {
            self.thread_queue.push(kthread.id.clone());
            self.kthreads.insert(kthread.id.id, kthread);
        }

        while let Some(thread) = self.thread_spawn_queue.pop() {
            self.thread_queue.push(thread.id.clone());
            self.threads.insert(thread.id.id, thread);
        }
    }

    /// Schedules the next thread id, updating current_thread_id.
    fn schedule_next(&mut self) -> bool {
        let now = Instant::now();
        self.current_thread_id = None;
        while let Some(id) = self.thread_queue.pop() {
            if id.kernel {
                if let Some(thread) = self.kthreads.get_mut(&id.id) {
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
                    self.current_thread_id = Some(id);
                    return true;
                }
            } else if let Some(thread) = self.threads.get_mut(&id.id) {
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
                self.current_thread_id = Some(id);
                return true;
            }
        }
        self.current_thread_id = None;
        false
    }

    /// Current thread id.
    pub fn current_id(&self) -> ThreadId {
        without_interrupts(|| self.current_thread_id.clone().expect("should have a id"))
    }

    /// Mostly called from syscalls
    pub fn current_thread(&self) -> &UserThread {
        let id = self.current_id();
        self.threads.get(&id.id).unwrap()
    }

    /// Mostly called from syscalls
    pub fn current_thread_mut(&mut self) -> &mut UserThread {
        let id = self.current_id();
        self.threads.get_mut(&id.id).unwrap()
    }

    pub fn current_id_opt(&self) -> Option<ThreadId> {
        without_interrupts(|| self.current_thread_id.clone())
    }

    /// Wake the given thread
    pub fn thread_wake(&mut self, id: ThreadId) {
        without_interrupts(|| {
            if id.kernel {
                if let Some(thread) = self.kthreads.get_mut(&id.id)
                    && thread.state != ThreadState::Ready
                {
                    thread.state = ThreadState::Ready;
                    self.thread_queue.push(id);
                }
            } else if let Some(thread) = self.threads.get_mut(&id.id)
                && thread.state != ThreadState::Ready
            {
                thread.state = ThreadState::Ready;
                self.thread_queue.push(id);
            }
        })
    }

    /// Sets the given thread as waiting for a maximum of the given timeout.
    pub fn thread_set_wait_timeout(&mut self, id: ThreadId, timeout: Duration) {
        without_interrupts(|| {
            let now = Instant::now();
            if id.kernel {
                if let Some(thread) = self.kthreads.get_mut(&id.id) {
                    thread.state = ThreadState::WaitTimeout((now, timeout))
                }
            } else if let Some(thread) = self.threads.get_mut(&id.id) {
                thread.state = ThreadState::WaitTimeout((now, timeout))
            }
        })
    }

    // Does not halt.
    pub fn thread_exit(&mut self, code: i32) {
        without_interrupts(|| {
            let id = self.current_id();
            println!("called exit with {code}");
            if id.kernel {
                if let Some(thread) = self.kthreads.get_mut(&id.id) {
                    thread.state = ThreadState::Exited(code)
                }
            } else if let Some(thread) = self.threads.get_mut(&id.id) {
                thread.state = ThreadState::Exited(code)
            }
            let mut lapic = get_lapic();
            unsafe {
                lapic.send_ipi_self(InterruptIndex::Timer as u8);
            }
        });
    }

    /// The caller thread gets put in a wait (and yields execution) until timeout or another thread wakes it.
    pub fn thread_wait_timeout(&mut self, timeout: Duration) {
        without_interrupts(|| {
            let now = Instant::now();
            let id = self.current_id();
            if id.kernel {
                if let Some(thread) = self.kthreads.get_mut(&id.id) {
                    thread.state = ThreadState::WaitTimeout((now, timeout))
                }
            } else if let Some(thread) = self.threads.get_mut(&id.id) {
                thread.state = ThreadState::WaitTimeout((now, timeout))
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
    pub fn thread_sleep(&mut self, duration: Duration) {
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
