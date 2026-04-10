use crate::{
    fs::{
        PollState,
        handle::{PollEntry, PollKey, PollRegistration, Pollable},
        inode::VfsInode,
        path::Path,
    },
    net::socket::Socket,
    thread::{mutex::BlockingMutex, pty::Pty, waitqueue::WaitQueue},
    util::uaccess::{try_copy_from_user, try_copy_to_user},
};
use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

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
                pipe.lock().readers += 1;
            }
            FileDescriptor::PipeWrite(pipe) => {
                pipe.lock().writers += 1;
            }
            FileDescriptor::PtyMaster(pty) => {
                pty.lock().masters += 1;
            }
            FileDescriptor::PtySlave(pty) => {
                pty.lock().slaves += 1;
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

    pub fn write(&mut self, data: &[u8]) -> (usize, PipeNotifications) {
        self.buffer.extend_from_slice(data);
        let written = data.len();
        (written, self.notify_pollers())
    }

    /// Write directly from user space into pipe buffer.
    /// Returns bytes written, or None on fault.
    pub fn write_from_user(
        &mut self,
        user_ptr: *const u8,
        len: usize,
    ) -> (Option<usize>, PipeNotifications) {
        if len == 0 {
            return (Some(0), PipeNotifications::EMPTY);
        }

        // Reserve space in buffer
        let start = self.buffer.len();
        self.buffer.resize(start + len, 0);

        // Copy directly from user space
        if !unsafe { try_copy_from_user(self.buffer[start..].as_mut_ptr(), user_ptr, len) } {
            // Rollback on fault
            self.buffer.truncate(start);
            return (None, PipeNotifications::EMPTY);
        }

        (Some(len), self.notify_pollers())
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

    /// Read directly from pipe buffer to user space.
    /// Returns bytes read, or None on fault.
    pub fn read_to_user(
        &mut self,
        user_ptr: *mut u8,
        count: usize,
    ) -> (Option<usize>, PipeNotifications) {
        if count == 0 {
            return (Some(0), PipeNotifications::EMPTY);
        }

        let available = count.min(self.buffer.len());
        if available == 0 {
            return (Some(0), PipeNotifications::EMPTY);
        }

        // Copy directly from pipe buffer to user space
        if !unsafe { try_copy_to_user(user_ptr, self.buffer.as_ptr(), available) } {
            return (None, PipeNotifications::EMPTY);
        }

        // Remove copied bytes from buffer
        self.buffer.drain(..available);
        (Some(available), self.notify_pollers())
    }

    fn poll_state(&self) -> PollState {
        let mut state = PollState::none();

        if !self.buffer.is_empty() {
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
    const EMPTY: Self = Self {
        entries: heapless::Vec::new(),
        state: PollState::none(),
        reader_wq: None,
    };

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

#[derive(Clone)]
pub struct FsFile {
    pub path: Path,
    pub offset: u64,
    pub append: bool,
    /// Cached VFS inode for per-inode locking. None for virtual filesystems
    /// (procfs, devfs) that don't have meaningful inodes.
    pub inode: Option<Arc<VfsInode>>,
}

impl core::fmt::Debug for FsFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FsFile")
            .field("path", &self.path)
            .field("offset", &self.offset)
            .field("append", &self.append)
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
