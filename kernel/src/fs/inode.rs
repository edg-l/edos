//! VFS-level inode abstraction with per-inode locking.
//!
//! Each `VfsInode` represents a unique file/directory on a specific mount.
//! It carries a per-inode `RwLock<()>` that serializes access: multiple
//! concurrent readers OR one exclusive writer per inode. The lock protects
//! no data directly -- it enforces the access protocol at the VFS layer.
//!
//! Open `FsFile` descriptors hold `Arc<VfsInode>`, keeping the inode alive
//! as long as any fd references it. Eviction from the dentry cache only
//! removes the lookup path; the Arc refcount prevents deallocation.

use alloc::sync::Arc;

use crate::thread::rwlock::RwLock as BlockingRwLock;

use super::FileKind;

/// A VFS-level inode. Wraps a filesystem-local inode number with per-inode
/// synchronization and cached metadata.
pub struct VfsInode {
    /// Unique mount identifier (assigned by VFS on mount).
    pub mount_id: usize,
    /// Filesystem-local inode number (EFS ino, FAT32 start cluster, memfs node id).
    pub ino: u64,
    /// Cached file kind (file, directory, special).
    pub kind: FileKind,
    /// Per-inode read-write lock.
    /// Readers: concurrent read operations on this file.
    /// Writer: exclusive write/truncate/rename on this file.
    pub lock: BlockingRwLock<()>,
}

impl VfsInode {
    pub fn new(mount_id: usize, ino: u64, kind: FileKind) -> Arc<Self> {
        Arc::new(Self {
            mount_id,
            ino,
            kind,
            lock: BlockingRwLock::new(()),
        })
    }
}
