use efs_common::{EfsBlockGroupDesc, EfsSuperblock, has_superblock_backup};

use crate::report::{Category, Finding, Report, Severity};

/// An in-memory bitmap rebuilt from filesystem metadata.
///
/// Bit layout matches on-disk EFS bitmaps: little-endian, bit 0 of byte 0 is
/// index 0, bit 1 of byte 0 is index 1, etc.
pub struct RebuiltBitmap {
    bits: Vec<u64>,
    /// Total number of tracked bits (may be less than `bits.len() * 64`).
    pub len: u64,
}

impl RebuiltBitmap {
    /// Create a new all-zero bitmap covering `len` indices.
    pub fn new(len: u64) -> Self {
        let words = (len as usize).div_ceil(64);
        RebuiltBitmap {
            bits: vec![0u64; words],
            len,
        }
    }

    /// Set bit `index` to 1.
    pub fn set(&mut self, index: u64) {
        if index >= self.len {
            return;
        }
        self.bits[(index / 64) as usize] |= 1u64 << (index % 64);
    }

    /// Return whether bit `index` is set.
    pub fn get(&self, index: u64) -> bool {
        if index >= self.len {
            return false;
        }
        self.bits[(index / 64) as usize] & (1u64 << (index % 64)) != 0
    }

    /// Count how many bits are set.
    #[expect(
        dead_code,
        reason = "the bitmap API is complete; the allocation checks compare bit by bit"
    )]
    pub fn count_ones(&self) -> u64 {
        self.bits.iter().map(|w| w.count_ones() as u64).sum()
    }

    /// Serialize to bytes in little-endian bit order (matching on-disk layout).
    ///
    /// The returned vec has `ceil(len / 8)` bytes; unused trailing bits in the
    /// last byte are 0.
    #[expect(
        dead_code,
        reason = "the bitmap API is complete; nothing serialises a bitmap back out until --repair does"
    )]
    pub fn to_bytes(&self) -> Vec<u8> {
        let byte_count = (self.len as usize).div_ceil(8);
        let mut out = Vec::with_capacity(byte_count);
        for i in 0..byte_count {
            let word = self.bits[i / 8];
            let byte_shift = (i % 8) * 8;
            out.push((word >> byte_shift) as u8);
        }
        out
    }
}

/// Mark all static metadata blocks as used in `block_bm`.
///
/// Static metadata includes: reserved block 0, primary superblock (block 1),
/// primary BGD table, per-group metadata (bitmap + inode table blocks),
/// backup superblock + BGD table blocks in groups that carry them, and the
/// journal region.
pub fn seed_static_metadata(
    block_bm: &mut RebuiltBitmap,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
) {
    // Copy packed fields to locals.
    let total_blocks = sb.total_blocks;
    let block_size = sb.block_size() as u64;
    let block_group_count = sb.block_group_count;
    let blocks_per_group = sb.blocks_per_group as u64;
    let inodes_per_group = sb.inodes_per_group as u64;
    let inode_size = sb.inode_size as u64;
    let journal_first_block = sb.journal_first_block;
    let journal_block_count = sb.journal_block_count as u64;

    // Block 0: reserved.
    block_bm.set(0);

    // Block 1: primary superblock.
    block_bm.set(1);

    // Primary BGD table: blocks 2..2+bgd_table_blocks.
    let bgd_entry_size = 64u64;
    let bgd_table_blocks = (block_group_count as u64 * bgd_entry_size).div_ceil(block_size);
    for b in 2..2 + bgd_table_blocks {
        block_bm.set(b);
    }

    // Per-group metadata.
    let inode_table_blocks_per_group = (inodes_per_group * inode_size).div_ceil(block_size);

    for bgd in bgds.iter() {
        // Block bitmap.
        block_bm.set(bgd.block_bitmap_block);
        // Inode bitmap.
        block_bm.set(bgd.inode_bitmap_block);
        // Inode table.
        for b in bgd.inode_table_block..bgd.inode_table_block + inode_table_blocks_per_group {
            if b < total_blocks {
                block_bm.set(b);
            }
        }
    }

    // Group 0 also stores an in-group backup of SB + BGD immediately after the
    // primary BGD table (mkfs layout: reserved, SB, BGD×N, backupSB, backupBGD×N,
    // block_bitmap, inode_bitmap, inode_table...).
    let group0_backup_sb = 2 + bgd_table_blocks;
    block_bm.set(group0_backup_sb);
    for b in group0_backup_sb + 1..group0_backup_sb + 1 + bgd_table_blocks {
        if b < total_blocks {
            block_bm.set(b);
        }
    }

    // Backup superblock + BGD table for groups >= 1 that carry a backup.
    for g in 1u16..block_group_count {
        if !has_superblock_backup(g) {
            continue;
        }
        let group_start = g as u64 * blocks_per_group;
        // Backup superblock at the start of the group.
        block_bm.set(group_start);
        // Backup BGD table immediately after backup SB.
        for b in group_start + 1..group_start + 1 + bgd_table_blocks {
            if b < total_blocks {
                block_bm.set(b);
            }
        }
    }

    // Journal region.
    for b in journal_first_block..journal_first_block + journal_block_count {
        if b < total_blocks {
            block_bm.set(b);
        }
    }
}

