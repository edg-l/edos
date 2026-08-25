//! The syscall face of the sampling profiler. The sampler itself is
//! [`crate::profile`]; this is the session's start, stop and drain.

use alloc::vec::Vec;

use edos_profile_abi::{Sample, Stats, ctl};

use crate::{
    profile::{self as sampler, MAX_READ_BATCH},
    syscalls::{Errno, current_thread_id, current_thread_info, fail_with},
    util::uaccess::try_copy_to_user,
};

/// Start, stop, or report on the session.
///
/// `START` returns the period actually in force, which is the requested one
/// clamped to what the sampler accepts.
pub fn sys_profile_ctl(op: u64, arg: u64) -> u64 {
    current_thread_info().lock().errno = Errno::Clear;

    let Some(caller) = current_thread_id() else {
        return fail_with(Errno::EINVAL);
    };

    match op {
        ctl::START => match sampler::start(caller.0, arg) {
            Ok(period) => period,
            Err(()) => fail_with(Errno::EBUSY),
        },
        ctl::STOP => {
            if sampler::owner_tid() != caller.0 {
                return fail_with(Errno::EPERM);
            }
            sampler::stop();
            0
        }
        ctl::STATS => {
            let stats = sampler::stats();
            let ok = unsafe {
                try_copy_to_user(
                    arg as *mut u8,
                    core::ptr::addr_of!(stats) as *const u8,
                    core::mem::size_of::<Stats>(),
                )
            };
            if ok { 0 } else { fail_with(Errno::EFAULT) }
        }
        _ => fail_with(Errno::EINVAL),
    }
}

/// Drain up to `max` samples into the caller's buffer, parking for up to
/// `timeout_ms` if none are waiting.
///
/// Returns the number of samples written, which is 0 on timeout.
pub fn sys_profile_read(dst: *mut Sample, max: u64, timeout_ms: u64) -> u64 {
    current_thread_info().lock().errno = Errno::Clear;

    if dst.is_null() || max == 0 {
        return fail_with(Errno::EINVAL);
    }
    let max = (max as usize).min(MAX_READ_BATCH);

    // The session owner's alone, for the reason the tracer's read is: anyone
    // else draining takes samples the profiler then never sees, and anyone
    // else parking below adds a second waiter to a queue this keeps at depth
    // one.
    let Some(caller) = current_thread_id() else {
        return fail_with(Errno::EINVAL);
    };
    if sampler::owner_tid() != caller.0 {
        return fail_with(Errno::EPERM);
    }

    if timeout_ms > 0 {
        sampler::wait_for_samples(timeout_ms);
    }

    let mut batch: Vec<Sample> = Vec::new();
    if batch.try_reserve_exact(max).is_err() {
        return fail_with(Errno::ENOMEM);
    }
    if sampler::drain(&mut batch, max) == 0 {
        return 0;
    }

    let bytes = core::mem::size_of_val(batch.as_slice());
    if !unsafe { try_copy_to_user(dst as *mut u8, batch.as_ptr() as *const u8, bytes) } {
        // The samples are already out of the ring; a profiler that hands the
        // kernel an unwritable buffer loses them, which is its own doing.
        return fail_with(Errno::EFAULT);
    }

    batch.len() as u64
}
