//! VFS inode cache: `(mount_id, ino) -> Weak<VfsInode>`.
//!
//! A `VfsInode` owns the file's page cache, so there must be exactly one live
//! `VfsInode` per on-disk inode. The dentry cache cannot provide that: it is
//! keyed by path and is invalidated by `truncate`, `rename`, `create_file` and
//! by its own LRU eviction. Building a fresh `VfsInode` on the next resolve
//! would fork the file into two objects with independent page caches, so a
//! write still sitting in the first one's cache is invisible to readers coming
//! through the second, and is written back over their newer data later.
//!
//! References are `Weak`, so an inode still dies when its last user drops it
//! and eviction semantics are unchanged. Dead entries are swept when the map
//! grows past `SWEEP_THRESHOLD`; nothing is removed on drop, which keeps the
//! cache out of the drop path entirely (`doc/invariants/drop-contract.md`).
//!
//! `ino == 0` is the "not a real inode" sentinel used by stateless
//! filesystems, where every path reports the same number. Those are never
//! cached, since one entry would alias every file on the mount.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use crate::{debug::lock_order::RANK_ICACHE, ranked_lock, thread::mutex::BlockingMutex};

use super::{FileKind, inode::VfsInode};

/// Sweep dead entries once the map grows past this many.
const SWEEP_THRESHOLD: usize = 512;

static INODE_CACHE: BlockingMutex<BTreeMap<(usize, u64), Weak<VfsInode>>> =
    BlockingMutex::new(BTreeMap::new());

/// Return the live `VfsInode` for `(mount_id, ino)`, creating and caching one
/// if there is none.
pub fn get_or_insert(mount_id: usize, ino: u64, kind: FileKind) -> Arc<VfsInode> {
    if ino == 0 {
        return VfsInode::new(mount_id, ino, kind);
    }

    // Build the candidate outside the lock: allocating under it would hold the
    // cache while the heap lock is taken, and the loser of a race drops its
    // candidate, which runs `VfsInode::drop`.
    let fresh = VfsInode::new(mount_id, ino, kind);

    let existing = {
        let mut map = ranked_lock!(RANK_ICACHE, "INODE_CACHE", INODE_CACHE);
        match map.get(&(mount_id, ino)).and_then(Weak::upgrade) {
            Some(inode) => Some(inode),
            None => {
                if map.len() >= SWEEP_THRESHOLD {
                    map.retain(|_, weak| weak.strong_count() > 0);
                }
                map.insert((mount_id, ino), Arc::downgrade(&fresh));
                None
            }
        }
    };

    existing.unwrap_or(fresh)
}
