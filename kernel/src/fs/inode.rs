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
//! # Reference counting = Linux `i_count`
//!
//! The `Arc<VfsInode>` refcount models Linux's `i_count`: every open
//! `FsFile`, every `FileBacked` VMA, and the dentry cache all hold an Arc.
//! When the last Arc drops, `Drop` fires and — if the inode was unlinked
//! while references were live (`orphan == true`) — calls
//! `FileSystem::evict_inode(ino)` to free on-disk blocks. This is the
//! equivalent of Linux's `iput_final` / `evict_inode`.
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
//! > Fault paths drop the mm mapper lock before touching the page cache.
//! > See doc/invariants/lock-order.md for the full rank table and enforcement.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use spin::RwLock;

use crate::thread::{UserThread, mutex::BlockingMutex, rwlock::RwLock as BlockingRwLock};

use super::{FileKind, page_cache::InodePages};

/// A VFS-level inode.
pub struct VfsInode {
    /// Unique mount identifier (assigned by VFS on mount).
    pub mount_id: usize,
    /// Filesystem-local inode number (EFS ino, FAT32 start cluster, memfs node id).
    pub ino: u64,
    /// Cached file kind (file, directory, special). Part of the identity the
    /// inode is created with; readers go through the filesystem's own lookup.
    #[allow(dead_code)]
    pub kind: FileKind,
    /// Per-inode read-write lock.
    pub lock: BlockingRwLock<()>,
    /// Per-inode page cache (file data pages).
    pub pages: InodePages,
    /// Reverse map: every UserThread that has a FileBacked VMA on this inode.
    /// Weak references so process exit tombstones entries automatically.
    /// Protected by a BlockingMutex; entries are deduplicated on insert.
    pub mappers: BlockingMutex<alloc::vec::Vec<Weak<RwLock<UserThread>>>>,
    /// Set by `vfs::remove_file` / `vfs::remove_dir` when the dentry is
    /// detached while Arc refs are still live. Triggers `evict_inode` on the
    /// final Arc drop. Equivalent to "`nlink == 0 && i_count > 0`" in Linux.
    pub orphan: AtomicBool,
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
            orphan: AtomicBool::new(false),
        })
    }

    pub fn mark_orphan(&self) {
        ORPHANS_MARKED.fetch_add(1, Ordering::Relaxed);
        self.orphan.store(true, Ordering::Release);
    }

    pub fn is_orphan(&self) -> bool {
        self.orphan.load(Ordering::Acquire)
    }
}

/// Inodes marked orphan by `unlink`, and orphans whose last reference was
/// released. Storage is only reclaimed on the second, so a gap between them is
/// inodes and blocks that no longer have a name and have not been freed.
pub static ORPHANS_MARKED: AtomicU64 = AtomicU64::new(0);
pub static ORPHANS_DROPPED: AtomicU64 = AtomicU64::new(0);

impl Drop for VfsInode {
    fn drop(&mut self) {
        // Linux `evict_inode` equivalent: if the dentry was detached while we
        // still had refs, now that the last ref is gone we ask the FS to free
        // its on-disk allocations (blocks, cluster chain, node content, etc).
        // ino == 0 is a sentinel for "not a real inode" (stateless FSes).
        if !self.is_orphan() || self.ino == 0 {
            return;
        }
        ORPHANS_DROPPED.fetch_add(1, Ordering::Relaxed);

        // Running here on the reaper is expected, not a violation: the reaper
        // frees a dead thread's descriptors and VMAs, so it routinely releases
        // the last reference to an orphaned inode. That is exactly why the
        // work is posted to a kthread — the contract is that this drop never
        // blocks, not that it never runs in those contexts. The one blocking
        // path, the queue-full fallback, is guarded inside `post_evict`.

        // Post to the evict kthread instead of calling evict_inode directly.
        // This keeps Drop non-blocking: evict_inode on EFS can issue AHCI I/O,
        // which would block the reaper (and any other thread whose VMA drop
        // triggers this). See doc/invariants/drop-contract.md and
        // doc/bugs/2026-04-16-drop-audit.md for the full rationale.
        super::evict::post_evict(self.mount_id, self.ino);
    }
}
