use crate::{
    fs::{
        PollState,
        handle::{PollKey, PollRef, PollRegistration, Pollable},
    },
    thread::{mutex::BlockingMutex, signal, waitqueue::WaitQueue},
};
use alloc::{sync::Arc, vec::Vec};

/// Outcome of a slave-side read.
///
/// `Eof` and `WouldBlock` are both zero bytes and mean opposite things: the
/// first is Ctrl-D, which POSIX reports as a zero-length read, and the second
/// means park until the master writes. Collapsing them is why Ctrl-D used to
/// hang until the master closed.
pub enum PtySlaveRead {
    Data(Vec<u8>),
    Eof,
    WouldBlock,
}

pub const PTY_IOCTL_SET_RAW: u64 = 0x5001;
/// Record the character grid the terminal draws, packed `(rows << 16) | cols`.
/// Set by the terminal on resize, read by full-screen programs.
pub const PTY_IOCTL_SET_WINSIZE: u64 = 0x5004;
pub const PTY_IOCTL_GET_WINSIZE: u64 = 0x5005;
pub const PTY_IOCTL_SET_CANONICAL: u64 = 0x5002;
pub const PTY_IOCTL_GET_MODE: u64 = 0x5003;

/// Which side of the PTY a poller is registered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtySide {
    Master,
    Slave,
}

#[derive(Debug)]
pub enum LineAction {
    None,
    Eof,
    Interrupt,
    Suspend,
}

#[derive(Debug)]
struct LineDiscipline {
    canonical: bool,
    echo: bool,
    line_buf: Vec<u8>,
}

impl LineDiscipline {
    fn new() -> Self {
        Self {
            canonical: true,
            echo: true,
            line_buf: Vec::new(),
        }
    }

