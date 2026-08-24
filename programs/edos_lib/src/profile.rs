//! The sampling profiler: claim the session, drain samples.
//!
//! The sample layout comes from `edos_profile_abi`, which the kernel links
//! too.

use crate::sys;

pub use edos_profile_abi::{
    MAX_FRAMES, MAX_PERIOD_NS, MIN_PERIOD_NS, SAMPLE_BROKEN_CHAIN, SAMPLE_IDLE, SAMPLE_TRUNCATED,
    SAMPLE_USER, Sample, Stats, ctl,
};

/// Claim the session and start sampling every `period_ns` on every CPU.
///
/// Returns the period actually in force, which is the requested one clamped to
/// [`MIN_PERIOD_NS`]..[`MAX_PERIOD_NS`] and then raised by the kernel to its
/// own timer floor. `None` means another live process holds the session. It
/// ends when [`stop`] is called or the caller exits, whichever comes first.
pub fn start(period_ns: u64) -> Option<u64> {
    let ret = unsafe { sys::syscall2(sys::SYS_PROFILE_CTL, ctl::START, period_ns) };
    if sys::is_err(ret) { None } else { Some(ret) }
}

/// Stop sampling and free the ring. Session owner only.
pub fn stop() -> bool {
    unsafe { sys::syscall2(sys::SYS_PROFILE_CTL, ctl::STOP, 0) == 0 }
}

/// What the session has done so far.
pub fn stats() -> Option<Stats> {
    let mut stats = Stats::default();
    let ret = unsafe {
        sys::syscall2(
            sys::SYS_PROFILE_CTL,
            ctl::STATS,
            std::ptr::addr_of_mut!(stats) as u64,
        )
    };
    if sys::is_err(ret) { None } else { Some(stats) }
}

/// Drain up to `buf.len()` samples, parking for at most `timeout_ms` if none
/// are waiting.
///
/// Returns the number of samples written, which is 0 on timeout.
pub fn read(buf: &mut [Sample], timeout_ms: u64) -> usize {
    let n = unsafe {
        sys::syscall3(
            sys::SYS_PROFILE_READ,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            timeout_ms,
        )
    };
    if sys::is_err(n) { 0 } else { n as usize }
}

/// Convert a sampling frequency in hertz to the period the kernel wants.
pub fn hz_to_period_ns(hz: u64) -> u64 {
    if hz == 0 {
        return MAX_PERIOD_NS;
    }
    (1_000_000_000 / hz).clamp(MIN_PERIOD_NS, MAX_PERIOD_NS)
}
