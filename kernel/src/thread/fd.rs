use alloc::collections::BTreeMap;

use crate::thread::pipe::{FileDescriptor, StandardStream};

#[allow(unused)]
#[derive(Debug, Clone)]
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

    pub fn replace_fd(&mut self, fd: u64, new_fd: FileDescriptor) {
        if let Some(entry) = self.fds.get_mut(&fd) {
            *entry = new_fd;
        }
    }

    pub fn insert_fd(&mut self, fd: u64, descriptor: FileDescriptor) {
        self.fds.insert(fd, descriptor);
    }

    /// Remove and return all file descriptors (for process exit cleanup).
    pub fn drain_all(&mut self) -> alloc::vec::Vec<(u64, FileDescriptor)> {
        let entries: alloc::vec::Vec<(u64, FileDescriptor)> =
            self.fds.iter().map(|(&k, v)| (k, v.clone())).collect();
        self.fds.clear();
        entries
    }

    /// Atomically find the lowest free fd and insert the descriptor.
    pub fn allocate_lowest_fd(&mut self, descriptor: FileDescriptor) -> u64 {
        let mut candidate = 0u64;
        while self.fds.contains_key(&candidate) {
            candidate += 1;
        }
        self.fds.insert(candidate, descriptor);
        candidate
    }
}
