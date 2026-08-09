use core::sync::atomic::{AtomicU32, Ordering};

// Signal numbers
#[expect(unused)]
pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGKILL: u32 = 9;
#[expect(unused)]
pub const SIGPIPE: u32 = 13;
#[expect(unused)]
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;

#[expect(unused)]
pub const SIG_DFL: u32 = 0;
pub const SIG_IGN: u32 = 1;

/// `sigprocmask` operations.
pub const SIG_BLOCK: u32 = 0;
pub const SIG_UNBLOCK: u32 = 1;
pub const SIG_SETMASK: u32 = 2;

/// Signals that cannot be blocked. POSIX also names SIGSTOP, which does not
/// exist here because nothing stops a process short of killing it.
const UNBLOCKABLE: u32 = 1 << SIGKILL;

/// Default action for a signal.
pub enum DefaultAction {
    Terminate,
    Ignore,
}

pub fn default_action(signum: u32) -> DefaultAction {
    match signum {
        SIGCHLD => DefaultAction::Ignore,
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
    /// Per-signal disposition. 0 = SIG_DFL, 1 = SIG_IGN.
    /// (No userspace handlers in this simplified version.)
    handlers: [AtomicU32; 32],
}

impl SignalState {
    pub const fn new() -> Self {
        // Must use const initialization for all 32 elements
        const INIT: AtomicU32 = AtomicU32::new(0);
        Self {
            pending: AtomicU32::new(0),
            blocked: AtomicU32::new(0),
            handlers: [INIT; 32],
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

    /// Get the disposition for a signal (SIG_DFL=0 or SIG_IGN=1).
    pub fn get_handler(&self, signum: u32) -> u32 {
        if signum == 0 || signum >= 32 {
            return 0;
        }
        self.handlers[signum as usize].load(Ordering::Relaxed)
    }

    /// Set the disposition for a signal. Returns the previous value.
    pub fn set_handler(&self, signum: u32, handler: u32) -> u32 {
        if signum == 0 || signum >= 32 || signum == SIGKILL {
            return 0;
        }
        self.handlers[signum as usize].swap(handler, Ordering::Relaxed)
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
    #[expect(unused)]
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
