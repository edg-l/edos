//! VFS dentry (directory entry) cache.
//!
//! Maps (mount_id, path) -> Arc<VfsInode> for fast path-to-inode resolution
//! without re-walking the directory tree on every operation.
//!
//! Ownership model: the dentry cache holds Arc<VfsInode> references. Open
//! FsFile descriptors also hold Arc<VfsInode>. Evicting an entry from the
//! cache does NOT invalidate open file descriptors -- the Arc refcount
//! keeps the VfsInode alive until all references are dropped.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use crate::{debug::lock_order::RANK_DENTRY, ranked_lock, thread::mutex::BlockingMutex};

use super::{inode::VfsInode, path::Path};

/// Maximum number of cached dentry entries.
const DENTRY_CACHE_MAX: usize = 256;

struct DentryCacheInner {
    /// Maps (mount_id, path) -> inode. BTreeMap gives ordered iteration
    /// for LRU-style eviction (evict first key when full).
    entries: BTreeMap<(usize, Path), Arc<VfsInode>>,
    /// Insertion counter for LRU eviction. Each entry is tagged with its
    /// insertion order; we evict the oldest when the cache is full.
    order: BTreeMap<u64, (usize, Path)>,
    next_order: u64,
}

pub struct DentryCache {
    inner: BlockingMutex<DentryCacheInner>,
}

impl DentryCache {
    pub const fn new() -> Self {
        Self {
            inner: BlockingMutex::new(DentryCacheInner {
                entries: BTreeMap::new(),
                order: BTreeMap::new(),
                next_order: 0,
            }),
        }
    }

    /// Look up an inode by mount and path.
    pub fn lookup(&self, mount_id: usize, path: &Path) -> Option<Arc<VfsInode>> {
        let cache = ranked_lock!(RANK_DENTRY, "dentry_cache", self.inner);
        cache.entries.get(&(mount_id, path.clone())).cloned()
    }

    /// Insert an inode into the cache. Evicts the oldest entry if full.
    pub fn insert(&self, mount_id: usize, path: Path, inode: Arc<VfsInode>) {
        let mut cache = ranked_lock!(RANK_DENTRY, "dentry_cache", self.inner);

        // Don't re-insert if already present.
        if cache.entries.contains_key(&(mount_id, path.clone())) {
            return;
        }

        // Evict oldest if at capacity.
        if cache.entries.len() >= DENTRY_CACHE_MAX
            && let Some((&oldest_ord, _)) = cache.order.iter().next()
            && let Some((mid, p)) = cache.order.remove(&oldest_ord)
        {
            cache.entries.remove(&(mid, p));
        }

        let ord = cache.next_order;
        cache.next_order += 1;
        cache.order.insert(ord, (mount_id, path.clone()));
        cache.entries.insert((mount_id, path), inode);
    }

    /// Invalidate a specific path.
    pub fn invalidate(&self, mount_id: usize, path: &Path) {
        let mut cache = ranked_lock!(RANK_DENTRY, "dentry_cache", self.inner);
        if cache.entries.remove(&(mount_id, path.clone())).is_some() {
            cache.order.retain(|_, v| v != &(mount_id, path.clone()));
        }
    }

    /// Invalidate all entries under a parent path (for directory removal).
    pub fn invalidate_children(&self, mount_id: usize, parent: &Path) {
        let mut cache = ranked_lock!(RANK_DENTRY, "dentry_cache", self.inner);
        let to_remove: Vec<(usize, Path)> = cache
            .entries
            .keys()
            .filter(|(mid, p)| *mid == mount_id && p.starts_with(parent))
            .cloned()
            .collect();
        for key in &to_remove {
            cache.entries.remove(key);
        }
        cache.order.retain(|_, v| !to_remove.contains(v));
    }
}

/// Global dentry cache instance.
static DENTRY_CACHE: DentryCache = DentryCache::new();

/// Public access to the global dentry cache.
pub fn dentry_cache() -> &'static DentryCache {
    &DENTRY_CACHE
}
