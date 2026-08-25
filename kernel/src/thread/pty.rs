use crate::{
    fs::{
        PollState,
        handle::{PollKey, PollRef, PollRegistration, Pollable},
    },
    thread::{mutex::BlockingMutex, signal, waitqueue::WaitQueue},
    util::ring::ByteRing,
};
use alloc::{sync::Arc, vec::Vec};

/// Outcome of a slave-side read.
///
/// `Eof` and `WouldBlock` are both zero bytes and mean opposite things: the
/// first is Ctrl-D, which POSIX reports as a zero-length read, and the second
/// means park until the master writes. Collapsing them is why Ctrl-D used to
/// hang until the master closed.
pub enum PtySlaveRead {
    /// This many bytes were written into the caller's buffer.
    Data(usize),
    Eof,
    WouldBlock,
}

/// How much program output a PTY will hold before the writing program has to
/// wait for the terminal to catch up.
///
/// Without a bound the ring simply grows, so a program that outruns the
/// terminal drawing for it buys its speed with kernel heap: `yes` against a
/// terminal that has stopped reading is the shape, and so is an `sshd` whose
/// client stopped reading. The master side here is a program rather than the
/// kernel, so a blocked slave writer means the session stalls until that
/// program reads, which is what backpressure on a terminal means.
///
/// Matches [`PIPE_CAPACITY`](crate::thread::pipe::PIPE_CAPACITY): the two
/// carry the same kind of traffic and there is no reason for a terminal to
/// buffer more of it than a pipe.
pub const PTY_OUTPUT_CAPACITY: usize = 64 * 1024;

/// Room a slave write needs before it can make progress at all. `ONLCR` turns
/// one newline into two stored bytes, so anything less than two cannot be
/// guaranteed to move a single byte.
const PTY_OUTPUT_MIN_WRITE: usize = 2;

/// How much unread keyboard input a PTY will hold, POSIX `MAX_INPUT`.
///
/// The input side is bounded by discarding rather than by waiting, because
/// there is nothing to push back on: the bytes come from a keyboard or from a
/// network peer, and a terminal that cannot take them drops them. Linux's
/// `N_TTY` uses the same figure for the same reason.
const PTY_INPUT_CAPACITY: usize = 4096;

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
    /// Output post-processing, POSIX `OPOST` with `ONLCR`: a newline on the way
    /// out becomes a carriage return and a newline.
    ///
    /// A terminal moves the cursor *down* on a line feed and leaves the column
    /// where it was; only the carriage return sends it back to the left. A
    /// program that writes lines separated by `\n` alone therefore draws a
    /// staircase, and every real terminal relies on the driver adding the
    /// carriage return. `edos_render`'s widget happens to treat a bare newline
    /// as both, which is why this was invisible until output first left the
    /// machine over SSH.
    ///
    /// Off in raw mode, where a program is drawing with escape sequences and
    /// emits its own line endings.
    opost: bool,
    line_buf: Vec<u8>,
}

impl LineDiscipline {
    fn new() -> Self {
        Self {
            canonical: true,
            echo: true,
            opost: true,
            line_buf: Vec::new(),
        }
    }

    /// Append as much of `data` to `out` as `room` bytes of storage allow,
    /// expanding newlines when `opost` is on. Returns how many bytes of `data`
    /// were consumed.
    ///
    /// The bound is on what is stored, not on what was accepted, because
    /// `ONLCR` makes those different numbers: a caller handing over one
    /// newline asks for two bytes of room.
    ///
    /// The scan costs a pass over the bytes only when there is a newline to
    /// find; output with none is pushed as it stands.
    fn write_output(&self, data: &[u8], out: &mut ByteRing, room: usize) -> usize {
        if !self.opost || !data.contains(&b'\n') {
            let take = data.len().min(room);
            out.push(&data[..take]);
            return take;
        }
        // Walk the input counting what each byte costs to store, and stop at
        // the last one that fits whole. Splitting a translated newline would
        // put a carriage return on the wire with its newline still to come.
        let mut cost = 0usize;
        let mut end = data.len();
        for (i, &byte) in data.iter().enumerate() {
            let byte_cost = if byte == b'\n' { 2 } else { 1 };
            if cost + byte_cost > room {
                end = i;
                break;
            }
            cost += byte_cost;
        }
        let mut start = 0;
        for (i, &byte) in data[..end].iter().enumerate() {
            if byte == b'\n' {
                out.push(&data[start..i]);
                out.push(b"\r\n");
                start = i + 1;
            }
        }
        out.push(&data[start..end]);
        end
    }