    fn process_input(
        &mut self,
        byte: u8,
        input_buf: &mut Vec<u8>,
        output_buf: &mut Vec<u8>,
    ) -> LineAction {
        // Ctrl+C and Ctrl+Z reach the foreground job in both raw and
        // canonical mode, and neither reaches the reader as data.
        if byte == 0x03 {
            self.line_buf.clear();
            if self.echo {
                output_buf.extend_from_slice(b"^C\n");
            }
            return LineAction::Interrupt;
        }

        if byte == 0x1a {
            self.line_buf.clear();
            if self.echo {
                output_buf.extend_from_slice(b"^Z\n");
            }
            return LineAction::Suspend;
        }

        if !self.canonical {
            input_buf.push(byte);
            if self.echo {
                output_buf.push(byte);
            }
            return LineAction::None;
        }

        // Canonical mode
        match byte {
            // Enter / carriage return
            b'\r' | b'\n' => {
                self.line_buf.push(b'\n');
                input_buf.extend_from_slice(&self.line_buf);
                self.line_buf.clear();
                if self.echo {
                    output_buf.push(b'\n');
                }
                LineAction::None
            }
            // Backspace (DEL or BS)
            0x7F | 0x08 => {
                if !self.line_buf.is_empty() {
                    self.line_buf.pop();
                    if self.echo {
                        output_buf.extend_from_slice(b"\x08 \x08");
                    }
                }
                LineAction::None
            }
            // Ctrl+D (EOF)
            0x04 => {
                if self.line_buf.is_empty() {
                    LineAction::Eof
                } else {
                    input_buf.extend_from_slice(&self.line_buf);
                    self.line_buf.clear();
                    LineAction::None
                }
            }
            // Ctrl+C is handled above (before the canonical/raw check)
            // Tab
            b'\t' => {
                self.line_buf.push(byte);
                if self.echo {
                    output_buf.push(byte);
                }
                LineAction::None
            }
            // Escape (start of escape sequences)
            0x1B => {
                self.line_buf.push(byte);
                if self.echo {
                    output_buf.push(byte);
                }
                LineAction::None
            }
            // Printable bytes
            byte if byte >= 0x20 => {
                self.line_buf.push(byte);
                if self.echo {
                    output_buf.push(byte);
                }
                LineAction::None
            }
            // Other control characters: drop silently
            _ => LineAction::None,
        }
    }
}

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
    pollers: Vec<(PollKey, PtySide, PollRef)>,
    next_poll_key: PollKey,
    line_disc: LineDiscipline,
    eof_pending: bool,
    /// PID of the foreground process that should receive Ctrl+C signals.
    /// Process group signals from the line discipline are aimed at.
    ///
    /// A group rather than a pid because Ctrl+C belongs to the foreground
    /// *job*: `ls | grep x` is two processes and both must get it.
    pub foreground_pgid: Option<u64>,
    /// The character grid the terminal is drawing, so a full-screen program can
    /// ask instead of assuming. A program that assumes draws off the bottom of
    /// the window the moment the grid changes size, and nothing tells it.
    cols: u16,
    rows: u16,
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
            line_disc: LineDiscipline::new(),
            eof_pending: false,
            foreground_pgid: None,
            cols: 80,
            rows: 30,
        }
    }

    pub fn ioctl(&mut self, request: u64) -> Result<u64, ()> {
        self.ioctl_with(request, 0)
    }

    pub fn ioctl_with(&mut self, request: u64, arg: u64) -> Result<u64, ()> {
        match request {
            PTY_IOCTL_SET_WINSIZE => {
                let cols = (arg & 0xFFFF) as u16;
                let rows = ((arg >> 16) & 0xFFFF) as u16;
                if cols == 0 || rows == 0 {
                    return Err(());
                }
                self.cols = cols;
                self.rows = rows;
                Ok(0)
            }
            PTY_IOCTL_GET_WINSIZE => Ok((self.rows as u64) << 16 | self.cols as u64),
            PTY_IOCTL_SET_RAW => {
                self.line_disc.canonical = false;
                self.line_disc.echo = false;
                Ok(0)
            }
            PTY_IOCTL_SET_CANONICAL => {
                self.line_disc.canonical = true;
                self.line_disc.echo = true;
                Ok(0)
            }
            PTY_IOCTL_GET_MODE => Ok(if self.line_disc.canonical { 1 } else { 0 }),
            _ => Err(()),
        }
    }

    /// Clone the input WaitQueue Arc so callers can wait outside the lock.
    pub fn input_wq(&self) -> Arc<WaitQueue> {
        self.input_wq.clone()
    }

    /// Master writes keyboard input into input_buf (slave reads this).
    ///
    /// `data` is already in kernel memory: the caller copies it out of user
    /// space before taking the pty lock, because a user copy can demand fault
    /// and park, and a thread killed while parked never runs the guard's Drop.
    pub fn master_write(&mut self, data: &[u8]) -> (usize, PtyNotifications) {
        let len = data.len();
        if len == 0 {
            return (0, PtyNotifications::EMPTY);
        }

        // Process each byte through the line discipline.
        let mut signal_pgid: Option<(u64, u32)> = None;
        for &byte in data {
            // Borrow fields separately to satisfy the borrow checker.
            let action =
                self.line_disc
                    .process_input(byte, &mut self.input_buf, &mut self.output_buf);
            match action {
                LineAction::Eof => {
                    self.eof_pending = true;
                }
                LineAction::Interrupt => {
                    signal_pgid = self.foreground_pgid.map(|pgid| (pgid, signal::SIGINT));
                }
                LineAction::Suspend => {
                    signal_pgid = self.foreground_pgid.map(|pgid| (pgid, signal::SIGTSTP));
                }
                LineAction::None => {}
            }
        }

        let mut notif = self.notify_pollers();
        notif.signal_pgid = signal_pgid;
        (len, notif)
    }

    /// Master reads program output from output_buf (slave wrote this).
    ///
    /// Returns the drained bytes; the caller copies them to user space after
    /// releasing the pty lock.
    pub fn master_read(&mut self, count: usize) -> (Vec<u8>, PtyNotifications) {
        if count == 0 {
            return (Vec::new(), PtyNotifications::EMPTY);
        }

        let available = count.min(self.output_buf.len());
        if available == 0 {
            return (Vec::new(), PtyNotifications::EMPTY);
        }

        let out: Vec<u8> = self.output_buf.drain(..available).collect();
        (out, self.notify_pollers())
    }

    /// Slave writes program output into output_buf (master reads this).
    pub fn slave_write(&mut self, data: &[u8]) -> (usize, PtyNotifications) {
        if data.is_empty() {
            return (0, PtyNotifications::EMPTY);
        }

        self.output_buf.extend_from_slice(data);
        (data.len(), self.notify_pollers())
    }

    /// Slave reads keyboard input from input_buf (master wrote this).
    pub fn slave_read(&mut self, count: usize) -> (PtySlaveRead, PtyNotifications) {
        if count == 0 {
            return (PtySlaveRead::WouldBlock, PtyNotifications::EMPTY);
        }

        // Deliver EOF once when input is empty and eof_pending is set.
        if self.input_buf.is_empty() && self.eof_pending {
            self.eof_pending = false;
            return (PtySlaveRead::Eof, self.notify_pollers());
        }

        let available = count.min(self.input_buf.len());
        if available == 0 {
            return (PtySlaveRead::WouldBlock, PtyNotifications::EMPTY);
        }

        let out: Vec<u8> = self.input_buf.drain(..available).collect();
        (PtySlaveRead::Data(out), self.notify_pollers())
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

        if !self.input_buf.is_empty() || self.closed_master || self.eof_pending {
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

    pub fn add_poller(&mut self, side: PtySide, entry: PollRef) -> PollKey {
        let key = self.next_poll_key;
        self.next_poll_key = self.next_poll_key.wrapping_add(1).max(1);
        self.pollers.push((key, side, entry));
        key
    }

    pub fn remove_poller(&mut self, key: PollKey) {
        self.pollers.retain(|(stored, _, _)| *stored != key);
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
                entries: Vec::new(),
                master_state,
                slave_state,
                input_wq,
                output_wq,
                signal_pgid: None,
            };
        }

        let entries: Vec<(PtySide, PollRef)> = self
            .pollers
            .iter()
            .map(|(_, side, entry)| (*side, entry.clone()))
            .collect();

        PtyNotifications {
            entries,
            master_state,
            slave_state,
            input_wq,
            output_wq,
            signal_pgid: None,
        }
    }
}

