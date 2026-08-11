use crate::thread::preempt::PreemptSpinlock as Mutex;
use crate::{
    debug::lock_order::{RANK_PIPE, RANK_PTY},
    fs::{
        FileSystem, PollState,
        handle::{PollEntry, PollKey, PollRegistration, Pollable},
        inode::VfsInode,
        path::Path,
    },
    net::socket::Socket,
    ranked_lock,
    thread::{mutex::BlockingMutex, pty::Pty, waitqueue::WaitQueue},
};
use alloc::{sync::Arc, vec::Vec};

#[derive(Debug, Clone)]
pub enum FileDescriptor {
    StandardStream(StandardStream),
    #[allow(unused)]
    PipeRead(Arc<BlockingMutex<Pipe>>),
    #[allow(unused)]
    PipeWrite(Arc<BlockingMutex<Pipe>>),
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

#[allow(unused)]
#[derive(Debug)]
pub struct Pipe {
    pub buffer: Vec<u8>,
    pub readers: usize,
    pub writers: usize,
    pub closed: bool,
    pollers: Vec<(PollKey, Arc<PollEntry>)>,
    next_poll_key: PollKey,
    /// Wakes threads blocked in sys_read waiting for data or EOF.
    pub reader_wq: Arc<WaitQueue>,
}

#[allow(unused)]
impl Pipe {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            readers: 1,
            writers: 1,
            closed: false,
            pollers: Vec::new(),
            next_poll_key: 1,
            reader_wq: Arc::new(WaitQueue::new()),
        }
    }

    pub fn close_writer(&mut self) -> PipeNotifications {
        self.writers = self.writers.saturating_sub(1);
        if self.writers == 0 {
            self.closed = true;
        }
        self.notify_pollers()
    }

    pub fn close_reader(&mut self) -> PipeNotifications {
        self.readers = self.readers.saturating_sub(1);
        self.notify_pollers()
    }

    /// Append to the pipe, or report that nobody is left to read it.
    ///
    /// A pipe with no reader is not a slow pipe, it is a dead one: buffering
    /// into it grows the kernel heap for output no one will ever take. The
    /// caller turns `None` into EPIPE and a `SIGPIPE`, which is what makes
    /// `yes | head -1` terminate instead of running until memory runs out.
    pub fn write(&mut self, data: &[u8]) -> (Option<usize>, PipeNotifications) {
        if self.readers == 0 {
            return (None, self.notify_pollers());
        }
        self.buffer.extend_from_slice(data);
        let written = data.len();
        (Some(written), self.notify_pollers())
    }

    pub fn read(&mut self, count: usize) -> (Vec<u8>, PipeNotifications) {
        let available = count.min(self.buffer.len());
        let mut out = self.buffer[..available].to_vec();
        self.buffer.drain(..available);
        let notif = self.notify_pollers();
        if available == 0 {
            out.clear();
        }
        (out, notif)
    }

    fn poll_state(&self) -> PollState {
        let mut state = PollState::none();

        // A pipe whose last writer is gone is readable: the read returns end of
        // file at once rather than waiting. The PTY sides report the same way.
        if !self.buffer.is_empty() || self.closed {
            state.readable = true;
        }

        if self.readers > 0 && !self.closed {
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

    fn add_poller(&mut self, entry: Arc<PollEntry>) -> PollKey {
        let key = self.next_poll_key;
        self.next_poll_key = self.next_poll_key.wrapping_add(1).max(1);
        self.pollers.push((key, entry));
        key
    }

    fn remove_poller(&mut self, key: PollKey) {
        self.pollers.retain(|(stored, _)| *stored != key);
    }

    /// Snapshot poller entries + state. Callers must pass the result to
    /// `Pipe::flush_notifications` AFTER dropping the pipe lock to avoid
    /// holding BlockingMutex while wake_thread spins (priority inversion).
    fn notify_pollers(&mut self) -> PipeNotifications {
        let state = self.poll_state();
        let wake_reader = state.readable || state.hangup;
        let reader_wq = if wake_reader {
            Some(self.reader_wq.clone())
        } else {
            None
        };
        if self.pollers.is_empty() {
            return PipeNotifications {
                entries: heapless::Vec::new(),
                state,
                reader_wq,
            };
        }
        let mut entries: heapless::Vec<Arc<PollEntry>, 8> = heapless::Vec::new();
        for (_, entry) in self.pollers.iter() {
            let _ = entries.push(entry.clone());
        }
        PipeNotifications {
            entries,
            state,
            reader_wq,
        }
    }
}

/// Deferred poll notifications to be flushed after releasing the pipe lock.
pub struct PipeNotifications {
    entries: heapless::Vec<Arc<PollEntry>, 8>,
    state: PollState,
    reader_wq: Option<Arc<WaitQueue>>,
}

impl PipeNotifications {
    /// Send notifications. Call this after dropping the pipe lock.
    pub fn flush(self) {
        for entry in &self.entries {
            entry.update(self.state);
        }
        if let Some(wq) = &self.reader_wq {
            wq.wake_one();
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
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
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
