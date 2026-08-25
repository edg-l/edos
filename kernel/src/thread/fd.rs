use alloc::collections::BTreeMap;

use crate::thread::pipe::{FileDescriptor, StandardStream};

/// A descriptor plus its flags.
///
/// POSIX splits these in two: `FD_CLOEXEC` belongs to the table entry, while
/// the status flags belong to the open file description and are therefore
/// shared by every `dup` of it. This kernel has no open-file-description object
/// to hang the second kind on — `FsFile` carries its offset inside the
/// descriptor, so a `dup`ed file already advances independently — so status
/// flags live here too and `dup` copies them rather than sharing them. Giving
/// the two ends of a `dup` divergent `O_NONBLOCK` is the one case that
/// behaves differently from POSIX, and it takes a deliberate `F_SETFL` on one
/// of them to reach.
#[derive(Debug, Clone)]
struct FdEntry {
    desc: FileDescriptor,
    /// Closed by `execve` instead of being carried into the new image.
    cloexec: bool,
    /// `O_NONBLOCK`: a read or write that would wait fails with `EAGAIN`.
    nonblock: bool,
}

#[derive(Debug, Clone)]
pub struct FileDescriptorTable {
    fds: BTreeMap<u64, FdEntry>,
}

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

    /// The descriptor and its `O_NONBLOCK` flag, in one walk of the table.
    ///
    /// Every read and write wants both, and they are the two hottest lookups in
    /// the kernel: asking for them separately searches the same `BTreeMap`
    /// twice per call, four times per pipe round trip. The clone is here rather
    /// than at the call site because every caller takes one — the guard is
    /// dropped before the descriptor is used, since the work it leads to can
    /// block.
    pub fn get_fd_nonblock(&self, fd: u64) -> (Option<FileDescriptor>, bool) {
        match self.fds.get(&fd) {
            Some(entry) => (Some(entry.desc.clone()), entry.nonblock),
            None => (None, false),
        }
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
                nonblock: false,
            },
        );
    }

    /// A fresh table holding every descriptor of this one, with the flags and
    /// with each descriptor's refcount bumped: what `fork` gives the child.
    ///
    /// A method rather than a loop at the call site because the flags are the
    /// part that is easy to leave behind — the entries carry `FD_CLOEXEC`,
    /// which `fork` preserves and only `execve` acts on, and `O_NONBLOCK`,
    /// which a child inheriting a descriptor must see behave as its parent's
    /// did.
    pub fn deep_clone(&self) -> Self {
        for entry in self.fds.values() {
            entry.desc.inc_refcount();
        }
        Self {
            fds: self.fds.clone(),
        }
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

    /// Whether `fd` was opened, or later set, `O_NONBLOCK`. Unknown descriptors
    /// report false, which is what a read or write on one does anyway before it
    /// fails with `EBADF`.
    pub fn is_nonblock(&self, fd: u64) -> bool {
        self.fds.get(&fd).is_some_and(|e| e.nonblock)
    }

    /// Set or clear `O_NONBLOCK`. Returns false if the descriptor is not open.
    pub fn set_nonblock(&mut self, fd: u64, nonblock: bool) -> bool {
        match self.fds.get_mut(&fd) {
            Some(entry) => {
                entry.nonblock = nonblock;
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
