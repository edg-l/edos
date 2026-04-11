#![expect(unused)]

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::{string::ToString, sync::Arc};
use crossbeam_queue::ArrayQueue;
use spin::Once;
use x86_64::{VirtAddr, instructions::hlt, structures::paging::PageTableFlags};

use crate::{
    memory::{
        KTHREAD_STACK_REGION_SIZE, KTHREAD_STACK_SIZE, USER_STACK_SIZE, USER_STACK_TOP,
        mapper::{MemoryManager, memory_mapper},
        valloc::vmalloc,
    },
    print, println,
    smp::NUM_CPUS,
    thread::{
        UserThreadInfo,
        scheduler::{SCHEDULERS, Scheduler, exit_thread, sched},
        thread::{Thread, ThreadId},
    },
};

static KTHREAD_FREED_STACKS: Once<ArrayQueue<u64>> = Once::new();

fn freed_stacks() -> &'static ArrayQueue<u64> {
    KTHREAD_FREED_STACKS.call_once(|| ArrayQueue::new(256))
}

/// Returns the stack top of a kthread or a kernel stack for a user process.
///
/// Note: When used in a iretq, the stack must be 8 byte aligned as to emulate a call.
/// Thus called must decrement the stack top by 8 bytes.
pub fn kthread_stack_alloc() -> u64 {
    if let Some(region_bottom) = freed_stacks().pop() {
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
    if freed_stacks().push(region_bottom).is_err() {
        // Queue full -- stack region leaked. Not critical since the virtual
        // memory is still mapped and can't be reused anyway.
        println!("WARNING: kthread freed stack queue full, leaking stack region");
    }
}

pub fn queue_spawn_kthread(entry: u64) -> ThreadId {
    let thread = Thread::new_kernel(None, entry, 0);
    let id = thread.id;
    let sched = pick_sched();
    sched.spawn_thread(thread);
    id
}

pub fn queue_spawn_kthread_named(name: &str, entry: u64) -> ThreadId {
    let thread = Thread::new_kernel(Some(name.to_string()), entry, 0);

    let id = thread.id;
    let sched = pick_sched();
    sched.spawn_thread(thread);
    id
}

pub fn queue_spawn_kthread_named_arg(name: &str, entry: u64, arg: *mut u8) -> ThreadId {
    let mut thread = Thread::new_kernel(Some(name.to_string()), entry, arg as u64);

    let id = thread.id;
    let sched = pick_sched();
    sched.spawn_thread(thread);
    id
}

pub fn pick_sched() -> &'static Scheduler {
    let num_cpus = NUM_CPUS.load(Ordering::Relaxed);

    let schedulers = SCHEDULERS.read();
    let mut best_id = 0;
    let mut min_count = u64::MAX;
    for i in 0..num_cpus {
        if let Some(sched) = schedulers.get(&(i as u32)).cloned() {
            let count = sched.thread_count.load(Ordering::Acquire);
            if count < min_count {
                min_count = count;
                best_id = i;
            }
        }
    }

    schedulers
        .get(&(best_id as u32))
        .expect("failed to find scheduler")
}

/// Exits a kthread.
pub fn kthread_exit(code: i32) -> ! {
    sched().thread_exit(code)
}

pub fn thread_stack_alloc(_manager: &mut MemoryManager) -> u64 {
    USER_STACK_TOP.as_u64()
}

/// Free a lazily-allocated user stack. Only unmaps pages that were actually
/// faulted in (present in the page table).
pub fn thread_stack_free(manager: &mut MemoryManager, stack_top: u64) {
    use x86_64::structures::paging::{Mapper, Page, Size4KiB, Translate};

    let stack_bottom = stack_top - USER_STACK_SIZE;
    let page_count = USER_STACK_SIZE / 4096;
    for i in 0..page_count {
        let virt = VirtAddr::new(stack_bottom + i * 4096);
        let page: Page<Size4KiB> = Page::containing_address(virt);
        if let Ok(phys) = manager.mapper.translate_page(page) {
            if let Ok((_, flush)) = manager.mapper.unmap(page) {
                flush.ignore();
            }
            unsafe { crate::memory::frame_allocator::frame_allocator().deallocate_frame(phys) };
        }
    }
}

pub fn queue_spawn_thread(thread: Arc<Thread>) -> ThreadId {
    let id = thread.id;
    let sched = pick_sched();
    sched.spawn_thread(thread);
    id
}
