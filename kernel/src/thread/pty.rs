use crate::{
    fs::{
        PollState,
        handle::{PollEntry, PollKey, PollRegistration, Pollable},
    },
    thread::{mutex::BlockingMutex, waitqueue::WaitQueue},
    util::uaccess::{try_copy_from_user, try_copy_to_user},
};
use alloc::{sync::Arc, vec::Vec};

#[derive(Debug)]
#[allow(unused)]
pub struct Pty {
    /// Data written by master (keyboard input), consumed by slave readers.
    pub input_buf: Vec<u8>,
    /// Data written by slave (program output), consumed by master readers.
    pub output_buf: Vec<u8>,
    /// Number of master file descriptor handles open.
    pub masters: usize,
    /// Number of slave file descriptor handles open.
    pub slaves: usize,
    pub closed_master: bool,
    pub closed_slave: bool,
    /// Wakes slave readers when input_buf gets data or master closes.
    input_wq: Arc<WaitQueue>,
    /// Wakes master readers when output_buf gets data or slave closes.
    output_wq: Arc<WaitQueue>,
    pollers: Vec<(PollKey, Arc<PollEntry>)>,
    next_poll_key: PollKey,
}

#[allow(unused)]
impl Pty {
    pub fn new() -> Self {
        Self {
            input_buf: Vec::new(),
            output_buf: Vec::new(),
            masters: 1,
            slaves: 1,
            closed_master: false,
            closed_slave: false,
            input_wq: Arc::new(WaitQueue::new()),
            output_wq: Arc::new(WaitQueue::new()),
            pollers: Vec::new(),
            next_poll_key: 1,
        }
    }

    /// Clone the input WaitQueue Arc so callers can wait outside the lock.
    pub fn input_wq(&self) -> Arc<WaitQueue> {
        self.input_wq.clone()
    }

    /// Clone the output WaitQueue Arc so callers can wait outside the lock.
    pub fn output_wq(&self) -> Arc<WaitQueue> {
        self.output_wq.clone()
    }

    /// Master writes keyboard input into input_buf (slave reads this).
    pub fn master_write_from_user(
        &mut self,
        user_ptr: *const u8,
        len: usize,
    ) -> (Option<usize>, PtyNotifications) {
        if len == 0 {
            return (Some(0), PtyNotifications::EMPTY);
        }

        let start = self.input_buf.len();
        self.input_buf.resize(start + len, 0);

        if !unsafe { try_copy_from_user(self.input_buf[start..].as_mut_ptr(), user_ptr, len) } {
            self.input_buf.truncate(start);
            return (None, PtyNotifications::EMPTY);
        }

        (Some(len), self.notify_pollers())
    }

    /// Master reads program output from output_buf (slave wrote this).
    pub fn master_read_to_user(
        &mut self,
        user_ptr: *mut u8,
        count: usize,
    ) -> (Option<usize>, PtyNotifications) {
        if count == 0 {
            return (Some(0), PtyNotifications::EMPTY);
        }

        let available = count.min(self.output_buf.len());
        if available == 0 {
            return (Some(0), PtyNotifications::EMPTY);
        }

        if !unsafe { try_copy_to_user(user_ptr, self.output_buf.as_ptr(), available) } {
            return (None, PtyNotifications::EMPTY);
        }

        self.output_buf.drain(..available);
        (Some(available), self.notify_pollers())
    }

    /// Slave writes program output into output_buf (master reads this).
    pub fn slave_write_from_user(
        &mut self,
        user_ptr: *const u8,
        len: usize,
    ) -> (Option<usize>, PtyNotifications) {
        if len == 0 {
            return (Some(0), PtyNotifications::EMPTY);
        }

        let start = self.output_buf.len();
        self.output_buf.resize(start + len, 0);

        if !unsafe { try_copy_from_user(self.output_buf[start..].as_mut_ptr(), user_ptr, len) } {
            self.output_buf.truncate(start);
            return (None, PtyNotifications::EMPTY);
        }

        (Some(len), self.notify_pollers())
    }

    /// Slave reads keyboard input from input_buf (master wrote this).
    pub fn slave_read_to_user(
        &mut self,
        user_ptr: *mut u8,
        count: usize,
    ) -> (Option<usize>, PtyNotifications) {
        if count == 0 {
            return (Some(0), PtyNotifications::EMPTY);
        }

        let available = count.min(self.input_buf.len());
        if available == 0 {
            return (Some(0), PtyNotifications::EMPTY);
        }

        if !unsafe { try_copy_to_user(user_ptr, self.input_buf.as_ptr(), available) } {
            return (None, PtyNotifications::EMPTY);
        }

        self.input_buf.drain(..available);
        (Some(available), self.notify_pollers())
    }

    /// Decrement master refcount; set closed_master when it reaches zero.
    pub fn close_master(&mut self) -> PtyNotifications {
        self.masters = self.masters.saturating_sub(1);
        if self.masters == 0 {
            self.closed_master = true;
        }
        self.notify_pollers()
    }

