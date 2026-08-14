use crate::thread::preempt::PreemptSpinlock as Mutex;
use crate::{
    debug::lock_order::{RANK_PIPE, RANK_PTY},
    fs::{
        FileSystem, PollState,
        handle::{PollKey, PollRef, PollRegistration, Pollable},
        inode::VfsInode,
        path::Path,
    },
    net::socket::Socket,
    ranked_lock,
    thread::{mutex::BlockingMutex, pty::Pty, waitqueue::WaitQueue},
    util::ring::ByteRing,
};
use alloc::{sync::Arc, vec::Vec};

#[derive(Debug, Clone)]
pub enum FileDescriptor {
    StandardStream(StandardStream),
    #[allow(unused)]
    PipeRead(Arc<BlockingMutex<Pipe>>),
    #[allow(unused)]
    PipeWrite(Arc<BlockingMutex<Pipe>>),
    /// Both ends of one pipe on a single descriptor.
    ///
    /// Only a named pipe opened `O_RDWR` produces this. It is what lets a
    /// program hold a control channel open across writers coming and going:
    /// its own write end keeps the pipe from ever reaching end of file, so a
    /// reader waiting on it parks instead of spinning on a hangup nobody will
    /// clear.
    PipeReadWrite(Arc<BlockingMutex<Pipe>>),
    // Filesystem-backed file descriptor with maintained offset
    FsFile(FsFile),
    PtyMaster(Arc<BlockingMutex<Pty>>),
    PtySlave(Arc<BlockingMutex<Pty>>),
    Socket(Arc<Mutex<Socket>>),
}

