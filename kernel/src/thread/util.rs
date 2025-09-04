use core::sync::atomic::AtomicU64;

use alloc::string::ToString;
use crossbeam_queue::SegQueue;
use x86_64::{VirtAddr, instructions::hlt, structures::paging::PageTableFlags};

use crate::{
    memory::{
        KTHREAD_STACK_FIRST, KTHREAD_STACK_SIZE, USER_STACK_FIRST, USER_STACK_SIZE,
        mapper::memory_mapper,
    },
    thread::{
        KernelThread,
        scheduler::sched,
        signal::{Signal, send_signal},
    },
};

static KTHREAD_FREED_STACKS: SegQueue<u64> = SegQueue::new();
static KTHREAD_NEXT_STACK: AtomicU64 = AtomicU64::new(KTHREAD_STACK_FIRST.as_u64());

/// Returns the stack top of a kthread or a kernel stack for a user process.
///
/// Note: When used in a iretq, the stack must be 8 byte aligned as to emulate a call.
/// Thus called must decrement the stack top by 8 bytes.
pub fn kthread_stack_alloc() -> u64 {
    if let Some(freed) = KTHREAD_FREED_STACKS.pop() {
        return freed;
    }

    let stack_bottom =
        KTHREAD_NEXT_STACK.fetch_add(KTHREAD_STACK_SIZE, core::sync::atomic::Ordering::Relaxed);

    let mut mapper = memory_mapper();

    mapper
        .map_memory(
            VirtAddr::new(stack_bottom),
            KTHREAD_STACK_SIZE,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        )
        .expect("failed to map kstack");

    stack_bottom + KTHREAD_STACK_SIZE
}

pub fn kthread_stack_free(stack_top: u64) {
    let stack_bottom = stack_top - KTHREAD_STACK_SIZE;
    KTHREAD_FREED_STACKS.push(stack_bottom);
}

pub fn queue_spawn_kthread(entry: fn() -> !) {
    let thread = KernelThread::new(None, entry);

    sched().kthread_spawn_queue.push(thread);
}

pub fn queue_spawn_kthread_named(name: &str, entry: fn() -> !) {
    let thread = KernelThread::new(Some(name.to_string()), entry);

    sched().kthread_spawn_queue.push(thread);
}

/// Exits a kthread.
pub fn kthread_exit(code: i32) -> ! {
    send_signal(Signal::Exit(code));

    loop {
        hlt();
    }
}

static THREAD_FREED_STACKS: SegQueue<u64> = SegQueue::new();
static THREAD_NEXT_STACK: AtomicU64 = AtomicU64::new(USER_STACK_FIRST.as_u64());

/// Returns the stack top of a kthread or a kernel stack for a user process.
///
/// Note: When used in a iretq, the stack must be 8 byte aligned as to emulate a call.
/// Thus called must decrement the stack top by 8 bytes.
pub fn thread_stack_alloc() -> u64 {
    if let Some(freed) = THREAD_FREED_STACKS.pop() {
        return freed;
    }

    let stack_bottom = THREAD_NEXT_STACK.fetch_add(
        USER_STACK_FIRST.as_u64(),
        core::sync::atomic::Ordering::Relaxed,
    );

    let mut mapper = memory_mapper();

    mapper
        .map_memory(
            VirtAddr::new(stack_bottom),
            USER_STACK_SIZE,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_EXECUTE
                | PageTableFlags::USER_ACCESSIBLE,
        )
        .expect("failed to map kstack");

    stack_bottom + USER_STACK_SIZE
}

pub fn thread_stack_free(stack_top: u64) {
    let stack_bottom = stack_top - USER_STACK_SIZE;
    THREAD_FREED_STACKS.push(stack_bottom);
}

pub fn queue_spawn_thread(thread: KernelThread) {
    sched().kthread_spawn_queue.push(thread);
}