/// Deferred PTY notifications flushed after releasing the PTY lock.
pub struct PtyNotifications {
    entries: Vec<(PtySide, PollRef)>,
    master_state: PollState,
    slave_state: PollState,
    input_wq: Option<Arc<WaitQueue>>,
    output_wq: Option<Arc<WaitQueue>>,
    /// If set, signal this process group after releasing the PTY lock.
    pub signal_pgid: Option<(u64, u32)>,
}

impl PtyNotifications {
    const EMPTY: Self = Self {
        entries: Vec::new(),
        master_state: PollState::none(),
        slave_state: PollState::none(),
        input_wq: None,
        output_wq: None,
        signal_pgid: None,
    };

    /// Send all notifications. Call this after dropping the PTY lock.
    pub fn flush(self) {
        for (side, entry) in &self.entries {
            let state = match side {
                PtySide::Master => self.master_state,
                PtySide::Slave => self.slave_state,
            };
            entry.update(state);
        }
        if let Some(wq) = &self.input_wq {
            wq.wake_one();
        }
        if let Some(wq) = &self.output_wq {
            wq.wake_one();
        }
        if let Some((pgid, signum)) = self.signal_pgid {
            crate::thread::thread::signal_process_group(pgid, signum);
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
    fn register(&self, entry: PollRef) -> PollRegistration {
        let mut pty = self.inner.lock();
        let state = pty.poll_state_master();
        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = pty.add_poller(PtySide::Master, entry);
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
    fn register(&self, entry: PollRef) -> PollRegistration {
        let mut pty = self.inner.lock();
        let state = pty.poll_state_slave();
        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = pty.add_poller(PtySide::Slave, entry);
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
