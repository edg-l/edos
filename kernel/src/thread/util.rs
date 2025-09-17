#![expect(unused)]

use core::sync::atomic::AtomicU64;

use alloc::string::ToString;
use crossbeam_queue::SegQueue;
use x86_64::{VirtAddr, instructions::hlt, structures::paging::PageTableFlags};

use crate::{
    memory::{
        KTHREAD_STACK_REGION_SIZE, KTHREAD_STACK_SIZE, USER_STACK_SIZE, USER_STACK_TOP,
        mapper::{MemoryManager, memory_mapper},
        valloc::vmalloc,
    },
    smp::NUM_CPUS,
    thread::{
        Thread, UserThreadInfo,
        scheduler::{SCHEDULERS, Scheduler, sched},
    },
};

static KTHREAD_FREED_STACKS: SegQueue<u64> = SegQueue::new();

/// Returns the stack top of a kthread or a kernel stack for a user process.
///
/// Note: When used in a iretq, the stack must be 8 byte aligned as to emulate a call.
/// Thus called must decrement the stack top by 8 bytes.
pub fn kthread_stack_alloc() -> u64 {
    if let Some(region_bottom) = KTHREAD_FREED_STACKS.pop() {
        // Reuse existing region: region_bottom points to start of guard page
        // Stack top = region_bottom + stack_size
        return region_bottom + KTHREAD_STACK_SIZE;
    }

    let stack_bottom = vmalloc(KTHREAD_STACK_REGION_SIZE);

    let mut mapper = memory_mapper();

    // Map only the stack portion, leaving guard page at the bottom unmapped
    mapper
        .map_memory(
            stack_bottom,
            KTHREAD_STACK_SIZE,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        )
        .expect("failed to map kstack");

    stack_bottom.as_u64() + KTHREAD_STACK_SIZE // Skip guard page + stack size
}

pub fn kthread_stack_free(stack_top: u64) {
    // Calculate the original region bottom (including guard page)
    let region_bottom = stack_top - KTHREAD_STACK_SIZE;
    KTHREAD_FREED_STACKS.push(region_bottom);
}

pub fn queue_spawn_kthread(entry: u64) -> u64 {
    // TODO: pick a cpu with lowest thread count
    let thread = Thread::new_kernel(None, entry);
    let id = thread.id;
    sched().thread_spawn_queue.push((thread, None));
    id
}

pub fn queue_spawn_kthread_named(name: &str, entry: u64) -> u64 {
    // TODO: pick a cpu with lowest thread count
    let thread = Thread::new_kernel(Some(name.to_string()), entry);

    let id = thread.id;
    let sched = pick_sched();
    sched.thread_spawn_queue.push((thread, None));
    id
}

pub fn queue_spawn_kthread_named_arg(name: &str, entry: u64, arg: *mut u8) -> u64 {
    let mut thread = Thread::new_kernel(Some(name.to_string()), entry);
    thread.context.rdi = arg as u64;

    let id = thread.id;
    let sched = pick_sched();
    sched.thread_spawn_queue.push((thread, None));
    id
}

pub fn pick_sched() -> &'static Scheduler {
    let num_cpus = NUM_CPUS.load(core::sync::atomic::Ordering::Relaxed);

    let schedulers = SCHEDULERS.read();
    let mut id = 0;
    let mut min_count = u64::MAX;
    for i in 0..num_cpus {
        if let Some(sched) = schedulers.get(&(i as u32)).cloned() {
            let count = sched
                .thread_count
                .load(core::sync::atomic::Ordering::Acquire);

            if count < min_count {
                min_count = count;
                id = i;
            }
        }
    }

    (*SCHEDULERS
        .read()
        .get(&(id as u32))
        .expect("failed to find scheduler")) as _
}

/// Exits a kthread.
pub fn kthread_exit(code: i32) -> ! {
    let sched = sched();
    if let Some(tid) = sched.current_id_opt() {
        sched.thread_exit(code);
    }

    loop {
        hlt();
    }
}

/// Returns the stack top of a kthread or a kernel stack for a user process.
///
/// Note: When used in a iretq, the stack must be 8 byte aligned as to emulate a call.
/// Thus called must decrement the stack top by 8 bytes.
pub fn thread_stack_alloc(manager: &mut MemoryManager) -> u64 {
    let stack_bottom = USER_STACK_TOP - USER_STACK_SIZE;

    manager
        .map_memory(
            stack_bottom,
            USER_STACK_SIZE,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_EXECUTE
                | PageTableFlags::USER_ACCESSIBLE,
        )
        .expect("failed to map kstack");

    USER_STACK_TOP.as_u64()
}

pub fn thread_stack_free(manager: &mut MemoryManager, stack_top: u64) {
    let stack_bottom = VirtAddr::new(stack_top - USER_STACK_SIZE);
    manager.unmap_memory(stack_bottom, USER_STACK_SIZE).ok();
}

pub fn queue_spawn_thread(thread: Thread, info: UserThreadInfo) {
    let sched = pick_sched();
    sched.thread_spawn_queue.push((thread, Some(info)));
}
