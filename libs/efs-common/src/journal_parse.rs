/// Pure parser helpers for journal ring blocks.
///
/// These are `no_std`-compatible and shared between the kernel replay path
/// and `efs-fsck`. They accept `block_size: usize` so the caller can supply
/// the correct value rather than relying on a compile-time constant.
extern crate alloc;

use alloc::vec::Vec;

use crate::{DescriptorEntry, JOURNAL_BLOCK_MAGIC, JournalBlockHeader, RevokeEntry};

/// Parse a `JournalBlockHeader` from the first bytes of `block`.
///
/// Returns `None` if the block is too short or the magic does not match.
pub fn parse_header(block: &[u8]) -> Option<JournalBlockHeader> {
    if block.len() < core::mem::size_of::<JournalBlockHeader>() {
        return None;
    }
    let hdr: JournalBlockHeader =
        unsafe { core::ptr::read_unaligned(block.as_ptr() as *const JournalBlockHeader) };
    if hdr.magic != JOURNAL_BLOCK_MAGIC {
        return None;
    }
    Some(hdr)
}

/// Parse all `DescriptorEntry` records from a descriptor block.
///
/// The entry count is stored as a `u32` immediately after the
/// `JournalBlockHeader`. Entries follow in sequence; parsing stops if an
/// entry would extend beyond `block_size`.
pub fn parse_descriptor_entries(block: &[u8], block_size: usize) -> Vec<DescriptorEntry> {
    let hdr_size = core::mem::size_of::<JournalBlockHeader>();
    if block.len() < hdr_size + 4 {
        return Vec::new();
    }
    let count = u32::from_le_bytes([
        block[hdr_size],
        block[hdr_size + 1],
        block[hdr_size + 2],
        block[hdr_size + 3],
    ]) as usize;
    let entries_offset = hdr_size + 4;
    let entry_size = core::mem::size_of::<DescriptorEntry>();
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let off = entries_offset + i * entry_size;
        if off + entry_size > block_size {
            break;
        }
        if off + entry_size > block.len() {
            break;
        }
        let entry: DescriptorEntry =
            unsafe { core::ptr::read_unaligned(block[off..].as_ptr() as *const DescriptorEntry) };
        entries.push(entry);
    }
    entries
}

/// Parse all `RevokeEntry` records from a revoke block.
///
/// Entries follow immediately after the `JournalBlockHeader` and continue
/// until a zero entry is found or `block_size` is reached.
pub fn parse_revoke_entries(block: &[u8], block_size: usize) -> Vec<RevokeEntry> {
    let hdr_size = core::mem::size_of::<JournalBlockHeader>();
    let entry_size = core::mem::size_of::<RevokeEntry>();
    let mut entries = Vec::new();
    let mut off = hdr_size;
    while off + entry_size <= block_size && off + entry_size <= block.len() {
        let entry: RevokeEntry =
            unsafe { core::ptr::read_unaligned(block[off..].as_ptr() as *const RevokeEntry) };
        if entry.fs_block == 0 && entry.seq == 0 {
            break;
        }
        entries.push(entry);
        off += entry_size;
    }
    entries
}
