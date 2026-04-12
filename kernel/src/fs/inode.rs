//! VFS-level inode abstraction with per-inode locking and page cache.
//!
//! Each `VfsInode` represents a unique file/directory on a specific mount.
//! It carries a per-inode `RwLock<()>` that serializes access: multiple
//! concurrent readers OR one exclusive writer per inode.
//!
//! The `pages` field holds a per-inode page cache (Linux calls this the
//! address_space). Different files have independent page maps and locks,
//! so concurrent reads of different files have zero contention.
//!
//! # Reverse mapping for truncate invalidation
//!
//! `mappers` is a list of `Weak` references to every `UserThread` that has a
//! live `FileBacked` VMA backed by this inode.  On `truncate`, the kernel
//! walks this list to unmap PTEs past the new EOF in every affected process.
//! Entries are `Weak` so that a process exit naturally tombstones the entry
//! without requiring an explicit remove-on-exit.  Tombstones are cleaned up
//! lazily during `truncate` invalidation.
//!
//! Lock ordering: inode.lock (write) > inode.pages.lock > inode.mappers.lock
//! > vmas.lock > memory_manager.lock.  Truncate acquires in this order.
//! Fault paths drop the mm mapper lock before touching the page cache.

use alloc::sync::{Arc, Weak};
use spin::RwLock;

use crate::thread::{UserThread, mutex::BlockingMutex, rwlock::RwLock as BlockingRwLock};

use super::{FileKind, page_cache::InodePages};

/// A VFS-level inode.
pub struct VfsInode {
    /// Unique mount identifier (assigned by VFS on mount).
    pub mount_id: usize,
    /// Filesystem-local inode number (EFS ino, FAT32 start cluster, memfs node id).
    pub ino: u64,
    /// Cached file kind (file, directory, special).
    pub kind: FileKind,
    /// Per-inode read-write lock.
    pub lock: BlockingRwLock<()>,
    /// Per-inode page cache (file data pages).
    pub pages: InodePages,
    /// Reverse map: every UserThread that has a FileBacked VMA on this inode.
    /// Weak references so process exit tombstones entries automatically.
    /// Protected by a BlockingMutex; entries are deduplicated on insert.
    pub mappers: BlockingMutex<alloc::vec::Vec<Weak<RwLock<UserThread>>>>,
}

impl VfsInode {
    pub fn new(mount_id: usize, ino: u64, kind: FileKind) -> Arc<Self> {
        Arc::new(Self {
            mount_id,
            ino,
            kind,
            lock: BlockingRwLock::new(()),
            pages: InodePages::new(),
            mappers: BlockingMutex::new(alloc::vec::Vec::new()),
        })
    }
}
