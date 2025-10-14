use crate::{
    fs::{
        PollState,
        handle::{PollEntry, PollKey, PollRegistration, Pollable},
        path::Path,
    },
    thread::mutex::BlockingMutex,
};
use alloc::{sync::Arc, vec::Vec};

#[derive(Debug, Clone)]
pub enum FileDescriptor {
    StandardStream(StandardStream),
    #[allow(unused)]
    Pipe(Arc<BlockingMutex<Pipe>>),
    // Filesystem-backed file descriptor with maintained offset
    FsFile(FsFile),
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
        }
    }

    pub fn close_writer(&mut self) {
        self.writers = self.writers.saturating_sub(1);
        if self.writers == 0 {
            self.closed = true;
        }
        self.notify_pollers();
    }

    pub fn close_reader(&mut self) {
        self.readers = self.readers.saturating_sub(1);
        self.notify_pollers();
    }

    pub fn write(&mut self, data: &[u8]) -> usize {
        self.buffer.extend_from_slice(data);
        let written = data.len();
        self.notify_pollers();
        written
    }

    pub fn read(&mut self, count: usize) -> Vec<u8> {
        let available = count.min(self.buffer.len());
        let mut out = self.buffer[..available].to_vec();
        self.buffer.drain(..available);
        self.notify_pollers();
        if available == 0 {
            out.clear();
        }
        out
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

    fn notify_pollers(&mut self) {
        if self.pollers.is_empty() {
            return;
        }
        let state = self.poll_state();
        for (_, entry) in self.pollers.iter() {
            entry.update(state);
        }
    }
}

#[derive(Debug, Clone)]
pub struct FsFile {
    pub path: Path,
    pub offset: u64,
    pub append: bool,
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
