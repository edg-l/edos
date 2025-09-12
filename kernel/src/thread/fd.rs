use alloc::collections::BTreeMap;

use crate::thread::pipe::{FileDescriptor, StandardStream};

#[allow(unused)]
#[derive(Debug)]
pub struct FileDescriptorTable {
    fds: BTreeMap<u64, FileDescriptor>,
    next_fd: u64,
}

#[allow(unused)]
impl FileDescriptorTable {
    pub fn new() -> Self {
        let mut table = Self {
            fds: BTreeMap::new(),
            next_fd: 3, // Start after stdin/stdout/stderr
        };

        // Initialize standard streams
        table
            .fds
            .insert(0, FileDescriptor::StandardStream(StandardStream::Stdin));
        table
            .fds
            .insert(1, FileDescriptor::StandardStream(StandardStream::Stdout));
        table
            .fds
            .insert(2, FileDescriptor::StandardStream(StandardStream::Stderr));

        table
    }

    pub fn allocate_fd(&mut self, fd: FileDescriptor) -> u64 {
        let fd_num = self.next_fd;
        self.fds.insert(fd_num, fd);
        self.next_fd += 1;
        fd_num
    }

    pub fn get_fd(&self, fd: u64) -> Option<&FileDescriptor> {
        self.fds.get(&fd)
    }

    pub fn close_fd(&mut self, fd: u64) -> Option<FileDescriptor> {
        self.fds.remove(&fd)
    }
}
