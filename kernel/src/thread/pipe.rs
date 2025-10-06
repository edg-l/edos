use crate::{
    fs::{PollState, handle::Pollable, path::Path},
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
}

#[allow(unused)]
impl Pipe {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            readers: 1,
            writers: 1,
            closed: false,
        }
    }

    pub fn close_writer(&mut self) {
        self.writers = self.writers.saturating_sub(1);
        if self.writers == 0 {
            self.closed = true;
        }
    }

    pub fn close_reader(&mut self) {
        self.readers = self.readers.saturating_sub(1);
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
    fn subscribe(&self) -> PollState {
        let inner = self.inner.lock();

        if inner.closed || inner.buffer.is_empty() {
            PollState::none()
        } else {
            PollState {
                readable: true,
                writable: true,
                error: false,
            }
        }
    }

    fn unsubscribe(&self) {}
}
