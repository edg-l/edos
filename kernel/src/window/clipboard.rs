//! The session clipboard.
//!
//! Two byte buffers the kernel owns, so cut and paste work between programs
//! that share nothing else. The alternative every program agreeing on a path in
//! a filesystem is worse than it looks: a clipboard is meant to outlive the
//! program that filled it, and `/tmp` is a mount that need not be there.
//!
//! There are two buffers because a selection and a clipboard are different
//! things, as X established. [`CLIPBOARD`] is filled by an explicit copy and
//! read by an explicit paste. [`PRIMARY`] is filled merely by selecting text
//! and pasted with the middle mouse button, so selecting somewhere must not
//! destroy what was deliberately copied.
//!
//! Content is bytes rather than a string: the buffer is handed back exactly as
//! it arrived, and nothing here has to decide what a program meant by it.

use alloc::vec::Vec;

use crate::{debug::lock_order::RANK_CLIPBOARD, ranked_lock, thread::preempt::PreemptSpinlock};

/// Filled by an explicit copy.
pub const CLIPBOARD: u64 = 0;
/// Filled by selecting text.
pub const PRIMARY: u64 = 1;

/// The most either buffer will hold. Any process may write here and the memory
/// is never reclaimed until something else is copied, so an unbounded buffer is
/// a way to exhaust the kernel heap from userspace.
pub const MAX_LEN: usize = 64 * 1024;

struct Buffers {
    clipboard: Vec<u8>,
    primary: Vec<u8>,
}

static BUFFERS: PreemptSpinlock<Buffers> = PreemptSpinlock::new(Buffers {
    clipboard: Vec::new(),
    primary: Vec::new(),
});

/// True if `which` names a buffer.
pub fn is_valid(which: u64) -> bool {
    which == CLIPBOARD || which == PRIMARY
}

/// Replace the contents of `which`.
///
/// The caller copies out of userspace first: this takes a spin lock, and a
/// copy from a user pointer can demand-fault and park.
pub fn set(which: u64, bytes: Vec<u8>) {
    let mut buffers = ranked_lock!(RANK_CLIPBOARD, "clipboard::set", BUFFERS);
    if which == PRIMARY {
        buffers.primary = bytes;
    } else {
        buffers.clipboard = bytes;
    }
}

/// A copy of the contents of `which`.
///
/// Returns an owned copy rather than a guard for the same reason [`set`] takes
/// an owned vector: the caller has to be out of the lock before it touches a
/// user pointer.
pub fn get(which: u64) -> Vec<u8> {
    let buffers = ranked_lock!(RANK_CLIPBOARD, "clipboard::get", BUFFERS);
    if which == PRIMARY {
        buffers.primary.clone()
    } else {
        buffers.clipboard.clone()
    }
}