    /// Echo `bytes` back to the master, or drop them if the output ring is
    /// full.
    ///
    /// Echo is a courtesy copy of what the user typed, so queueing it past the
    /// capacity would be the same unbounded growth the capacity exists to
    /// prevent, and there is nobody to apply backpressure to: the master is
    /// the one that supplied the keystroke.
    fn echo(output_buf: &mut ByteRing, bytes: &[u8]) {
        if PTY_OUTPUT_CAPACITY.saturating_sub(output_buf.len()) >= bytes.len() {
            output_buf.push(bytes);
        }
    }

    fn process_input(
        &mut self,
        byte: u8,
        input_buf: &mut ByteRing,
        output_buf: &mut ByteRing,
    ) -> LineAction {
        // Ctrl+C and Ctrl+Z reach the foreground job in both raw and
        // canonical mode, and neither reaches the reader as data.
        if byte == 0x03 {
            self.line_buf.clear();
            if self.echo {
                Self::echo(output_buf, b"^C\n");
            }
            return LineAction::Interrupt;
        }

        if byte == 0x1a {
            self.line_buf.clear();
            if self.echo {
                Self::echo(output_buf, b"^Z\n");
            }
            return LineAction::Suspend;
        }

        // Overflow is discarded once the queue is full, as POSIX has it. The
        // characters that end a runaway program are exempt above and here, or
        // a queue filled by that program's own input would be unrecoverable.
        if byte != 0x04 && input_buf.len() + self.line_buf.len() >= PTY_INPUT_CAPACITY {
            return LineAction::None;
        }

        if !self.canonical {
            input_buf.push_byte(byte);
            if self.echo {
                Self::echo(output_buf, &[byte]);
            }
            return LineAction::None;
        }

        // Canonical mode
        match byte {
            // Enter / carriage return
            b'\r' | b'\n' => {
                self.line_buf.push(b'\n');
                input_buf.push(&self.line_buf);
                self.line_buf.clear();
                if self.echo {
                    if self.opost {
                        Self::echo(output_buf, b"\r\n");
                    } else {
                        Self::echo(output_buf, b"\n");
                    }
                }
                LineAction::None
            }
            // Backspace (DEL or BS)
            0x7F | 0x08 => {
                if !self.line_buf.is_empty() {
                    self.line_buf.pop();
                    if self.echo {
                        Self::echo(output_buf, b"\x08 \x08");
                    }
                }
                LineAction::None
            }
            // Ctrl+D (EOF)
            0x04 => {
                if self.line_buf.is_empty() {
                    LineAction::Eof
                } else {
                    input_buf.push(&self.line_buf);
                    self.line_buf.clear();
                    LineAction::None
                }
            }
            // Ctrl+C is handled above (before the canonical/raw check)
            // Tab
            b'\t' => {
                self.line_buf.push(byte);
                if self.echo {
                    Self::echo(output_buf, &[byte]);
                }
                LineAction::None
            }
            // Escape (start of escape sequences)
            0x1B => {
                self.line_buf.push(byte);
                if self.echo {
                    Self::echo(output_buf, &[byte]);
                }
                LineAction::None
            }
            // Printable bytes
            byte if byte >= 0x20 => {
                self.line_buf.push(byte);
                if self.echo {
                    Self::echo(output_buf, &[byte]);
                }
                LineAction::None
            }
            // Other control characters: drop silently
            _ => LineAction::None,
        }
    }
}

