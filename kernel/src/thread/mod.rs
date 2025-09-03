use core::{sync::atomic::AtomicU64, time::Duration};

use alloc::{string::String, sync::Arc};

use crate::{
    thread::{
        context::CpuContext,
        util::{kthread_stack_alloc, kthread_stack_free, thread_stack_alloc, thread_stack_free},
    },
    timer::Instant,
};

pub mod broadcast;
pub mod context;
pub mod interrupt;
pub mod mailbox;
pub mod paging;
pub mod scheduler;
pub mod signal;
pub mod util;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ThreadId {
    pub id: u64,
    pub name: Option<Arc<String>>,
    pub kernel: bool,
}

impl ThreadId {
    pub fn new(id: u64, kernel: bool) -> Self {
        Self {
            id,
            kernel,
            name: None,
        }
    }

    pub fn new_named(id: u64, kernel: bool, name: String) -> Self {
        Self {
            id,
            kernel,
            name: Some(name.into()),
        }
    }
}

#[derive(Debug)]
pub struct KernelThread {
    pub id: ThreadId,
    /// Saved to free it in case the thread exits.
    pub initial_stack_top: u64,
    pub context: CpuContext,
    pub state: ThreadState,
    // no need for another page table for kthreads.
    // no need for tss kernel stack
}

#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub enum ThreadState {
    Ready,
    Waiting,
    WaitTimeout((Instant, Duration)),
    Exited(i32),
}

impl KernelThread {
    pub fn new(name: Option<String>, entry_point: fn() -> !) -> Self {
        let stack_top = kthread_stack_alloc();
        let context = CpuContext::new_kernel_thread(entry_point as *const u8 as u64, stack_top);

        static KTHREAD_NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let id = KTHREAD_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        KernelThread {
            id: ThreadId {
                id,
                kernel: true,
                name: name.map(Arc::new),
            },
            initial_stack_top: stack_top,
            context,
            state: ThreadState::Ready,
        }
    }

    pub fn free(&self) {
        kthread_stack_free(self.initial_stack_top);
    }
}

#[derive(Debug, Clone)]
pub struct UserThread {
    pub id: ThreadId,
    /// Saved to free it in case the thread exits.
    pub initial_stack_top: u64,
    pub context: CpuContext,
    pub state: ThreadState,
    /// Physical addr
    pub cr3: u64,
    pub initial_kernel_stack_top: u64,
    pub kernel_stack_top: u64,
}

impl UserThread {
    /// Must provide entry point and cr3 page table.
    ///
    /// TODO: also handle arguments.
    pub fn new(entry_point: fn() -> !, cr3: u64) -> Self {
        let kernel_stack_top = kthread_stack_alloc();
        let stack_top = thread_stack_alloc();
        let context = CpuContext::new_user_thread(entry_point as *const u8 as u64, stack_top);

        static THREAD_NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let id = THREAD_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        UserThread {
            id: ThreadId::new(id, false),
            initial_stack_top: stack_top,
            context,
            state: ThreadState::Ready,
            kernel_stack_top,
            initial_kernel_stack_top: kernel_stack_top,
            cr3,
        }
    }

    pub fn free(&self) {
        // todo: memory cleanup
        // todo: cleanup page table and switch to kernel
        thread_stack_free(self.initial_stack_top);
        kthread_stack_free(self.initial_kernel_stack_top);
    }
}
