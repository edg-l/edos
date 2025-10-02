use alloc::{collections::btree_map::BTreeMap, sync::Arc};
use core::cell::Cell;

use crate::{
    syscalls::Errno,
    thread::{
        mutex::BlockingMutex,
        scheduler::sched,
        waitqueue::{WaitOutcome, WaitQueue},
    },
    util::uaccess::try_read_user,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FutexKey {
    mm_ptr: usize,
    addr: u64,
}

impl FutexKey {
    fn new(mm_ptr: usize, addr: u64) -> Self {
        Self { mm_ptr, addr }
    }
}

static FUTEX_REGISTRY: BlockingMutex<BTreeMap<FutexKey, Arc<WaitQueue>>> =
    BlockingMutex::new(BTreeMap::new());

fn queue_for(key: FutexKey) -> Arc<WaitQueue> {
    let mut map = FUTEX_REGISTRY.lock();
    Arc::clone(map.entry(key).or_insert_with(|| Arc::new(WaitQueue::new())))
}

fn cleanup_if_empty(key: &FutexKey, queue: &Arc<WaitQueue>) {
    if !queue.is_empty() {
        return;
    }

    let mut map = FUTEX_REGISTRY.lock();
    if let Some(existing) = map.get(key) {
        if Arc::ptr_eq(existing, queue) && existing.is_empty() {
            map.remove(key);
        }
    }
}

pub fn sys_futex_wait(addr: *const u32, expected: u32) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if addr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let current = match unsafe { try_read_user(addr) } {
        Some(value) => value,
        None => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    };

    if current != expected {
        return 1;
    }

    let mm_arc = {
        let guard = info.lock();
        guard.memory_manager.clone()
    };
    let key = FutexKey::new(Arc::as_ptr(&mm_arc) as usize, addr as u64);
    let queue = queue_for(key);

    let fault = Cell::new(None);
    let outcome = queue.wait_until(|| match unsafe { try_read_user(addr) } {
        Some(value) => value != expected,
        None => {
            fault.set(Some(Errno::EFAULT));
            true
        }
    });

    if let Some(err) = fault.get() {
        cleanup_if_empty(&key, &queue);
        info.lock().errno = err;
        return !0u64;
    }

    let result = match outcome {
        WaitOutcome::Ready => 1,
        WaitOutcome::Parked => match unsafe { try_read_user(addr) } {
            Some(value) => {
                if value != expected {
                    0
                } else {
                    1
                }
            }
            None => {
                cleanup_if_empty(&key, &queue);
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
        },
    };

    cleanup_if_empty(&key, &queue);

    result
}

pub fn sys_futex_wake(addr: *const u32, count: u32) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if addr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    if count == 0 {
        return 0;
    }

    let mm_arc = {
        let guard = info.lock();
        guard.memory_manager.clone()
    };
    let key = FutexKey::new(Arc::as_ptr(&mm_arc) as usize, addr as u64);
    let queue = {
        let map = FUTEX_REGISTRY.lock();
        match map.get(&key) {
            Some(queue) => Arc::clone(queue),
            None => return 0,
        }
    };

    let mut woken = 0u64;
    for _ in 0..count {
        if queue.wake_one() {
            woken += 1;
        } else {
            break;
        }
    }

    if woken > 0 {
        cleanup_if_empty(&key, &queue);
    }

    woken
}
