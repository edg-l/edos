//! The syscall face of the sampling profiler. The sampler itself is
//! [`crate::profile`]; this is the session's start, stop and drain.

use alloc::vec::Vec;

use edos_profile_abi::{Sample, Stats, ctl};

use crate::{
    profile::{self as sampler, MAX_READ_BATCH},
    syscalls::{Errno, current_thread_id},
    util::uaccess::try_copy_to_user,
};

/// Start, stop, or report on the session.
///
/// `START` returns the period actually in force, which is the requested one
/// clamped to what the sampler accepts.
pub fn sys_profile_ctl(op: u64, arg: u64) -> Result<u64, Errno> {
    let Some(caller) = current_thread_id() else {
        return Err(Errno::EINVAL);
    };

    match op {
        ctl::START => sampler::start(caller.0, arg).map_err(|()| Errno::EBUSY),
        ctl::STOP => {
            if sampler::owner_tid() != caller.0 {
                return Err(Errno::EPERM);
            }
            sampler::stop();
            Ok(0)
        }
        ctl::STATS => {
            let stats = sampler::stats();
            // SAFETY: `stats` is a live local, so the source is valid for exactly
            // the `size_of::<Stats>()` bytes named. The destination is the caller's
            // pointer, which `try_copy_to_user` range-checks and faults through.
            let ok = unsafe {
                try_copy_to_user(
                    arg as *mut u8,
                    core::ptr::addr_of!(stats) as *const u8,
                    core::mem::size_of::<Stats>(),
                )
            };
            if ok { Ok(0) } else { Err(Errno::EFAULT) }
        }
        _ => Err(Errno::EINVAL),
    }
}

/// Drain up to `max` samples into the caller's buffer, parking for up to
/// `timeout_ms` if none are waiting.
///
/// Returns the number of samples written, which is 0 on timeout.
pub fn sys_profile_read(dst: *mut Sample, max: u64, timeout_ms: u64) -> Result<u64, Errno> {
    if dst.is_null() || max == 0 {
        return Err(Errno::EINVAL);
    }
    let max = (max as usize).min(MAX_READ_BATCH);

    // The session owner's alone, for the reason the tracer's read is: anyone
    // else draining takes samples the profiler then never sees, and anyone
    // else parking below adds a second waiter to a queue this keeps at depth
    // one.
    let Some(caller) = current_thread_id() else {
        return Err(Errno::EINVAL);
    };
    if sampler::owner_tid() != caller.0 {
        return Err(Errno::EPERM);
    }

    if timeout_ms > 0 {
        sampler::wait_for_samples(timeout_ms);
    }

    let mut batch: Vec<Sample> = Vec::new();
    if batch.try_reserve_exact(max).is_err() {
        return Err(Errno::ENOMEM);
    }
    if sampler::drain(&mut batch, max) == 0 {
        return Ok(0);
    }

    let bytes = core::mem::size_of_val(batch.as_slice());
    // SAFETY: `bytes` is `size_of_val` of the batch's own slice, so the
    // source is valid for the length named.
    if !unsafe { try_copy_to_user(dst as *mut u8, batch.as_ptr() as *const u8, bytes) } {
        // The samples are already out of the ring; a profiler that hands the
        // kernel an unwritable buffer loses them, which is its own doing.
        return Err(Errno::EFAULT);
    }

    Ok(batch.len() as u64)
}
