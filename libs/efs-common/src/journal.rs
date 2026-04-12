/// On-disk journal structures for the EFS journal.
///
/// All structs are `repr(C, packed)` to guarantee the exact byte layout.
/// Size assertions ensure the layout never silently drifts.

use core::mem::size_of;

use crate::crc32::{as_bytes, crc32};

// ---- Magic ----------------------------------------------------------------

/// Magic that identifies a valid `JournalSuperblock` at the journal's first block.
pub const JOURNAL_MAGIC: u32 = u32::from_le_bytes(*b"EJS!");

/// Magic embedded in every `JournalBlockHeader`.
pub const JOURNAL_BLOCK_MAGIC: u32 = u32::from_le_bytes(*b"EJB!");

// ---- JournalSuperblock ----------------------------------------------------

/// Superblock stored at the journal's first block (first 64 bytes of that block).
/// Remaining bytes of the block must be zero on disk.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct JournalSuperblock {
    /// `JOURNAL_MAGIC`
    pub magic: u32,
    /// Format version. Must be 1.
    pub version: u32,
    /// Total number of blocks in the journal region (including this superblock block).
    pub block_count: u32,
    /// Block size in bytes (must match the filesystem block size).
    pub block_size: u32,
    /// Sequence number of the oldest live transaction (tail).
    /// Transactions with `seq < tail_seq` have been checkpointed and the space
    /// can be reused.
    pub tail_seq: u64,
    /// Sequence number that will be assigned to the next new transaction (head).
    /// Invariant: `head_seq >= tail_seq`.
    pub head_seq: u64,
    /// CRC32 over the entire struct with this field zeroed.
    pub crc32: u32,
    /// Reserved; must be zero.
    pub reserved: [u8; 28],
}

const _: () = assert!(size_of::<JournalSuperblock>() == 64);

// ---- JournalBlockKind -------------------------------------------------------

/// Discriminant stored in `JournalBlockHeader::kind`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalBlockKind {
    /// Descriptor block: lists the filesystem blocks whose data follows.
    Descriptor = 1,
    /// Commit block: seals a transaction; carries a checksum of the data blocks.
    Commit = 2,
    /// Revoke block: records blocks whose stale journal copies must not be
    /// replayed.
    Revoke = 3,
}

// ---- JournalBlockHeader -----------------------------------------------------

/// 24-byte header placed at offset 0 of every journal metadata block
/// (descriptor, commit, revoke).
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct JournalBlockHeader {
    /// `JOURNAL_BLOCK_MAGIC`
    pub magic: u32,
    /// Block kind: one of the `JournalBlockKind` discriminants.
    pub kind: u8,
    /// Padding; must be zero.
    pub _pad: [u8; 3],
    /// Sequence number of the transaction this block belongs to.
    pub seq: u64,
    /// Transaction identifier (monotonically increasing, same as `seq` for now).
    pub tx_id: u64,
}

const _: () = assert!(size_of::<JournalBlockHeader>() == 24);

// ---- DescriptorEntry --------------------------------------------------------

/// One entry in a descriptor block, identifying a single filesystem block
/// whose data immediately follows in the journal log.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct DescriptorEntry {
    /// Absolute filesystem block number being journalled.
    pub fs_block: u64,
}

const _: () = assert!(size_of::<DescriptorEntry>() == 8);

// ---- RevokeEntry ------------------------------------------------------------

/// One entry in a revoke block: requests that replayers discard any journal
/// copy of `fs_block` with sequence number <= `seq`.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct RevokeEntry {
    /// Absolute filesystem block number being revoked.
    pub fs_block: u64,
    /// Highest sequence number whose journal copy must be discarded.
    pub seq: u64,
}

const _: () = assert!(size_of::<RevokeEntry>() == 16);

// ---- Checksum helpers -------------------------------------------------------

/// Compute the CRC32 checksum for a `JournalSuperblock`.
///
/// The `crc32` field in the copy is zeroed before hashing so the result is
/// stable regardless of what value the field currently holds.
pub fn journal_sb_checksum(sb: &JournalSuperblock) -> u32 {
    let mut copy = *sb;
    copy.crc32 = 0;
    // SAFETY: JournalSuperblock is repr(C, packed) with no padding.
    crc32(unsafe { as_bytes(&copy) })
}

/// Compute the CRC32 checksum over a commit block's payload.
///
/// The payload passed here should be the raw bytes of all data blocks written
/// for a transaction, concatenated in order.
pub fn commit_block_checksum(payload: &[u8]) -> u32 {
    crc32(payload)
}
