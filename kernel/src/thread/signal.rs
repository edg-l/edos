use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// Signal numbers
#[expect(unused)]
pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGKILL: u32 = 9;
pub const SIGPIPE: u32 = 13;
#[expect(unused)]
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

/// `sigprocmask` operations.
pub const SIG_BLOCK: u32 = 0;
pub const SIG_UNBLOCK: u32 = 1;
pub const SIG_SETMASK: u32 = 2;

/// Signals that cannot be blocked, per POSIX: neither can be caught or
/// ignored either, which is what makes them the ones that always work.
const UNBLOCKABLE: u32 = (1 << SIGKILL) | (1 << SIGSTOP);

/// Signals whose disposition userspace may not change.
pub fn is_uncatchable(signum: u32) -> bool {
    signum == SIGKILL || signum == SIGSTOP
}

/// Default action for a signal.
pub enum DefaultAction {
    Terminate,
    Ignore,
    /// Suspend the thread until a `SIGCONT` arrives.
    Stop,
    /// Resume a stopped thread.
    Continue,
}

pub fn default_action(signum: u32) -> DefaultAction {
    match signum {
        SIGCHLD => DefaultAction::Ignore,
        SIGSTOP | SIGTSTP => DefaultAction::Stop,
        SIGCONT => DefaultAction::Continue,
        _ => DefaultAction::Terminate,
    }
}

/// Per-thread signal state. All fields are atomic -- no allocation needed.
pub struct SignalState {
    /// Bitmask of pending signals (bit N = signal N is pending).
    pub pending: AtomicU32,
    /// Bitmask of blocked signals. A blocked signal stays pending until
    /// `sigprocmask` unblocks it.
    blocked: AtomicU32,
    /// Per-signal disposition: `SIG_DFL`, `SIG_IGN`, or the address of a
    /// userspace handler. An address is distinguishable from either sentinel
    /// because no user function lives in the first page.
    handlers: [AtomicU64; 32],
    /// Address userspace wants a handler to return through, set with the
    /// first `sigaction` that installs one.
    ///
    /// The kernel cannot invent this: returning from a handler has to run a
    /// `sigreturn`, and the instructions that do so must live in the process's
    /// own text rather than on a stack the kernel would have to make
    /// executable.
    pub restorer: AtomicU64,
}

impl SignalState {
    pub const fn new() -> Self {
        // Must use const initialization for all 32 elements
        // Used only as the repeated element of an array initializer, where each
        // copy is a fresh value rather than a shared one.
        #[allow(clippy::declare_interior_mutable_const)]
        const INIT: AtomicU64 = AtomicU64::new(0);
        Self {
            pending: AtomicU32::new(0),
            blocked: AtomicU32::new(0),
            handlers: [INIT; 32],
            restorer: AtomicU64::new(0),
        }
    }

    /// The set of signals currently blocked.
    pub fn blocked(&self) -> u32 {
        self.blocked.load(Ordering::Acquire)
    }

    /// Replace the blocked set, returning the previous one. Signals that
    /// cannot be blocked are dropped from `mask` rather than rejected, as
    /// POSIX requires of `sigprocmask`.
    pub fn set_blocked(&self, mask: u32) -> u32 {
        self.blocked.swap(mask & !UNBLOCKABLE, Ordering::AcqRel)
    }

    /// Whether `signum` is blocked.
    pub fn is_blocked(&self, signum: u32) -> bool {
        if signum == 0 || signum >= 32 {
            return false;
        }
        self.blocked.load(Ordering::Acquire) & (1 << signum) != 0
    }

    /// Remove and return the pending signals the current mask does not block.
    pub fn take_deliverable(&self) -> u32 {
        let deliverable = self.pending.load(Ordering::Acquire) & !self.blocked();
        if deliverable != 0 {
            self.pending.fetch_and(!deliverable, Ordering::Release);
        }
        deliverable
    }

    /// Check if any signal is pending.
    #[expect(unused)]
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }

    /// Get the disposition for a signal.
    pub fn get_handler(&self, signum: u32) -> u64 {
        if signum == 0 || signum >= 32 {
            return SIG_DFL;
        }
        self.handlers[signum as usize].load(Ordering::Relaxed)
    }

    /// Whether `signum` has a userspace handler rather than a disposition.
    pub fn has_user_handler(&self, signum: u32) -> bool {
        self.get_handler(signum) > SIG_IGN
    }

    /// Set the disposition for a signal. Returns the previous value.
    pub fn set_handler(&self, signum: u32, handler: u64) -> u64 {
        if signum == 0 || signum >= 32 || is_uncatchable(signum) {
            return SIG_DFL;
        }
        self.handlers[signum as usize].swap(handler, Ordering::Relaxed)
    }

    /// Block `signum` for the duration of its own handler and return the mask
    /// to restore on `sigreturn`.
    ///
    /// POSIX: a handler does not interrupt itself.
    pub fn block_during_handler(&self, signum: u32) -> u32 {
        let previous = self.blocked.load(Ordering::Acquire);
        self.blocked
            .store((previous | (1 << signum)) & !UNBLOCKABLE, Ordering::Release);
        previous
    }

    /// Restore a mask saved by [`block_during_handler`].
    pub fn restore_blocked(&self, mask: u32) {
        self.blocked.store(mask & !UNBLOCKABLE, Ordering::Release);
    }

    /// Reset dispositions across `execve`.
    ///
    /// POSIX: signals set to be ignored stay ignored, everything else returns
    /// to the default, because the handlers belonged to the image being
    /// replaced. Pending signals are kept — they were sent to the process,
    /// which still exists.
    pub fn reset_for_exec(&self) {
        for handler in self.handlers.iter() {
            if handler.load(Ordering::Relaxed) != SIG_IGN {
                handler.store(SIG_DFL, Ordering::Relaxed);
            }
        }
    }

    /// Add a pending signal.
    pub fn send(&self, signum: u32) {
        if signum == 0 || signum >= 32 {
            return;
        }
        self.pending.fetch_or(1 << signum, Ordering::Release);
    }

    /// Check if a specific signal is pending.
    #[expect(unused)]
    pub fn is_pending(&self, signum: u32) -> bool {
        if signum == 0 || signum >= 32 {
            return false;
        }
        self.pending.load(Ordering::Acquire) & (1 << signum) != 0
    }

    /// Clear a specific pending signal.
    pub fn clear(&self, signum: u32) {
        if signum == 0 || signum >= 32 {
            return;
        }
        self.pending.fetch_and(!(1 << signum), Ordering::Release);
    }
}

impl core::fmt::Debug for SignalState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "SignalState {{ pending: 0x{:08x} }}",
            self.pending.load(Ordering::Relaxed)
        )
    }
}
