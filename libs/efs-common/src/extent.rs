use crate::inode::INODE_DATA_AREA_SIZE;

/// Maximum number of extents (or index entries) that fit inline in an inode's
/// `data_area` after the header.
pub const MAX_INLINE_EXTENTS: usize = (INODE_DATA_AREA_SIZE
    - core::mem::size_of::<EfsExtentHeader>())
    / core::mem::size_of::<EfsExtent>();

/// Extent tree node header. Appears at the start of `data_area` in inodes and
/// at the start of every extent tree node block.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfsExtentHeader {
    /// Magic: `0xEF10`. Validates this is an extent header.
    pub magic: u16,
    /// Number of valid extent or index entries following this header.
    pub entries: u16,
    /// Maximum number of entries that can fit after this header.
    pub max_entries: u16,
    /// Tree depth. `0` = leaf (entries are `EfsExtent`). `> 0` = internal
    /// (entries are `EfsExtentIndex`).
    pub depth: u16,
    /// Reserved; must be zero.
    pub reserved: u32,
}

const _: () = assert!(core::mem::size_of::<EfsExtentHeader>() == 12);

/// Leaf extent entry (present when `EfsExtentHeader::depth == 0`). Maps a
/// contiguous range of logical blocks to physical blocks.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfsExtent {
    /// Starting logical block number within the file.
    pub logical_block: u32,
    /// Number of blocks in this extent. Bit 15 is reserved and must be 0 in v1.
    /// Valid range: 1..=32767.
    pub length: u16,
    /// Bits 47:32 of the physical starting block number.
    pub start_hi: u16,
    /// Bits 31:0 of the physical starting block number.
    pub start_lo: u32,
}

const _: () = assert!(core::mem::size_of::<EfsExtent>() == 12);

impl EfsExtent {
    /// Reconstruct the 48-bit physical starting block number.
    pub fn physical_start(&self) -> u64 {
        (self.start_hi as u64) << 32 | self.start_lo as u64
    }
}

/// Internal (index) extent entry (present when `EfsExtentHeader::depth > 0`).
/// Points to a child node block.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfsExtentIndex {
    /// Lowest logical block covered by the subtree rooted at the child block.
    pub logical_block: u32,
    /// Reserved; must be zero.
    pub reserved: u16,
    /// Bits 47:32 of the child block number.
    pub leaf_hi: u16,
    /// Bits 31:0 of the child block number.
    pub leaf_lo: u32,
}

const _: () = assert!(core::mem::size_of::<EfsExtentIndex>() == 12);

impl EfsExtentIndex {
    /// Reconstruct the 48-bit child block number.
    pub fn child_block(&self) -> u64 {
        (self.leaf_hi as u64) << 32 | self.leaf_lo as u64
    }
}
