/// Block Group Descriptor. One 64-byte entry per block group, stored
/// consecutively starting at block 2.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfsBlockGroupDesc {
    /// Absolute block number of this group's block allocation bitmap.
    pub block_bitmap_block: u64,
    /// Absolute block number of this group's inode allocation bitmap.
    pub inode_bitmap_block: u64,
    /// Absolute block number of the first block of this group's inode table.
    pub inode_table_block: u64,
    /// Number of free (unallocated) blocks in this group.
    pub free_blocks_count: u64,
    /// Number of free (unallocated) inodes in this group.
    pub free_inodes_count: u64,
    /// Number of directory inodes allocated in this group.
    pub used_dirs_count: u64,
    /// CRC32 of this 64-byte descriptor with this field zeroed during computation.
    pub checksum: u32,
    /// Reserved; must be zero.
    pub reserved: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<EfsBlockGroupDesc>() == 64);
