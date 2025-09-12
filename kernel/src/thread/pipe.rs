use alloc::{sync::Arc, vec::Vec};
use spin::RwLock;

#[derive(Debug, Clone)]
pub enum FileDescriptor {
    StandardStream(StandardStream),
    #[allow(unused)]
    Pipe(Arc<RwLock<Pipe>>),
    // Future: File(Arc<RwLock<File>>),
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

    #[expect(unused)]
    pub fn close_reader(&mut self) {
        self.readers = self.readers.saturating_sub(1);
    }
}
