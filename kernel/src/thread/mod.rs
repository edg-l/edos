use core::{fmt::Display, sync::atomic::AtomicU64, time::Duration};

use alloc::{string::String, sync::Arc};

use crate::{
    thread::{
        context::CpuContext,
        util::{kthread_stack_alloc, kthread_stack_free},
    },
    timer::Instant,
};

pub mod broadcast;
pub mod context;
pub mod fd;
pub mod interrupt;
pub mod mailbox;
pub mod paging;
pub mod pipe;
pub mod scheduler;
pub mod user;
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

impl Display for ThreadId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.kernel {
            write!(f, "KID({})", self.id)
        } else {
            write!(f, "PID({})", self.id)
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
