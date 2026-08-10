//! Which processes may manage windows they do not own.
//!
//! Moving, resizing, framing, minimizing and focusing a window are operations
//! a *shell* performs on windows belonging to other processes, so they cannot
//! be gated on ownership. Until now they were gated on nothing at all: any
//! process could move any window, put it away, or push a synthetic close event
//! into it.
//!
//! Authority is granted, not claimed. The kernel starts exactly one process,
//! `bin/edos-init`, and init decides what a session is; so init holds this
//! privilege from the start and hands it to the processes it appoints. A
//! first-caller-wins claim would not work here anyway: the compositor and the
//! panel are separate processes and both need it, and whichever raced first
//! would lock the other out.
//!
//! The grant is per pid and dies with the process, so a pid cannot inherit
//! authority by being reused.

use alloc::vec::Vec;

use crate::debug::lock_order::RANK_WINDOW_SHELL;
use crate::thread::preempt::PreemptRwLock;
use crate::thread::thread::init_pid;
use crate::{ranked_read, ranked_write};

/// Processes init has appointed. Init itself is not listed: it is privileged by
/// being the process the kernel started.
///
/// Ranked outside the window registry, so a caller's authority is settled
/// before the registry is touched. The two must not be co-held: they would be
/// two locks of the same rank otherwise, which the tracker rejects, and it
/// caught exactly that the first time this was written.
static SHELL_PIDS: PreemptRwLock<Vec<u64>> = PreemptRwLock::new(Vec::new());

/// A session is a handful of processes, but the privilege is held per thread
/// id, and a compositor with worker threads holds several. The bound exists so
/// a buggy grantor cannot grow this without limit, not to be tight.
const MAX_SHELL_PIDS: usize = 32;

/// Whether `pid` may manage windows it does not own.
pub fn is_shell(pid: u64) -> bool {
    if pid != 0 && pid == init_pid() {
        return true;
    }
    ranked_read!(RANK_WINDOW_SHELL, "window::shell", SHELL_PIDS).contains(&pid)
}

/// Appoint `pid`. Returns false if the table is full.
pub fn grant(pid: u64) -> bool {
    let mut pids = ranked_write!(RANK_WINDOW_SHELL, "window::shell::grant", SHELL_PIDS);
    if pids.contains(&pid) {
        return true;
    }
    if pids.len() >= MAX_SHELL_PIDS {
        return false;
    }
    pids.push(pid);
    true
}

/// Drop `pid`'s appointment. Called from process teardown, so a later process
/// that happens to reuse the number does not inherit the privilege.
pub fn revoke(pid: u64) {
    ranked_write!(RANK_WINDOW_SHELL, "window::shell::revoke", SHELL_PIDS).retain(|p| *p != pid);
}
