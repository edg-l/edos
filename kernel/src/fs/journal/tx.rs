// TxHandle: RAII transaction handle for the EFS journal.
//
// A TxHandle is opened at the start of each top-level metadata operation and
// dropped (merged into the active transaction) on success.  On error the
// caller must call `abort()` before dropping so the staged enrollments are
// discarded rather than merged.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use super::Journal;
use crate::fs::block_page_cache::CachedBlockPage;

/// RAII handle for a single logical transaction.
///
/// Callers enroll metadata pages and revoked blocks.  When dropped (without
/// `abort()`), all staged data is merged into `journal.state.active`.
pub struct TxHandle<'a> {
    journal: &'a Arc<Journal>,
    staged_blocks: BTreeMap<(u64, u64), Arc<CachedBlockPage>>,
    staged_revokes: BTreeSet<(u64, u64)>,
    aborted: bool,
}

impl<'a> TxHandle<'a> {
    pub(super) fn new(journal: &'a Arc<Journal>) -> Self {
        Self {
            journal,
            staged_blocks: BTreeMap::new(),
            staged_revokes: BTreeSet::new(),
            aborted: false,
        }
    }

    /// Enroll a metadata page in this transaction.
    ///
    /// If the same `(dev, block)` is enrolled multiple times within one
    /// handle, the last enrollment wins (same Arc, so it is idempotent).
    pub fn enroll_block(&mut self, dev: u64, block: u64, page: Arc<CachedBlockPage>) {
        self.staged_blocks.insert((dev, block), page);
    }

    /// Record that `(dev, block)` is being freed; any stale journal copy must
    /// not be replayed after this transaction commits.
    pub fn enroll_revoke(&mut self, dev: u64, block: u64) {
        self.staged_revokes.insert((dev, block));
    }

    /// Abort this transaction.  On drop, staged enrollments will be discarded
    /// rather than merged into the active transaction.
    pub fn abort(&mut self) {
        self.aborted = true;
    }
}

impl Drop for TxHandle<'_> {
    fn drop(&mut self) {
        if self.aborted {
            // Discard all staged data; nothing merges into the active tx.
            return;
        }

        if self.staged_blocks.is_empty() && self.staged_revokes.is_empty() {
            return;
        }

        // Merge staged enrollments into the active transaction.
        //
        // Lock ordering: acquire state first to merge blocks, then tracker.
        // We must read active.seq under the SAME state lock as the merge to
        // avoid TOCTOU (active could be sealed between two lock acquisitions).
        let staged_blocks = core::mem::take(&mut self.staged_blocks);
        let staged_revokes = core::mem::take(&mut self.staged_revokes);

        let active_seq = {
            let mut state = self.journal.state.lock();
            let active = &mut state.active;
            let seq = active.seq;
            for (key, page) in staged_blocks.iter() {
                active.enrolled_blocks.insert(*key, page.clone());
            }
            for &key in &staged_revokes {
                active.revokes.insert(key);
            }
            seq
        };

        // Update checkpoint_tracker outside the state lock (different lock).
        let mut tracker = self.journal.checkpoint_tracker.lock();
        for &key in staged_blocks.keys() {
            tracker.insert(key, active_seq);
        }
    }
}