    /// Decrement slave refcount; set closed_slave when it reaches zero.
    pub fn close_slave(&mut self) -> PtyNotifications {
        self.slaves = self.slaves.saturating_sub(1);
        if self.slaves == 0 {
            self.closed_slave = true;
        }
        self.notify_pollers()
    }

    fn poll_state_master(&self) -> PollState {
        let mut state = PollState::none();

        if !self.output_buf.is_empty() || self.closed_slave {
            state.readable = true;
        }

        if self.slaves > 0 && !self.closed_slave {
            state.writable = true;
        }

        if self.closed_slave && self.output_buf.is_empty() {
            state.hangup = true;
        }

        state
    }

    fn poll_state_slave(&self) -> PollState {
        let mut state = PollState::none();

        if !self.input_buf.is_empty() || self.closed_master {
            state.readable = true;
        }

        if self.masters > 0 && !self.closed_master {
            state.writable = true;
        }

        if self.closed_master && self.input_buf.is_empty() {
            state.hangup = true;
        }

        state
    }

    pub fn add_poller(&mut self, entry: Arc<PollEntry>) -> PollKey {
        let key = self.next_poll_key;
        self.next_poll_key = self.next_poll_key.wrapping_add(1).max(1);
        self.pollers.push((key, entry));
        key
    }

    pub fn remove_poller(&mut self, key: PollKey) {
        self.pollers.retain(|(stored, _)| *stored != key);
    }

    /// Snapshot pollers + current state for deferred notification after lock drop.
    fn notify_pollers(&mut self) -> PtyNotifications {
        let master_state = self.poll_state_master();
        let slave_state = self.poll_state_slave();

        let wake_input = slave_state.readable || slave_state.hangup;
        let wake_output = master_state.readable || master_state.hangup;

        let input_wq = if wake_input {
            Some(self.input_wq.clone())
        } else {
            None
        };
        let output_wq = if wake_output {
            Some(self.output_wq.clone())
        } else {
            None
        };

        if self.pollers.is_empty() {
            return PtyNotifications {
                entries: heapless::Vec::new(),
                master_state,
                slave_state,
                input_wq,
                output_wq,
            };
        }

        let mut entries: heapless::Vec<Arc<PollEntry>, 8> = heapless::Vec::new();
        for (_, entry) in self.pollers.iter() {
            let _ = entries.push(entry.clone());
        }

        PtyNotifications {
            entries,
            master_state,
            slave_state,
            input_wq,
            output_wq,
        }
    }
}

/// Deferred PTY notifications flushed after releasing the PTY lock.
pub struct PtyNotifications {
    entries: heapless::Vec<Arc<PollEntry>, 8>,
    master_state: PollState,
    slave_state: PollState,
    input_wq: Option<Arc<WaitQueue>>,
    output_wq: Option<Arc<WaitQueue>>,
}

impl PtyNotifications {
    const EMPTY: Self = Self {
        entries: heapless::Vec::new(),
        master_state: PollState::none(),
        slave_state: PollState::none(),
        input_wq: None,
        output_wq: None,
    };

    /// Send all notifications. Call this after dropping the PTY lock.
    pub fn flush(self) {
        for entry in &self.entries {
            // Update each poller with the appropriate state depending on whether
            // it is a master or slave poller. Since we cannot distinguish here,
            // we merge both states for simplicity -- callers use separate
            // PollablePtyMaster / PollablePtySlave that register separately.
            // The state stored on the PollEntry is set at register time from the
            // correct side, so we send whichever is non-zero.
            let state = if self.master_state.readable
                || self.master_state.writable
                || self.master_state.hangup
            {
                self.master_state
            } else {
                self.slave_state
            };
            entry.update(state);
        }
        if let Some(wq) = &self.input_wq {
            wq.wake_one();
        }
        if let Some(wq) = &self.output_wq {
            wq.wake_one();
        }
    }
}

// ---------------------------------------------------------------------------
// Pollable implementations
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PollablePtyMaster {
    inner: Arc<BlockingMutex<Pty>>,
}

impl PollablePtyMaster {
    pub fn new(pty: Arc<BlockingMutex<Pty>>) -> Self {
        Self { inner: pty }
    }
}

impl Pollable for PollablePtyMaster {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
        let mut pty = self.inner.lock();
        let state = pty.poll_state_master();
        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = pty.add_poller(entry);
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        self.inner.lock().remove_poller(key);
    }
}

#[derive(Clone, Debug)]
pub struct PollablePtySlave {
    inner: Arc<BlockingMutex<Pty>>,
}

impl PollablePtySlave {
    pub fn new(pty: Arc<BlockingMutex<Pty>>) -> Self {
        Self { inner: pty }
    }
}

impl Pollable for PollablePtySlave {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
        let mut pty = self.inner.lock();
        let state = pty.poll_state_slave();
        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = pty.add_poller(entry);
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        self.inner.lock().remove_poller(key);
    }
}
