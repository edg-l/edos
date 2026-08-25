extern crate alloc;

use alloc::{vec, vec::Vec};

use crate::{EXTENT_MAGIC, inode::INODE_DATA_AREA_SIZE};

/// Maximum number of extents (or index entries) that fit inline in an inode's
/// `data_area` after the header.
pub const MAX_INLINE_EXTENTS: usize = (INODE_DATA_AREA_SIZE
    - core::mem::size_of::<EfsExtentHeader>())
    / core::mem::size_of::<EfsExtent>();

/// Entries of either kind that fit after a header in a node of `bytes` bytes.
///
/// `EfsExtent` and `EfsExtentIndex` are both 12 bytes, so leaf and index nodes
/// hold the same count; the asserts below are what keeps that true.
pub const fn entries_per_node(bytes: usize) -> usize {
    (bytes - core::mem::size_of::<EfsExtentHeader>()) / core::mem::size_of::<EfsExtent>()
}

const _: () = assert!(
    core::mem::size_of::<EfsExtent>() == core::mem::size_of::<EfsExtentIndex>(),
    "entries_per_node assumes leaf and index entries are the same width"
);
const _: () = assert!(entries_per_node(INODE_DATA_AREA_SIZE) == MAX_INLINE_EXTENTS);

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

// ---- Node codec -------------------------------------------------------------
//
// Every extent node — the inode's `data_area` and every tree block — is a
// header followed by a packed array of 12-byte entries. Encoding lives here so
// the kernel driver, `efs-mkfs` and `efs-fsck` cannot disagree about it.

/// Read the node header at the start of `node`.
///
/// Returns `None` if `node` is too small to contain one.
pub fn read_node_header(node: &[u8]) -> Option<EfsExtentHeader> {
    if node.len() < core::mem::size_of::<EfsExtentHeader>() {
        return None;
    }
    // SAFETY: length checked above; `EfsExtentHeader` is `repr(C)` and plain
    // data, and the read is unaligned.
    Some(unsafe { core::ptr::read_unaligned(node.as_ptr() as *const EfsExtentHeader) })
}

/// Write `hdr` at the start of `node`. Panics if `node` is too small, which
/// would be a caller bug rather than a corrupt-image case.
pub fn write_node_header(node: &mut [u8], hdr: &EfsExtentHeader) {
    let size = core::mem::size_of::<EfsExtentHeader>();
    // SAFETY: `EfsExtentHeader` is `repr(C)` plain data with no padding to
    // leak; the slice length is checked by the copy below.
    let bytes =
        unsafe { core::slice::from_raw_parts(hdr as *const EfsExtentHeader as *const u8, size) };
    node[..size].copy_from_slice(bytes);
}

/// Read entry `index` from `node`, of whichever entry type the header's
/// `depth` implies. Returns `None` if the entry would run past the node.
pub fn read_node_entry<T: Copy>(node: &[u8], index: usize) -> Option<T> {
    let off = core::mem::size_of::<EfsExtentHeader>() + index * core::mem::size_of::<T>();
    if off + core::mem::size_of::<T>() > node.len() {
        return None;
    }
    // SAFETY: bounds checked above; entry types are `repr(C)` plain data and
    // the read is unaligned.
    Some(unsafe { core::ptr::read_unaligned(node[off..].as_ptr() as *const T) })
}

/// Write `entry` at position `index` in `node`. Panics if it would not fit.
pub fn write_node_entry<T: Copy>(node: &mut [u8], index: usize, entry: &T) {
    let size = core::mem::size_of::<T>();
    let off = core::mem::size_of::<EfsExtentHeader>() + index * size;
    // SAFETY: `T` is a `repr(C)` plain-data entry type; the copy below bounds-checks.
    let bytes = unsafe { core::slice::from_raw_parts(entry as *const T as *const u8, size) };
    node[off..off + size].copy_from_slice(bytes);
}

// ---- Tree builder -----------------------------------------------------------

/// Largest map that can be stored at all, for a given block size.
///
/// Reaching it needs 4420 discontiguous runs in one file at 4 KiB blocks, so in
/// practice this is a corruption guard rather than a limit. Depth 2 is what
/// would raise it; the format has the field for it and nothing writes one.
pub fn max_extents(block_size: usize) -> usize {
    MAX_INLINE_EXTENTS * entries_per_node(block_size)
}

/// Encode `runs` into `data_area`, spilling into leaf blocks when there are
/// more runs than fit inline, and return how many leaves the shape needs.
///
/// A flat inline list holds `MAX_INLINE_EXTENTS` extents. Past that the node
/// becomes depth 1: the inline entries are `EfsExtentIndex` entries, each
/// naming a leaf block holding up to `entries_per_node(block_size)` extents.
///
/// `emit_leaf(index, node)` is handed each finished leaf node in tree order and
/// answers with the block it now lives on, so the caller owns both allocation
/// and the write: the driver reuses the blocks the inode already holds, and
/// `efs-mkfs` allocates fresh ones near the file. `runs` past
/// `max_extents(block_size)` is a caller error and panics; both callers reject
/// it first with an error of their own.
pub fn build_extent_tree<E>(
    data_area: &mut [u8; INODE_DATA_AREA_SIZE],
    runs: &[EfsExtent],
    block_size: usize,
    mut emit_leaf: impl FnMut(usize, &[u8]) -> Result<u64, E>,
) -> Result<usize, E> {
    assert!(runs.len() <= max_extents(block_size));
    let per_leaf = entries_per_node(block_size);
    *data_area = [0u8; INODE_DATA_AREA_SIZE];

    if runs.len() <= MAX_INLINE_EXTENTS {
        write_node_header(
            data_area,
            &EfsExtentHeader {
                magic: EXTENT_MAGIC,
                entries: runs.len() as u16,
                max_entries: MAX_INLINE_EXTENTS as u16,
                depth: 0,
                reserved: 0,
            },
        );
        for (i, ext) in runs.iter().enumerate() {
            write_node_entry(data_area, i, ext);
        }
        return Ok(0);
    }

    let leaf_count = runs.len().div_ceil(per_leaf);
    write_node_header(
        data_area,
        &EfsExtentHeader {
            magic: EXTENT_MAGIC,
            entries: leaf_count as u16,
            max_entries: MAX_INLINE_EXTENTS as u16,
            depth: 1,
            reserved: 0,
        },
    );

    let mut node: Vec<u8> = vec![0u8; block_size];
    for (i, chunk) in runs.chunks(per_leaf).enumerate() {
        node.fill(0);
        write_node_header(
            &mut node,
            &EfsExtentHeader {
                magic: EXTENT_MAGIC,
                entries: chunk.len() as u16,
                max_entries: per_leaf as u16,
                depth: 0,
                reserved: 0,
            },
        );
        for (j, ext) in chunk.iter().enumerate() {
            write_node_entry(&mut node, j, ext);
        }
        let leaf_block = emit_leaf(i, &node)?;

        write_node_entry(
            data_area,
            i,
            &EfsExtentIndex {
                logical_block: chunk[0].logical_block,
                reserved: 0,
                leaf_hi: (leaf_block >> 32) as u16,
                leaf_lo: leaf_block as u32,
            },
        );
    }

    Ok(leaf_count)
}