#[derive(Debug)]
pub struct Pty {
    /// Data written by master (keyboard input), consumed by slave readers.
    pub input_buf: ByteRing,
    /// Data written by slave (program output), consumed by master readers.
    pub output_buf: ByteRing,
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
    /// Wakes slave writers blocked on a full output ring, either because the
    /// master read some of it or because the master went away and the write
    /// can fail instead.
    slave_write_wq: Arc<WaitQueue>,
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

impl Pty {
    pub fn new() -> Self {
        Self {
            input_buf: ByteRing::new(),
            output_buf: ByteRing::new(),
            masters: 1,
            slaves: 1,
            closed_master: false,
            closed_slave: false,
            input_wq: Arc::new(WaitQueue::new()),
            output_wq: Arc::new(WaitQueue::new()),
            slave_write_wq: Arc::new(WaitQueue::new()),
            pollers: Vec::new(),
            next_poll_key: 1,
            line_disc: LineDiscipline::new(),
            eof_pending: false,
            foreground_pgid: None,
            cols: 80,
            rows: 30,
        }
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
                // A program in raw mode positions the cursor itself and writes
                // its own line endings, so translating them would insert
                // carriage returns it did not ask for.
                self.line_disc.opost = false;
                Ok(0)
            }
            PTY_IOCTL_SET_CANONICAL => {
                self.line_disc.canonical = true;
                self.line_disc.echo = true;
                self.line_disc.opost = true;
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

    /// Clone the slave-writer WaitQueue Arc so `sys_write` can wait outside the
    /// lock, as it does for a full pipe.
    pub fn slave_write_wq(&self) -> Arc<WaitQueue> {
        self.slave_write_wq.clone()
    }

    /// Room left in the output ring before a slave writer has to wait.
    pub fn output_space(&self) -> usize {
        PTY_OUTPUT_CAPACITY.saturating_sub(self.output_buf.len())
    }

    /// Whether a slave write can make progress right now.
    pub fn slave_write_ready(&self) -> bool {
        // Not room, but the write is about to fail, and a writer waiting for a
        // master that has gone would wait forever.
        self.closed_master || self.output_space() >= PTY_OUTPUT_MIN_WRITE
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
    /// Fills the caller's buffer and reports how many bytes it took, so the
    /// PTY never allocates on a read; the caller copies them to user space
    /// after releasing the pty lock.
    pub fn master_read(&mut self, out: &mut [u8]) -> (usize, PtyNotifications) {
        let taken = self.output_buf.pop(out);
        if taken == 0 {
            return (0, PtyNotifications::EMPTY);
        }
        (taken, self.notify_pollers())
    }

    /// Slave writes program output into output_buf (master reads this), or
    /// reports that the terminal is gone.
    ///
    /// Takes as much as there is room for, so a caller with more than the
    /// terminal can hold writes it across several calls, waiting between them.
    /// `None` means the last master closed: POSIX makes that `EIO` rather than
    /// a write that succeeds into a buffer nothing will ever drain.
    ///
    /// Reports what it consumed of `data`, not what went into the ring: the
    /// carriage returns are the driver's, and a writer told it had written
    /// more than it asked would loop trying to make up the difference.
    pub fn slave_write(&mut self, data: &[u8]) -> (Option<usize>, PtyNotifications) {
        if self.closed_master {
            return (None, PtyNotifications::EMPTY);
        }
        if data.is_empty() {
            return (Some(0), PtyNotifications::EMPTY);
        }

        let room = self.output_space();
        let taken = self
            .line_disc
            .write_output(data, &mut self.output_buf, room);
        if taken == 0 {
            // Nothing moved, so nothing to tell anyone: the caller waits on
            // `slave_write_wq` and a master read wakes it when it frees room.
            return (Some(0), PtyNotifications::EMPTY);
        }
        (Some(taken), self.notify_pollers())
    }

    /// Slave reads keyboard input from input_buf (master wrote this).
    ///
    /// Fills the caller's buffer, as [`master_read`](Self::master_read) does.
    pub fn slave_read(&mut self, out: &mut [u8]) -> (PtySlaveRead, PtyNotifications) {
        // Deliver EOF once when input is empty and eof_pending is set.
        if self.input_buf.is_empty() && self.eof_pending {
            self.eof_pending = false;
            return (PtySlaveRead::Eof, self.notify_pollers());
        }

        let taken = self.input_buf.pop(out);
        if taken == 0 {
            return (PtySlaveRead::WouldBlock, PtyNotifications::EMPTY);
        }
        (PtySlaveRead::Data(taken), self.notify_pollers())
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

        // Writable means a write would not block, which a full output ring is
        // not.
        if self.masters > 0 && !self.closed_master && self.output_space() >= PTY_OUTPUT_MIN_WRITE {
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
        // A slave writer waits for room or for the last master to leave; both
        // are visible here, and `wake_all` because room enough for one may be
        // room enough for several.
        let wake_slave_writer =
            (slave_state.writable || self.closed_master) && self.slave_write_wq.has_waiters();
        let slave_write_wq = if wake_slave_writer {
            Some(self.slave_write_wq.clone())
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
                slave_write_wq,
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
            slave_write_wq,
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
    slave_write_wq: Option<Arc<WaitQueue>>,
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
        slave_write_wq: None,
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
        if let Some(wq) = &self.slave_write_wq {
            wq.wake_all();
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
