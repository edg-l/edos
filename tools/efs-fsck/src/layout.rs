use efs_common::{EfsBlockGroupDesc, EfsSuperblock};

/// Where inode `ino` lives: `(block, byte offset within it)`.
///
/// Returns `None` when `ino` is 0 or names a group the BGD table does not have,
/// which is the shape a wild inode number arrives in.
pub fn inode_location(
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
    ino: u64,
    block_size: usize,
) -> Option<(u64, usize)> {
    if ino == 0 {
        return None;
    }
    let inodes_per_group = sb.inodes_per_group as u64;
    let group = ((ino - 1) / inodes_per_group) as usize;
    let local_idx = (ino - 1) % inodes_per_group;
    let bgd = bgds.get(group)?;

    let byte_offset = local_idx as usize * sb.inode_size as usize;
    Some((
        bgd.inode_table_block + (byte_offset / block_size) as u64,
        byte_offset % block_size,
    ))
}

#[expect(
    dead_code,
    reason = "the whole superblock-derived layout, of which the checks read a part"
)]
/// Runtime layout derived from the on-disk superblock.
/// Unlike mkfs, fsck does NOT recompute layout; it trusts the superblock fields.
pub struct RuntimeLayout {
    pub block_size: u32,
    pub block_group_count: u16,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub total_blocks: u64,
    pub total_inodes: u64,
    /// Number of blocks the BGD table occupies (rounded up).
    pub bgd_table_blocks: u64,
}

impl RuntimeLayout {
    pub fn from_superblock(sb: &EfsSuperblock) -> Self {
        let block_size = sb.block_size();
        let bgd_entry_size = 64u64;
        let bgd_table_blocks =
            (sb.block_group_count as u64 * bgd_entry_size).div_ceil(block_size as u64);
        RuntimeLayout {
            block_size,
            block_group_count: sb.block_group_count,
            blocks_per_group: sb.blocks_per_group,
            inodes_per_group: sb.inodes_per_group,
            total_blocks: sb.total_blocks,
            total_inodes: sb.total_inodes,
            bgd_table_blocks,
        }
    }
}
