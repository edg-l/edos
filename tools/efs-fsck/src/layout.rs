use efs_common::EfsSuperblock;

#[allow(dead_code)]
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

#[allow(dead_code)]
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