impl FileDescriptor {
    /// Increment internal refcounts for types that track them (pipes, PTYs).
    /// Must be called whenever a FileDescriptor is duplicated (dup, dup2, spawn fd inheritance).
    pub fn inc_refcount(&self) {
        match self {
            FileDescriptor::PipeRead(pipe) => {
                ranked_lock!(RANK_PIPE, "fd::inc_refcount", pipe).readers += 1;
            }
            FileDescriptor::PipeWrite(pipe) => {
                ranked_lock!(RANK_PIPE, "fd::inc_refcount", pipe).writers += 1;
            }
            FileDescriptor::PipeReadWrite(pipe) => {
                let mut guard = ranked_lock!(RANK_PIPE, "fd::inc_refcount", pipe);
                guard.readers += 1;
                guard.writers += 1;
            }
            FileDescriptor::PtyMaster(pty) => {
                ranked_lock!(RANK_PTY, "fd::inc_refcount", pty).masters += 1;
            }
            FileDescriptor::PtySlave(pty) => {
                ranked_lock!(RANK_PTY, "fd::inc_refcount", pty).slaves += 1;
            }
            FileDescriptor::Socket(sock) => {
                sock.lock().refcount += 1;
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub enum StandardStream {
    Stdin,
    Stdout,
    Stderr,
}

/// How much a pipe will hold before a writer has to wait.
///
/// Without a bound the ring simply grows, so a writer that outruns its reader
/// buys its speed with kernel heap and the reader gets no signal to slow it
/// down. `ssh host 'cat 10MB'` is the shape: `cat` finished and exited having
/// pushed all ten megabytes into the kernel, long before the server had
/// forwarded a tenth of them.
///
/// 64 KiB is Linux's default. It is far above `PIPE_BUF`, so the atomicity
/// guarantee below never costs a wait in practice.
pub const PIPE_CAPACITY: usize = 64 * 1024;

/// The largest write POSIX requires to be atomic: a write of at most this many
/// bytes either goes in whole or waits, so two writers to one pipe never
/// interleave a single small message. Larger writes may be split.
pub const PIPE_BUF: usize = 4096;

#[allow(unused)]
#[derive(Debug)]
pub struct Pipe {
    pub buffer: ByteRing,
    pub readers: usize,
    pub writers: usize,
    pub closed: bool,
    pollers: Vec<(PollKey, PollRef)>,
    next_poll_key: PollKey,
    /// Wakes threads blocked in sys_read waiting for data or EOF.
    pub reader_wq: Arc<WaitQueue>,
    /// Wakes threads blocked in sys_write waiting for room, or for the last
    /// reader to go away so the write can fail instead.
    pub writer_wq: Arc<WaitQueue>,
}

#[allow(unused)]
impl Pipe {
    pub fn new() -> Self {
        Self {
            buffer: ByteRing::new(),
            readers: 1,
            writers: 1,
            closed: false,
            pollers: Vec::new(),
            next_poll_key: 1,
            reader_wq: Arc::new(WaitQueue::new()),
            writer_wq: Arc::new(WaitQueue::new()),
        }
    }

    /// A named pipe's buffer, which starts with neither end open.
    ///
    /// The difference from [`Pipe::new`] is the whole difference between the
    /// two kinds: an anonymous pipe is created by the call that hands out both
    /// descriptors, while a FIFO exists as a name first and gains its ends as
    /// programs open it.
    pub fn new_fifo() -> Self {
        Self {
            readers: 0,
            writers: 0,
            ..Self::new()
        }
    }

    /// Room left before a writer has to wait.
    pub fn space(&self) -> usize {
        PIPE_CAPACITY.saturating_sub(self.buffer.len())
    }

    /// Whether a write of `len` can proceed at all right now.
    ///
    /// A write up to `PIPE_BUF` waits for room for all of it, because POSIX
    /// requires it to be atomic. A larger one waits for `PIPE_BUF` of room and
    /// then takes what fits, so it goes in in chunks another writer's small
    /// write can land between — which is what POSIX permits above `PIPE_BUF`.
    pub fn write_ready(&self, len: usize) -> bool {
        if self.readers == 0 {
            return true; // not room, but the write is about to fail
        }
        let want = len.clamp(1, PIPE_BUF);
        self.space() >= want
    }

    pub fn close_writer(&mut self) -> PipeNotifications {
        self.close_writer_silent();
        self.notify_ends()
    }

    pub fn close_reader(&mut self) -> PipeNotifications {
        self.close_reader_silent();
        self.notify_ends()
    }

    /// Drop one writer without building notifications, for a caller closing
    /// both ends of one pipe that ends with a single [`Pipe::notify_ends`].
    pub fn close_writer_silent(&mut self) {
        self.writers = self.writers.saturating_sub(1);
        if self.writers == 0 {
            self.closed = true;
        }
    }

    /// Drop one reader without building notifications; see
    /// [`Pipe::close_writer_silent`].
    pub fn close_reader_silent(&mut self) {
        self.readers = self.readers.saturating_sub(1);
    }

    /// Append to the pipe, or report that nobody is left to read it.
    ///
    /// A pipe with no reader is not a slow pipe, it is a dead one: buffering
    /// into it grows the kernel heap for output no one will ever take. The
    /// caller turns `None` into EPIPE and a `SIGPIPE`, which is what makes
    /// `yes | head -1` terminate instead of running until memory runs out.
    /// Takes as much as there is room for, so a caller that has more than the
    /// pipe can hold writes it across several calls, waiting between them.
    pub fn write(&mut self, data: &[u8]) -> (Option<usize>, PipeNotifications) {
        if self.readers == 0 {
            return (None, self.notify_ends());
        }
        // The atomicity rule is enforced here rather than only in the caller's
        // wait predicate, or the first attempt at a write of at most `PIPE_BUF`
        // would push whatever fitted and wait for the rest, which is exactly
        // the interleaving with another writer that POSIX forbids.
        if !self.write_ready(data.len()) {
            // Nothing moved, so nothing to tell anyone: the caller waits on
            // `writer_wq` and a reader wakes it when it frees room.
            return (Some(0), PipeNotifications::EMPTY);
        }
        let take = data.len().min(self.space());
        self.buffer.push(&data[..take]);
        (Some(take), self.notify_ends())
    }

    /// Take up to `out.len()` bytes, reporting how many and what to notify.
    ///
    /// The caller owns the destination so the pipe never allocates on a read:
    /// the bytes go from the ring into a buffer the syscall already has, and
    /// from there to user space after the pipe lock is released.
    pub fn read_into(&mut self, out: &mut [u8]) -> (usize, PipeNotifications) {
        let taken = self.buffer.pop(out);
        // A read that moved nothing changed nothing, so there is nothing to
        // tell anyone. This is the common case on the blocking path: the read
        // that finds the pipe empty and parks would otherwise build a poll
        // state, clone the reader queue and wake it, all to report the state it
        // already had.
        if taken == 0 {
            return (0, PipeNotifications::EMPTY);
        }
        // A read is what frees room, so it is the only thing that can end a
        // writer's wait.
        (taken, self.notify_ends())
    }

    /// A read would return end of file: nothing buffered and no writer left.
    pub fn at_eof(&self) -> bool {
        self.closed && self.buffer.is_empty()
    }

    /// A read would return without blocking, either with bytes or with EOF.
    pub fn readable(&self) -> bool {
        !self.buffer.is_empty() || self.closed
    }

    fn poll_state(&self) -> PollState {
        let mut state = PollState::none();

        // A pipe whose last writer is gone is readable: the read returns end of
        // file at once rather than waiting. The PTY sides report the same way.
        if !self.buffer.is_empty() || self.closed {
            state.readable = true;
        }

        // Writable means a write would not block, which a full pipe is not.
        if self.readers > 0 && !self.closed && self.space() > 0 {
            state.writable = true;
        }

        if self.closed && self.buffer.is_empty() {
            state.hangup = true;
        }

        if self.readers == 0 && self.writers > 0 {
            state.error = true;
        }

        state
    }

    fn add_poller(&mut self, entry: PollRef) -> PollKey {
        let key = self.next_poll_key;
        self.next_poll_key = self.next_poll_key.wrapping_add(1).max(1);
        self.pollers.push((key, entry));
        key
    }

    fn remove_poller(&mut self, key: PollKey) {
        self.pollers.retain(|(stored, _)| *stored != key);
    }

    /// Snapshot what every end of the pipe should be told: poll entries, and
    /// the wait queues whose predicate the change may have satisfied.
    ///
    /// Callers must flush the result AFTER dropping the pipe lock, to avoid
    /// holding a BlockingMutex while wake_thread spins (priority inversion).
    pub fn notify_ends(&mut self) -> PipeNotifications {
        let state = self.poll_state();
        // `has_waiters` is read with the pipe lock held and the bytes already
        // in the ring, so a reader that enrols after this point re-checks its
        // predicate against data that is already there.
        let wake_reader = (state.readable || state.hangup) && self.reader_wq.has_waiters();
        let reader_wq = if wake_reader {
            Some(self.reader_wq.clone())
        } else {
            None
        };
        // A writer waits for room or for the last reader to leave; both are
        // visible here, and `wake_all` because room enough for one may be room
        // enough for several.
        let wake_writer = (state.writable || self.readers == 0) && self.writer_wq.has_waiters();
        let writer_wq = if wake_writer {
            Some(self.writer_wq.clone())
        } else {
            None
        };
        if self.pollers.is_empty() {
            return PipeNotifications {
                entries: Vec::new(),
                state,
                reader_wq,
                writer_wq,
            };
        }
        let entries: Vec<PollRef> = self
            .pollers
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect();
        PipeNotifications {
            entries,
            state,
            reader_wq,
            writer_wq,
        }
    }
}

/// Deferred poll notifications to be flushed after releasing the pipe lock.
pub struct PipeNotifications {
    entries: Vec<PollRef>,
    state: PollState,
    reader_wq: Option<Arc<WaitQueue>>,
    writer_wq: Option<Arc<WaitQueue>>,
}

impl PipeNotifications {
    /// Nothing to tell anyone. `Vec::new` does not allocate, so this costs
    /// nothing to build or drop.
    pub const EMPTY: Self = Self {
        entries: Vec::new(),
        state: PollState::none(),
        reader_wq: None,
        writer_wq: None,
    };

    /// Send notifications. Call this after dropping the pipe lock.
    pub fn flush(self) {
        for entry in &self.entries {
            entry.update(self.state);
        }
        if let Some(wq) = &self.reader_wq {
            wq.wake_one();
        }
        if let Some(wq) = &self.writer_wq {
            wq.wake_all();
        }
    }
}

/// Access mode recorded at open time from the O_RDONLY / O_WRONLY / O_RDWR flag bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

impl OpenMode {
    pub fn readable(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    pub fn writable(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite)
    }
}

#[derive(Clone)]
pub struct FsFile {
    pub path: Path,
    pub offset: u64,
    pub append: bool,
    /// Access mode (read / write / read-write) parsed from open flags.
    pub mode: OpenMode,
    /// Cached filesystem handle (from open-time mount resolution), stable for
    /// the fd's lifetime. Eliminates re-scanning the VFS mount registry on
    /// every read/write syscall.
    pub fs: Option<Arc<dyn FileSystem + Send + Sync>>,
    /// Cached mount-relative path (from open-time mount resolution).
    pub relative: Option<Path>,
    /// Cached mount id (from open-time mount resolution).
    pub mount_id: usize,
    /// Cached VFS inode for per-inode locking. None for virtual filesystems
    /// (procfs, devfs) that don't have meaningful inodes.
    pub inode: Option<Arc<VfsInode>>,
    /// Per-fd sequential readahead window state. Updated on every vfs::read call.
    pub ra: crate::fs::readahead::ReadaheadState,
}

impl core::fmt::Debug for FsFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FsFile")
            .field("path", &self.path)
            .field("offset", &self.offset)
            .field("append", &self.append)
            .field("mode", &self.mode)
            .field("inode", &self.inode.as_ref().map(|i| (i.mount_id, i.ino)))
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct PollablePipe {
    inner: Arc<BlockingMutex<Pipe>>,
}

impl PollablePipe {
    pub fn new(pipe: Arc<BlockingMutex<Pipe>>) -> Self {
        Self { inner: pipe }
    }
}

impl Pollable for PollablePipe {
    fn register(&self, entry: PollRef) -> PollRegistration {
        let mut inner = self.inner.lock();
        let state = inner.poll_state();
        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = inner.add_poller(entry);
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        let mut inner = self.inner.lock();
        inner.remove_poller(key);
    }
}

/// Shut a descriptor down: the side effects that must happen when the last
/// reference in a process goes away, as opposed to merely dropping the value.
///
/// Pipes need an explicit close so the peer sees EOF, PTYs need their side
/// closed and the foreground pid cleared, and sockets need their refcount
/// dropped, their port released and a FIN sent. Shared by process exit and by
/// `execve` closing its close-on-exec descriptors.
pub fn close_descriptor(descriptor: FileDescriptor, owner_pid: u64) {
    match descriptor {
        FileDescriptor::PipeRead(pipe) => {
            let notif = ranked_lock!(RANK_PIPE, "pipe::close_reader", pipe).close_reader();
            notif.flush();
        }
        FileDescriptor::PipeWrite(pipe) => {
            let notif = ranked_lock!(RANK_PIPE, "pipe::close_writer", pipe).close_writer();
            notif.flush();
        }
        FileDescriptor::PipeReadWrite(pipe) => {
            let notif = {
                let mut guard = ranked_lock!(RANK_PIPE, "pipe::close_both", pipe);
                guard.close_reader_silent();
                guard.close_writer_silent();
                guard.notify_ends()
            };
            notif.flush();
        }
        FileDescriptor::PtySlave(pty) => {
            // The terminal keeps no foreground group once the group that held
            // it lets go of its end.
            let owner_pgid =
                crate::thread::thread::process_group_of(owner_pid, owner_pid).unwrap_or(owner_pid);
            let mut guard = ranked_lock!(RANK_PTY, "pipe::close_slave", pty);
            if guard.foreground_pgid == Some(owner_pgid) {
                guard.foreground_pgid = None;
            }
            let notif = guard.close_slave();
            drop(guard);
            notif.flush();
        }
        FileDescriptor::PtyMaster(pty) => {
            let notif = ranked_lock!(RANK_PTY, "pipe::close_master", pty).close_master();
            notif.flush();
        }
        FileDescriptor::Socket(sock) => {
            let mut s = sock.lock();
            s.refcount = s.refcount.saturating_sub(1);
            if s.refcount > 0 {
                return; // Other fds still reference this socket
            }
            s.closed = true;
            s.rx_wq.wake_all();
            // Key read under the socket guard, entry released after it goes:
            // the receive path takes the port table before a socket.
            let bound = crate::net::socket::port_key(&s);
            let tcp_conn = s.tcp_conn.clone();
            drop(s);
            if let Some(key) = bound {
                crate::net::socket::unbind_port(&sock, key);
            }
            // For TCP sockets, send FIN to initiate graceful close
            if let Some(conn) = tcp_conn {
                let fin = conn.lock().build_fin();
                if let Some(fin_seg) = fin {
                    let remote_ip = conn.lock().remote_ip;
                    if let Some(stack_mutex) = crate::net::stack::NET_STACK.get() {
                        let mut stack = stack_mutex.lock();
                        let _ =
                            stack.send_ip(remote_ip, crate::net::ipv4::IpProtocol::Tcp, &fin_seg);
                    }
                }
            }
        }
        _ => {}
    }
}
