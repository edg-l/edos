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
    thread::{KernelThread, ThreadState, UserThread, context::CpuContext},
    util::per_cpu::get_percpu_data,
};

#[derive(Debug, Default)]
pub struct Scheduler {
    kthread_queue: SegQueue<u64>,
    thread_queue: SegQueue<u64>,
    pub kthread_spawn_queue: SegQueue<KernelThread>,
    kthreads: BTreeMap<u64, KernelThread>,
    pub thread_spawn_queue: SegQueue<UserThread>,
    threads: BTreeMap<u64, UserThread>,
    current_thread_id: Option<(u64, bool)>,
    /// Physical addr
    pub kernel_cr3: u64,
    pub kernel_cr3_flags: u64,
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

        let mut current_is_kernel = false;

        if let Some((current_id, is_kernel)) = sched.current_thread_id {
            current_is_kernel = is_kernel;
            if is_kernel {
                // coming from kernel task
                if let Some(kthread) = sched.kthreads.get_mut(&current_id) {
                    kthread.context = (*context).clone();
                    serial_println!("Context switch from kernel");

                    match kthread.state {
                        ThreadState::Ready => sched.kthread_queue.push(current_id),
                        ThreadState::Waiting => {}
                        ThreadState::Exited(code) => {
                            serial_println!("KThread {current_id} exited {code}");
                            kthread.free();
                            // TODO: signal and remove
                        }
                    }
                }
            } else {
                // coming from user
                if let Some(thread) = sched.threads.get_mut(&current_id) {
                    thread.context = (*context).clone();
                    serial_println!("Context switch from user");

                    match thread.state {
                        ThreadState::Ready => sched.thread_queue.push(current_id),
                        ThreadState::Waiting => {}
                        ThreadState::Exited(code) => {
                            serial_println!("Thread {current_id} exited {code}");
                            thread.free();
                            // TODO: signal and remove
                        }
                    }
                }
            }
        }

        sched.schedule_next(current_is_kernel);

        if let Some((id, is_kernel)) = sched.current_thread_id {
            serial_println!("Next id (kernel = {is_kernel}): {:#?}", id);

            if is_kernel {
                if let Some(kthread) = sched.kthreads.get(&id) {
                    // going to kernel space.
                    *context = kthread.context.clone();
                    return context;
                }
            } else if let Some(thread) = sched.threads.get(&id) {
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
            self.kthread_queue.push(kthread.id);
            self.kthreads.insert(kthread.id, kthread);
        }
    }

    pub fn schedule_next(&mut self, current_is_kernel: bool) -> bool {
        if current_is_kernel {
            self.schedule_next_thread() || self.schedule_next_kthread()
        } else {
            self.schedule_next_kthread() || self.schedule_next_thread()
        }
    }

    fn schedule_next_kthread(&mut self) -> bool {
        if let Some(id) = self.kthread_queue.pop() {
            self.current_thread_id = Some((id, true));
            true
        } else {
            self.current_thread_id = None;
            false
        }
    }

    fn schedule_next_thread(&mut self) -> bool {
        if let Some(id) = self.thread_queue.pop() {
            self.current_thread_id = Some((id, false));
            true
        } else {
            self.current_thread_id = None;
            false
        }
    }
}
