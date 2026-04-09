use alloc::collections::BTreeMap;

use crate::thread::pipe::{FileDescriptor, StandardStream};

#[allow(unused)]
#[derive(Debug, Clone)]
pub struct FileDescriptorTable {
    fds: BTreeMap<u64, FileDescriptor>,
}

#[allow(unused)]
impl FileDescriptorTable {
    pub fn new() -> Self {
        let mut table = Self {
            fds: BTreeMap::new(),
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

    /// Create an empty table with no pre-inserted file descriptors.
    pub fn new_empty() -> Self {
        Self {
            fds: BTreeMap::new(),
        }
    }

    /// Iterate over all entries as (fd_number, &FileDescriptor).
    pub fn iter_all(&self) -> impl Iterator<Item = (u64, &FileDescriptor)> {
        self.fds.iter().map(|(&k, v)| (k, v))
    }

    /// Allocate the lowest available fd number for the given descriptor.
    pub fn allocate_fd(&mut self, fd: FileDescriptor) -> u64 {
        let mut candidate = 0u64;
        while self.fds.contains_key(&candidate) {
            candidate += 1;
        }
        self.fds.insert(candidate, fd);
        candidate
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
}