/// Seed the inode bitmap and block bitmap from scanned inode data.
///
/// For each inode in `infos`: mark its inode number in `inode_bm`.
/// For each extent: mark every covered block in `block_bm`.
/// Double-claimed blocks (already set when we attempt to set) are reported as
/// non-fixable errors.
pub fn apply_inode_info(
    block_bm: &mut RebuiltBitmap,
    inode_bm: &mut RebuiltBitmap,
    infos: &[InodeInfo],
    report: &mut Report,
) {
    // Track which inode last claimed each block so we can name both claimants.
    let mut block_owner: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();

    for info in infos {
        // Inode numbers are 1-based; bitmap is 0-based.
        inode_bm.set(info.ino - 1);

        for &(phys_start, length) in &info.extents {
            for b in phys_start..phys_start + length as u64 {
                if let Some(&prev_ino) = block_owner.get(&b) {
                    report.push(Finding {
                        severity: Severity::Error,
                        category: Category::BlockBitmap,
                        message: format!(
                            "block {b} double-claimed by inodes {prev_ino} and {}",
                            info.ino
                        ),
                        fixable: false,
                        context: None,
                    });
                    // Update owner so a third claimant pairs against the most
                    // recent one instead of silently going missing.
                    block_owner.insert(b, info.ino);
                } else {
                    block_owner.insert(b, info.ino);
                    block_bm.set(b);
                }
            }
        }
    }
}

/// Compare a rebuilt bitmap against the on-disk bitmap bytes for a specific range.
///
/// `range_start` is the first index this on-disk buffer covers.
/// `range_len` is the number of indices covered.
/// `on_disk` must be at least `ceil(range_len / 8)` bytes.
pub fn compare(
    rebuilt: &RebuiltBitmap,
    on_disk: &[u8],
    range_start: u64,
    range_len: u64,
    kind: Category,
    report: &mut Report,
) {
    for local_idx in 0..range_len {
        let global_idx = range_start + local_idx;
        let rebuilt_bit = rebuilt.get(global_idx);
        let byte_idx = (local_idx / 8) as usize;
        let bit_pos = local_idx % 8;
        let disk_bit = if byte_idx < on_disk.len() {
            on_disk[byte_idx] & (1 << bit_pos) != 0
        } else {
            false
        };

        match (rebuilt_bit, disk_bit) {
            (true, false) => {
                report.push(Finding {
                    severity: Severity::Error,
                    category: kind,
                    message: format!(
                        "missing bit in {} bitmap for index {}",
                        kind.as_str(),
                        global_idx
                    ),
                    fixable: false,
                    context: None,
                });
            }
            (false, true) => {
                report.push(Finding {
                    severity: Severity::Error,
                    category: kind,
                    message: format!("leaked {} bit at {}", kind.as_str(), global_idx),
                    fixable: true,
                    context: None,
                });
            }
            _ => {}
        }
    }
}

/// Scanned inode information used for bitmap rebuilding and directory-tree walks.
pub struct InodeInfo {
    /// Inode number (1-based).
    pub ino: u64,
    /// Raw mode field.
    pub mode: u16,
    /// Hard link count from the inode.
    pub link_count: u16,
    /// Whether this inode is a directory.
    pub is_dir: bool,
    /// Extent ranges: `(physical_start_block, length_in_blocks)`.
    pub extents: Vec<(u64, u16)>,
}
