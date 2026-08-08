use alloc::collections::BTreeMap;

use crate::thread::pipe::{FileDescriptor, StandardStream};

/// A descriptor plus the per-descriptor flags that belong to the table entry
/// rather than to the open file: today that is only close-on-exec.
#[derive(Debug, Clone)]
struct FdEntry {
    desc: FileDescriptor,
    /// Closed by `execve` instead of being carried into the new image.
    cloexec: bool,
}

#[allow(unused)]
#[derive(Debug, Clone)]
pub struct FileDescriptorTable {
    fds: BTreeMap<u64, FdEntry>,
}

#[allow(unused)]
impl FileDescriptorTable {
    pub fn new() -> Self {
        let mut table = Self {
            fds: BTreeMap::new(),
        };

        // Initialize standard streams
        table.insert_fd(0, FileDescriptor::StandardStream(StandardStream::Stdin));
        table.insert_fd(1, FileDescriptor::StandardStream(StandardStream::Stdout));
        table.insert_fd(2, FileDescriptor::StandardStream(StandardStream::Stderr));

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
        self.fds.iter().map(|(&k, v)| (k, &v.desc))
    }

    /// Allocate the lowest available fd number for the given descriptor.
    pub fn allocate_fd(&mut self, fd: FileDescriptor) -> u64 {
        self.allocate_fd_from(fd, 0)
    }

    /// Allocate the lowest available fd number that is at least `min`.
    ///
    /// The floor is what `fcntl(F_DUPFD)` needs; `allocate_fd` is this with a
    /// floor of zero.
    pub fn allocate_fd_from(&mut self, fd: FileDescriptor, min: u64) -> u64 {
        let mut candidate = min;
        while self.fds.contains_key(&candidate) {
            candidate += 1;
        }
        self.insert_fd(candidate, fd);
        candidate
    }

    pub fn get_fd(&self, fd: u64) -> Option<&FileDescriptor> {
        self.fds.get(&fd).map(|e| &e.desc)
    }

    pub fn close_fd(&mut self, fd: u64) -> Option<FileDescriptor> {
        self.fds.remove(&fd).map(|e| e.desc)
    }

    pub fn replace_fd(&mut self, fd: u64, new_fd: FileDescriptor) {
        if let Some(entry) = self.fds.get_mut(&fd) {
            entry.desc = new_fd;
        }
    }

    pub fn insert_fd(&mut self, fd: u64, descriptor: FileDescriptor) {
        // A descriptor placed at an explicit number starts inheritable, which
        // is what dup2 and the spawn redirections want.
        self.fds.insert(
            fd,
            FdEntry {
                desc: descriptor,
                cloexec: false,
            },
        );
    }

    /// Whether `fd` is marked close-on-exec. Unknown descriptors report false.
    pub fn is_cloexec(&self, fd: u64) -> bool {
        self.fds.get(&fd).is_some_and(|e| e.cloexec)
    }

    /// Set or clear close-on-exec. Returns false if the descriptor is not open.
    pub fn set_cloexec(&mut self, fd: u64, cloexec: bool) -> bool {
        match self.fds.get_mut(&fd) {
            Some(entry) => {
                entry.cloexec = cloexec;
                true
            }
            None => false,
        }
    }

    /// Remove and return every close-on-exec descriptor, for `execve` to close.
    ///
    /// The entries leave the table before the caller shuts them down, so a
    /// descriptor cannot be observed half-closed by the new image.
    pub fn take_cloexec(&mut self) -> alloc::vec::Vec<(u64, FileDescriptor)> {
        let doomed: alloc::vec::Vec<u64> = self
            .fds
            .iter()
            .filter(|(_, e)| e.cloexec)
            .map(|(&k, _)| k)
            .collect();
        doomed
            .into_iter()
            .filter_map(|fd| self.fds.remove(&fd).map(|e| (fd, e.desc)))
            .collect()
    }

    /// Remove and return all file descriptors (for process exit cleanup).
    pub fn drain_all(&mut self) -> alloc::vec::Vec<(u64, FileDescriptor)> {
        let entries: alloc::vec::Vec<(u64, FileDescriptor)> =
            self.fds.iter().map(|(&k, v)| (k, v.desc.clone())).collect();
        self.fds.clear();
        entries
    }
}
