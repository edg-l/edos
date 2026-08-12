//! Journal ring block writers: the counterpart to `journal_parse`.
//!
//! Layouts are specified in `doc/efs.md` §14. They live here rather than in the
//! kernel so that the writer and the two readers (`journal_scan`,
//! `journal_parse`) can only ever describe one on-disk format.

extern crate alloc;

use alloc::{vec, vec::Vec};

use crate::{
    DescriptorEntry, JOURNAL_BLOCK_MAGIC, JournalBlockHeader, JournalBlockKind, RevokeEntry,
};

/// Copy the bytes of `val` into `buf` at `offset`.
///
/// # Safety
/// `T` must be `repr(C)` (or `repr(C, packed)`) with no uninitialized padding.
fn write_struct<T>(buf: &mut [u8], offset: usize, val: &T) {
    let size = core::mem::size_of::<T>();
    let bytes = unsafe { core::slice::from_raw_parts(val as *const T as *const u8, size) };
    buf[offset..offset + size].copy_from_slice(bytes);
}

fn header_block(block_size: usize, kind: JournalBlockKind, seq: u64, tx_id: u64) -> Vec<u8> {
    let mut buf = vec![0u8; block_size];
    let hdr = JournalBlockHeader {
        magic: JOURNAL_BLOCK_MAGIC,
        kind: kind as u8,
        _pad: [0u8; 3],
        seq,
        tx_id,
    };
    write_struct(&mut buf, 0, &hdr);
    buf
}

/// Build a descriptor block listing the filesystem blocks whose journalled
/// copies follow it in the ring.
///
/// Layout: header, then the entry count as a `u32`, then the entries. The count
/// is stored so a reader does not have to rely on zero-termination. Entries
/// that would extend past `block_size` are dropped, so a caller must keep a
/// transaction within what one descriptor can name.
pub fn build_descriptor_block(
    block_size: usize,
    seq: u64,
    tx_id: u64,
    entries: &[DescriptorEntry],
) -> Vec<u8> {
    let mut buf = header_block(block_size, JournalBlockKind::Descriptor, seq, tx_id);
    let hdr_size = core::mem::size_of::<JournalBlockHeader>();
    buf[hdr_size..hdr_size + 4].copy_from_slice(&(entries.len() as u32).to_le_bytes());
    let entries_offset = hdr_size + 4;
    let entry_size = core::mem::size_of::<DescriptorEntry>();
    for (i, entry) in entries.iter().enumerate() {
        let off = entries_offset + i * entry_size;
        if off + entry_size > block_size {
            break;
        }
        write_struct(&mut buf, off, entry);
    }
    buf
}

/// Build a revoke block: the blocks whose stale journal copies must not be
/// replayed. Entries follow the header directly and are terminated by the
/// zeroed remainder of the block.
pub fn build_revoke_block(
    block_size: usize,
    seq: u64,
    tx_id: u64,
    entries: &[RevokeEntry],
) -> Vec<u8> {
    let mut buf = header_block(block_size, JournalBlockKind::Revoke, seq, tx_id);
    let hdr_size = core::mem::size_of::<JournalBlockHeader>();
    let entry_size = core::mem::size_of::<RevokeEntry>();
    for (i, entry) in entries.iter().enumerate() {
        let off = hdr_size + i * entry_size;
        if off + entry_size > block_size {
            break;
        }
        write_struct(&mut buf, off, entry);
    }
    buf
}

/// Build a commit block, which makes a transaction durable. `payload_crc` is
/// [`crate::commit_block_checksum`] over the transaction's data blocks
/// concatenated in order, and is stored directly after the header.
pub fn build_commit_block(block_size: usize, seq: u64, tx_id: u64, payload_crc: u32) -> Vec<u8> {
    let mut buf = header_block(block_size, JournalBlockKind::Commit, seq, tx_id);
    let off = core::mem::size_of::<JournalBlockHeader>();
    buf[off..off + 4].copy_from_slice(&payload_crc.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JournalBlockKind, parse_descriptor_entries, parse_header};

    const BLOCK: usize = 4096;

    #[test]
    fn descriptor_block_round_trips() {
        let entries = [
            DescriptorEntry {
                fs_block: 42,
                flags: 0,
                _reserved: 0,
            },
            DescriptorEntry {
                fs_block: 99,
                flags: crate::DESC_FLAG_ESCAPED,
                _reserved: 0,
            },
        ];
        let block = build_descriptor_block(BLOCK, 7, 7, &entries);
        assert_eq!(block.len(), BLOCK);

        // Packed fields are copied to locals: a reference to one is unaligned,
        // and the assertion macros take references.
        let hdr = parse_header(&block).expect("descriptor header should parse");
        let (kind, seq, tx_id) = (hdr.kind, hdr.seq, hdr.tx_id);
        assert_eq!(kind, JournalBlockKind::Descriptor as u8);
        assert_eq!(seq, 7);
        assert_eq!(tx_id, 7);

        let parsed = parse_descriptor_entries(&block, BLOCK);
        assert_eq!(parsed.len(), 2);
        let (first, second) = (parsed[0].fs_block, parsed[1].fs_block);
        let second_flags = parsed[1].flags;
        assert_eq!(first, 42);
        assert_eq!(second, 99);
        assert_eq!(second_flags, crate::DESC_FLAG_ESCAPED);
    }

    #[test]
    fn commit_block_carries_the_payload_crc() {
        let block = build_commit_block(BLOCK, 3, 3, 0xDEAD_BEEF);
        let hdr = parse_header(&block).expect("commit header should parse");
        let kind = hdr.kind;
        assert_eq!(kind, JournalBlockKind::Commit as u8);
        let off = core::mem::size_of::<JournalBlockHeader>();
        assert_eq!(
            u32::from_le_bytes(block[off..off + 4].try_into().unwrap()),
            0xDEAD_BEEF
        );
    }

    #[test]
    fn revoke_block_round_trips() {
        let entries = [RevokeEntry {
            fs_block: 1234,
            seq: 5,
        }];
        let block = build_revoke_block(BLOCK, 5, 5, &entries);
        let parsed = crate::parse_revoke_entries(&block, BLOCK);
        assert_eq!(parsed.len(), 1);
        let (fs_block, seq) = (parsed[0].fs_block, parsed[0].seq);
        assert_eq!(fs_block, 1234);
        assert_eq!(seq, 5);
    }
}
