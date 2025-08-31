use core::alloc::Layout;

use alloc::collections::btree_map::BTreeMap;
use crossbeam_queue::SegQueue;
use x86_64::{
    PhysAddr, VirtAddr,
    registers::control::{Cr3, Cr3Flags},
    structures::paging::PhysFrame,
};

use crate::{
    apic::get_lapic,
    serial_println,
    thread::{
        KernelThread, ThreadId, ThreadState, UserThread, context::CpuContext, signal::Signal,
    },
    util::per_cpu::get_percpu_data,
};

#[derive(Debug, Default)]
pub struct Scheduler {
    thread_queue: SegQueue<ThreadId>,
    pub kthread_spawn_queue: SegQueue<KernelThread>,
    kthreads: BTreeMap<u64, KernelThread>,
    pub thread_spawn_queue: SegQueue<UserThread>,
    threads: BTreeMap<u64, UserThread>,
    current_thread_id: Option<ThreadId>,
    /// Physical addr
    pub kernel_cr3: u64,
    pub kernel_cr3_flags: u64,
    pub signal_queue: SegQueue<(ThreadId, Signal)>,
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
        sched.process_signal_queue();

        if let Some(current_id) = sched.current_thread_id {
            if current_id.kernel {
                // coming from kernel task
                if let Some(kthread) = sched.kthreads.get_mut(&current_id.id) {
                    kthread.context = (*context).clone();
                    serial_println!("Context switch from kernel");

                    match kthread.state {
                        ThreadState::Ready => sched.thread_queue.push(current_id),
                        ThreadState::Waiting => {}
                        ThreadState::Exited(code) => {
                            serial_println!("KThread {current_id:?} exited {code}");
                            kthread.free();
                            // TODO: signal and remove
                        }
                    }
                }
            } else {
                // coming from user
                if let Some(thread) = sched.threads.get_mut(&current_id.id) {
                    thread.context = (*context).clone();
                    serial_println!("Context switch from user");

                    match thread.state {
                        ThreadState::Ready => sched.thread_queue.push(current_id),
                        ThreadState::Waiting => {}
                        ThreadState::Exited(code) => {
                            serial_println!("Thread {current_id:?} exited {code}");
                            thread.free();
                            // TODO: signal and remove
                        }
                    }
                }
            }
        }

        sched.schedule_next();

        if let Some(current_id) = sched.current_thread_id {
            serial_println!("Next id {:#?}", current_id);

            if current_id.kernel {
                if let Some(kthread) = sched.kthreads.get(&current_id.id) {
                    // going to kernel space.
                    *context = kthread.context.clone();
                    return context;
                }
            } else if let Some(thread) = sched.threads.get(&current_id.id) {
                // Going to user space.
                *context = thread.context.clone();
                // Set RSP0
                cpu.tss.privilege_stack_table[0] = VirtAddr::new(thread.kernel_stack_top);
                // Set page table
                Cr3::write(
                    PhysFrame::containing_address(PhysAddr::new(thread.cr3)),
                    Cr3Flags::from_bits_truncate(sched.kernel_cr3_flags),
                );

                return context;
            }
        }

        loop {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}

impl Scheduler {
    pub fn process_spawn_queue(&mut self) {
        while let Some(kthread) = self.kthread_spawn_queue.pop() {
            self.thread_queue.push(kthread.id);
            self.kthreads.insert(kthread.id.id, kthread);
        }

        while let Some(thread) = self.thread_spawn_queue.pop() {
            self.thread_queue.push(thread.id);
            self.threads.insert(thread.id.id, thread);
        }
    }

    /// Processes the signal queue
    pub fn process_signal_queue(&mut self) {
        while let Some((id, signal)) = self.signal_queue.pop() {
            if id.kernel {
                if let Some(kthread) = self.kthreads.get_mut(&id.id) {
                    match signal {
                        Signal::Exit(code) => kthread.state = ThreadState::Exited(code),
                    }
                }
            } else if let Some(thread) = self.threads.get_mut(&id.id) {
                match signal {
                    Signal::Exit(code) => thread.state = ThreadState::Exited(code),
                }
            }
        }
    }

    fn schedule_next(&mut self) -> bool {
        while let Some(id) = self.thread_queue.pop() {
            if id.kernel {
                if let Some(thread) = self.kthreads.get(&id.id)
                    && thread.state == ThreadState::Ready
                {
                    self.current_thread_id = Some(id);
                    return true;
                }
            } else if let Some(thread) = self.threads.get(&id.id)
                && thread.state == ThreadState::Ready
            {
                self.current_thread_id = Some(id);
                return true;
            }
        }
        self.current_thread_id = None;
        false
    }

    pub fn current_id(&self) -> ThreadId {
        self.current_thread_id.unwrap()
    }
}
