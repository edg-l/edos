use alloc::{collections::btree_map::BTreeMap, sync::Arc};
use core::{cell::Cell, time::Duration};

use crate::thread::scheduler::{current_thread, current_thread_info};
use crate::{
    syscalls::Errno,
    thread::{
        mutex::BlockingMutex,
        thread::{ThreadId, get_thread_by_id},
        waitqueue::{WaitOutcome, WaitQueue},
    },
    util::uaccess::{access_ok, try_read_user},
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
    if let Some(existing) = map.get(key)
        && Arc::ptr_eq(existing, queue)
        && existing.is_empty()
    {
        map.remove(key);
    }
}

/// A loan made to the thread a futex waiter named as the lock's owner.
///
/// Ended where it was made rather than by whatever releases the futex: the
/// kernel does not own the word, so there is no release to hook. See the
/// discipline note on [`sys_futex_wait_pi`].
struct FutexLoan(Option<ThreadId>);

impl FutexLoan {
    /// Lend the calling thread's priority to `owner_tid`.
    ///
    /// The loan is the *caller's own* effective priority, so a program naming
    /// a thread that does not hold its lock can raise that thread no higher
    /// than itself and only for as long as it is itself waiting. That bounds
    /// what a wrong or hostile `owner_tid` can do to a self-inflicted one,
    /// which is why the argument needs no ownership proof the kernel has no
    /// way to check.
    fn lend(owner_tid: u64) -> Self {
        let Some(me) = current_thread() else {
            return Self(None);
        };
        if owner_tid == 0 || owner_tid == me.id.0 {
            return Self(None);
        }
        let Some(owner) = get_thread_by_id(ThreadId(owner_tid)) else {
            return Self(None);
        };
        owner.lend_priority(me.effective_priority());
        Self(Some(ThreadId(owner_tid)))
    }

    fn take_back(self) {
        if let Some(tid) = self.0
            && let Some(owner) = get_thread_by_id(tid)
        {
            owner.drop_lent_priority();
        }
    }
}

pub fn sys_futex_wait(addr: *const u32, expected: u32, timeout_ns: u64) -> Result<u64, Errno> {
    futex_wait_inner(addr, expected, timeout_ns, 0)
}

/// `futex_wait`, plus the thread the caller believes holds the lock behind the
/// word.
///
/// A futex word is opaque to the kernel — it is a `u32` in a program's own
/// memory with no convention imposed on it — so unlike a `BlockingMutex` there
/// is nothing here to read an owner out of. The waiter therefore names one, and
/// the kernel lends to it for exactly as long as the wait lasts.
///
/// **The loan ends with the wait, not with the release.** Every other lock in
/// the kernel ends a loan when the holder lets go, because the kernel is what
/// the holder lets go *of*. Here the release is a userspace store the kernel
/// never sees, so the wait is the only span it can bound the loan by. A waiter
/// woken while the owner still holds the word re-lends on its next call, which
/// is the same loop every waiter already runs against a spurious wake.
///
/// `owner_tid` of 0 asks for no lending and makes this exactly `futex_wait`.
pub fn sys_futex_wait_pi(
    addr: *const u32,
    expected: u32,
    timeout_ns: u64,
    owner_tid: u64,
) -> Result<u64, Errno> {
    futex_wait_inner(addr, expected, timeout_ns, owner_tid)
}

fn futex_wait_inner(
    addr: *const u32,
    expected: u32,
    timeout_ns: u64,
    owner_tid: u64,
) -> Result<u64, Errno> {
    let info = current_thread_info();

    if addr.is_null() {
        return Err(Errno::EFAULT);
    }

    // SAFETY: `addr` is the caller's pointer to a `u32`, which has no
    // invalid bit patterns; the address is range-checked and a fault on it is
    // trapped rather than taken.
    let current = unsafe { try_read_user(addr) }.ok_or(Errno::EFAULT)?;

    if current != expected {
        return Ok(1);
    }

    let mm_arc = {
        let guard = info.lock();
        guard.memory_manager.clone()
    };
    let key = FutexKey::new(Arc::as_ptr(&mm_arc) as usize, addr as u64);
    let queue = queue_for(key);
    let timeout = if timeout_ns == u64::MAX {
        None
    } else {
        Some(Duration::from_nanos(timeout_ns))
    };
    let fault = Cell::new(None);
    let loan = FutexLoan::lend(owner_tid);
    let outcome = queue.wait_until_timeout(
        // SAFETY: as above. The word is re-read on every wake, which is what
        // makes the compare-and-park free of the lost-update race.
        || match unsafe { try_read_user(addr) } {
            Some(value) => value != expected,
            None => {
                fault.set(Some(Errno::EFAULT));
                true
            }
        },
        timeout,
    );
    loan.take_back();

    if let Some(err) = fault.get() {
        cleanup_if_empty(&key, &queue);
        return Err(err);
    }

    let result = match outcome {
        WaitOutcome::Ready => 1,
        // SAFETY: as above -- the word is read once more after the park to tell
        // a real wake from a timeout.
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
                return Err(Errno::EFAULT);
            }
        },
        WaitOutcome::TimedOut => 2,
        // `wait_until_timeout` is not killable, so it never reports this.
        WaitOutcome::Killed => unreachable!("futex wait is not killable"),
    };

    cleanup_if_empty(&key, &queue);

    Ok(result)
}

pub fn sys_futex_wake(addr: *const u32, count: u32) -> Result<u64, Errno> {
    let info = current_thread_info();

    // A wake never dereferences the word, it only keys the registry by its
    // address, so the range has to be checked here rather than being caught by
    // a copy the way `sys_futex_wait` is. Validated before the zero-count
    // short-circuit: waking nobody is still a claim about a real address.
    if addr.is_null() || !access_ok(addr as u64, size_of::<u32>()) {
        return Err(Errno::EFAULT);
    }

    if count == 0 {
        return Ok(0);
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
            None => return Ok(0),
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

    Ok(woken)
}
