//! CRC32 (ISO 3309) implementation with compile-time lookup table.
//!
//! Polynomial: 0xEDB88320 (reflected form).
//! Initial value: 0xFFFFFFFF. Final XOR: 0xFFFFFFFF.

use crate::{EfsBlockGroupDesc, EfsInode, EfsSuperblock};

const fn make_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0usize;
        while k < 8 {
            if c & 1 != 0 {
                c = 0xEDB88320 ^ (c >> 1);
            } else {
                c >>= 1;
            }
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

const CRC32_TABLE: [u32; 256] = make_crc32_table();

/// Compute CRC32 over a byte slice.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = CRC32_TABLE[index] ^ (crc >> 8);
    }
    !crc
}

/// Cast a `repr(C)` struct to its raw bytes.
///
/// # Safety
/// The type `T` must be `repr(C)` with no padding bytes that could be
/// uninitialized. All structs in this crate satisfy this requirement.
pub unsafe fn as_bytes<T>(val: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// Compute the checksum for a superblock.
///
/// Zeros the `checksum` field before computing so the result is stable.
pub fn checksum_superblock(sb: &EfsSuperblock) -> u32 {
    let mut copy = *sb;
    copy.checksum = 0;
    // SAFETY: EfsSuperblock is repr(C) with all fields explicitly sized; no padding.
    crc32(unsafe { as_bytes(&copy) })
}

/// Compute the checksum for a block group descriptor.
pub fn checksum_block_group_desc(bgd: &EfsBlockGroupDesc) -> u32 {
    let mut copy = *bgd;
    copy.checksum = 0;
    // SAFETY: EfsBlockGroupDesc is repr(C) with all fields explicitly sized; no padding.
    crc32(unsafe { as_bytes(&copy) })
}

/// Compute the checksum for an inode.
pub fn checksum_inode(inode: &EfsInode) -> u32 {
    let mut copy = *inode;
    copy.checksum = 0;
    // SAFETY: EfsInode is repr(C) with all fields explicitly sized; no padding.
    crc32(unsafe { as_bytes(&copy) })
}
