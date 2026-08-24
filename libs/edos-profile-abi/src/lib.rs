//! The ABI between the kernel's sampling profiler and userspace `profile`.
//!
//! Both sides link this crate, so the sample layout cannot drift apart: a
//! change here fails to compile on whichever side was not updated.
//!
//! A sample is one interrupted instruction pointer plus the return addresses
//! above it, captured in the timer interrupt. It carries raw addresses only —
//! the kernel resolves no symbols, because the ELF files with the DWARF in
//! them live on the build host and `addr2line` already reads them there.

#![no_std]

/// Return addresses one sample can carry, the interrupted RIP included.
///
/// A sample is fixed size so the ring is a plain array and a drain is one
/// `try_copy_to_user`. 32 frames is the depth the panic handler prints and is
/// deep enough for every stack in this kernel; a longer one is truncated and
/// says so through [`SAMPLE_TRUNCATED`].
pub const MAX_FRAMES: usize = 32;

/// The interrupted code was running in ring 3, so [`Sample::frames`] is a user
/// stack. Absent means ring 0 and a kernel stack.
pub const SAMPLE_USER: u32 = 1 << 0;

/// The frame walk stopped at [`MAX_FRAMES`] rather than at the end of the
/// stack, so the outermost frames are missing.
pub const SAMPLE_TRUNCATED: u32 = 1 << 1;

/// The walk stopped early because a frame pointer left the stack it was
/// walking, or a user page was not present. `frames` is a valid prefix.
pub const SAMPLE_BROKEN_CHAIN: u32 = 1 << 2;

/// No thread was running: the CPU was interrupted in its idle loop.
pub const SAMPLE_IDLE: u32 = 1 << 3;

/// One sample.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Sample {
    /// Nanoseconds since boot.
    pub time_ns: u64,
    /// Thread the CPU was running, or 0 when idle.
    ///
    /// The only identity a sample carries. Everything else about the thread —
    /// its command line, its address space, where its binary is mapped — is in
    /// procfs under this number, and the profiler reads it there rather than
    /// having the interrupt handler take a lock to find out.
    pub tid: u64,
    /// CPU the sample was taken on.
    pub cpu: u32,
    /// `SAMPLE_*` bits.
    pub flags: u32,
    /// Valid entries in `frames`, innermost first. Never zero: the
    /// interrupted RIP is always frame 0.
    pub depth: u32,
    pub _pad: u32,
    pub frames: [u64; MAX_FRAMES],
}

impl Sample {
    pub const fn zeroed() -> Self {
        Self {
            time_ns: 0,
            tid: 0,
            cpu: 0,
            flags: 0,
            depth: 0,
            _pad: 0,
            frames: [0; MAX_FRAMES],
        }
    }

    /// The frames actually captured, innermost first.
    pub fn stack(&self) -> &[u64] {
        let depth = (self.depth as usize).min(MAX_FRAMES);
        &self.frames[..depth]
    }

    pub fn is_user(&self) -> bool {
        self.flags & SAMPLE_USER != 0
    }
}

/// What a session has done so far, answered by [`ctl::STATS`].
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Stats {
    /// Samples written to the ring since the session was claimed.
    pub taken: u64,
    /// Samples the interrupt handler had to throw away because the ring was
    /// full. A profile with a non-zero count here is missing time, and the
    /// gap is not evenly spread: it lands wherever the profiler was slowest
    /// to drain.
    pub dropped: u64,
    /// Sampling period actually in force, which is the requested one raised
    /// to the timer's own floor.
    pub period_ns: u64,
    /// Samples waiting in the ring.
    pub queued: u64,
}

/// `profile_ctl` operations.
pub mod ctl {
    /// Claim the session and start sampling. `arg` is the requested period in
    /// nanoseconds. Fails with `EBUSY` if another profiler holds it.
    pub const START: u64 = 0;
    /// Stop sampling and release the session, freeing the ring.
    pub const STOP: u64 = 1;
    /// Copy a [`super::Stats`] to `arg`, which points at one in user memory.
    pub const STATS: u64 = 2;
}

/// Slowest sampling period a session may ask for: one sample every 100 ms.
pub const MAX_PERIOD_NS: u64 = 100_000_000;

/// Fastest sampling period a session may ask for. 50 us is 20 kHz, which is
/// far past useful and is here only to stop a caller asking for a period the
/// handler cannot finish inside.
pub const MIN_PERIOD_NS: u64 = 50_000;
