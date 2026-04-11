//! VFS-level inode abstraction with per-inode locking and page cache.
//!
//! Each `VfsInode` represents a unique file/directory on a specific mount.
//! It carries a per-inode `RwLock<()>` that serializes access: multiple
//! concurrent readers OR one exclusive writer per inode.
//!
//! The `pages` field holds a per-inode page cache (Linux calls this the
//! address_space). Different files have independent page maps and locks,
//! so concurrent reads of different files have zero contention.

use alloc::sync::Arc;

use crate::thread::rwlock::RwLock as BlockingRwLock;

use super::{FileKind, page_cache::InodePages};

/// A VFS-level inode.
#[expect(unused)]
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
}

impl VfsInode {
    pub fn new(mount_id: usize, ino: u64, kind: FileKind) -> Arc<Self> {
        Arc::new(Self {
            mount_id,
            ino,
            kind,
            lock: BlockingRwLock::new(()),
            pages: InodePages::new(),
        })
    }
}
