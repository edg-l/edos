use efs_common::has_superblock_backup;

/// Describes the computed layout of one block group.
#[derive(Clone, Debug)]
pub struct GroupLayout {
    /// Index of this group.
    pub group: u16,
    /// Absolute block number of the first block in this group.
    pub group_start: u64,
    /// Whether this group carries a superblock + BGD table backup.
    pub has_backup: bool,
    /// Absolute block number of the block bitmap.
    pub block_bitmap_block: u64,
    /// Absolute block number of the inode bitmap.
    pub inode_bitmap_block: u64,
    /// Absolute block number of the first inode table block.
    pub inode_table_block: u64,
    /// Total blocks in this group (last group may have fewer).
    pub total_blocks_in_group: u64,
}

/// Full filesystem layout derived from partition size and block size.
#[derive(Clone, Debug)]
pub struct Layout {
    pub block_size: u32,
    pub block_size_log2: u32,
    /// Number of blocks that fit in the entire partition.
    pub total_blocks: u64,
    /// Blocks per group (= block_size * 8, so the bitmap fits in one block).
    pub blocks_per_group: u32,
    /// Inodes per group.
    pub inodes_per_group: u32,
    /// Number of inode table blocks per group.
    pub inode_table_blocks: u32,
    /// Number of block groups.
    pub block_group_count: u16,
    /// Number of blocks consumed by the BGD table (blocks 2..2+bgd_table_blocks).
    pub bgd_table_blocks: u64,
    /// Per-group layouts.
    pub groups: Vec<GroupLayout>,
    /// Byte offset of the partition within the image file.
    pub partition_offset: u64,
    /// Total inodes across all groups.
    pub total_inodes: u64,
}

impl Layout {
    /// Compute the layout for a partition of `partition_size` bytes.
    pub fn compute(partition_size: u64, block_size: u32, partition_offset: u64) -> Self {
        let block_size_log2 = block_size.trailing_zeros();
        let total_blocks = partition_size / block_size as u64;

        // One bitmap block covers block_size * 8 blocks.
        let blocks_per_group = block_size * 8;

        // Inodes per group: 1 inode per 8 blocks is a reasonable default similar to ext2.
        // Each inode is 256 bytes; inode_table_blocks = inodes_per_group * 256 / block_size.
        let inodes_per_group = blocks_per_group / 8;
        let inode_table_blocks = (inodes_per_group as u64 * 256).div_ceil(block_size as u64) as u32;

        let block_group_count = total_blocks.div_ceil(blocks_per_group as u64) as u16;

        // BGD table: ceil(block_group_count * 64 / block_size) blocks, starts at block 2.
        let bgd_table_blocks = ((block_group_count as u64 * 64).div_ceil(block_size as u64)).max(1);

        let mut groups = Vec::with_capacity(block_group_count as usize);
        let mut total_inodes = 0u64;

        for g in 0..block_group_count {
            let group_start = g as u64 * blocks_per_group as u64;
            let has_backup = has_superblock_backup(g);

            // Blocks occupied before the block bitmap within this group.
            let mut metadata_before_bitmap = 0u64;
            if has_backup {
                if g == 0 {
                    // Group 0: primary superblock at block 1, BGD at block 2.
                    // Group 0 starts at block 0.  Block 0 is reserved, block 1 is the
                    // primary superblock, blocks 2..2+bgd_table_blocks are the BGD table.
                    // The group also holds a backup right after BGD.
                    // Spec: group 0 backup is also stored after the BGD table.
                    // metadata_before_bitmap = primary SB block (1) + BGD (bgd_table_blocks)
                    // + group0 backup SB (1) + group0 backup BGD (bgd_table_blocks) + reserved block 0.
                    // reserved(1) + SB(1) + BGD(bgd_table_blocks) + backup_SB(1) + backup_BGD(bgd_table_blocks).
                    metadata_before_bitmap = 1 + 1 + bgd_table_blocks + 1 + bgd_table_blocks;
                } else {
                    // Non-zero qualifying group: backup SB (1) + backup BGD.
                    metadata_before_bitmap = 1 + bgd_table_blocks;
                }
            } else if g == 0 {
                // Shouldn't happen since group 0 always has backups, but be safe.
                metadata_before_bitmap = 1 + 1 + bgd_table_blocks;
            }

            let block_bitmap_block = group_start + metadata_before_bitmap;
            let inode_bitmap_block = block_bitmap_block + 1;
            let inode_table_block = inode_bitmap_block + 1;
            let first_data_block = inode_table_block + inode_table_blocks as u64;

            // How many blocks actually exist in this group?
            let total_blocks_in_group = if g as u64 == block_group_count as u64 - 1 {
                total_blocks - group_start
            } else {
                blocks_per_group as u64
            };

            let _ = first_data_block;
            total_inodes += inodes_per_group as u64;

            groups.push(GroupLayout {
                group: g,
                group_start,
                has_backup,
                block_bitmap_block,
                inode_bitmap_block,
                inode_table_block,
                total_blocks_in_group,
            });
        }

        Layout {
            block_size,
            block_size_log2,
            total_blocks,
            blocks_per_group,
            inodes_per_group,
            inode_table_blocks,
            block_group_count,
            bgd_table_blocks,
            groups,
            partition_offset,
            total_inodes,
        }
    }

    /// Returns the byte offset of block `n` within the image file.
    pub fn block_offset(&self, block_num: u64) -> u64 {
        self.partition_offset + block_num * self.block_size as u64
    }

    /// Group number and in-group index for an inode number (1-based).
    pub fn inode_location(&self, inode_num: u64) -> (usize, u64) {
        let idx = inode_num - 1;
        let group = (idx / self.inodes_per_group as u64) as usize;
        let index = idx % self.inodes_per_group as u64;
        (group, index)
    }
}
