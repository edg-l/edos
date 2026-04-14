//! FAT32 PageCacheOps implementation.
//!
//! File data I/O routes exclusively through `crate::drivers::ahci::direct`
//! (not through BlockPageCache) per the project invariant for file data paths.
//!
//! Phase 3 adds: lookup_inode_entry, cluster_at_index, ensure_chain_to,
//! fill_page, flush_page, update_size, pin_inode, unpin_inode, mmap override.
