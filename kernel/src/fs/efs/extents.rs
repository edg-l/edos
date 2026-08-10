//! A file's block map, and how it is stored.
//!
//! Every non-inline inode carries an extent node in its 176-byte `data_area`:
//! a header plus up to [`MAX_INLINE_EXTENTS`] (13) twelve-byte entries. At
//! `depth == 0` those entries are the file's extents, which caps the file at
//! 13 discontiguous runs — a limit an ordinary 1 MiB write hits as soon as
//! free space is fragmented, and one that used to surface as
//! `Error::Unsupported` from `fsync` and as lost data from writeback.
//!
//! At `depth == 1` the inline entries are [`EfsExtentIndex`] entries instead,
//! each naming a leaf block that holds a header plus
//! `entries_per_node(block_size)` extents. At the 4 KiB block size that is 13
//! x 340 = 4420 extents, past anything reachable in practice.
//!
//! [`ExtentMap`] is the in-memory form both shapes load into, so the rest of
//! the driver works with a plain list of extents and never encodes a node.

use alloc::{vec, vec::Vec};

use efs_common::{
    EXTENT_MAGIC, EfsExtent, EfsExtentHeader, EfsExtentIndex, EfsInode, INODE_DATA_AREA_SIZE,
    MAX_INLINE_EXTENTS, entries_per_node, read_node_entry, read_node_header, write_node_entry,
    write_node_header,
};

use super::EfsDriver;
use crate::fs::Error;
use crate::fs::journal::tx::TxHandle;

/// A file's complete logical-to-physical block map.
///
/// Extents are kept sorted by `logical_block`, which is what lets a lookup
/// binary-search and what leaf splitting relies on to keep each index entry's
/// `logical_block` a true lower bound for its subtree.
#[derive(Clone, Default)]
pub(super) struct ExtentMap {
    extents: Vec<EfsExtent>,
}

/// The contiguous run a logical block starts, as reported by
/// [`ExtentMap::run_at`].
pub(super) enum BlockRun {
    /// `blocks` contiguous blocks starting at physical block `phys`.
    Mapped { phys: u64, blocks: u32 },
    /// Unallocated blocks, `blocks` of them before the next extent begins;
    /// `None` when no extent follows, so the hole runs to end of file.
    Hole { blocks: Option<u32> },
}

impl ExtentMap {
    pub fn as_slice(&self) -> &[EfsExtent] {
        &self.extents
    }

    pub fn len(&self) -> usize {
        self.extents.len()
    }

    /// Total data blocks mapped, which is what `EfsInode::blocks` records.
    /// Tree nodes are metadata and deliberately not counted here.
    pub fn block_count(&self) -> u64 {
        self.extents.iter().map(|e| e.length as u64).sum()
    }

    /// Physical block backing `logical_block`, if it is mapped.
    pub fn lookup(&self, logical_block: u32) -> Option<u64> {
        let i = match self
            .extents
            .binary_search_by_key(&logical_block, |e| e.logical_block)
        {
            Ok(i) => i,
            // `logical_block` falls after extent `i - 1`; that is the only
            // extent that can still cover it.
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let e = self.extents[i];
        if logical_block < e.logical_block + e.length as u32 {
            Some(e.physical_start() + (logical_block - e.logical_block) as u64)
        } else {
            None
        }
    }

    /// What backs the run of blocks starting at `logical_block`.
    ///
    /// A file may have holes: `truncate` can grow a file past its last extent,
    /// and the blocks in between are never allocated. They read as zeros.
    pub fn run_at(&self, logical_block: u32) -> BlockRun {
        let pos = self
            .extents
            .partition_point(|e| e.logical_block <= logical_block);

        if pos > 0 {
            let e = self.extents[pos - 1];
            let end = e.logical_block + e.length as u32;
            if logical_block < end {
                return BlockRun::Mapped {
                    phys: e.physical_start() + (logical_block - e.logical_block) as u64,
                    blocks: end - logical_block,
                };
            }
        }

        BlockRun::Hole {
            blocks: self
                .extents
                .get(pos)
                .map(|next| next.logical_block - logical_block),
        }
    }

    /// Record `logical_block -> phys_block`, extending an adjacent extent when
    /// the pair is contiguous in both spaces and inserting a new one
    /// otherwise. Insertion keeps the list sorted.
    pub fn insert(&mut self, logical_block: u32, phys_block: u64) {
        let pos = self
            .extents
            .partition_point(|e| e.logical_block <= logical_block);

        // Append to the extent immediately before this position.
        if pos > 0 {
            let prev = &mut self.extents[pos - 1];
            if prev.logical_block + prev.length as u32 == logical_block
                && prev.physical_start() + prev.length as u64 == phys_block
                && prev.length < u16::MAX >> 1
            {
                prev.length += 1;
                self.merge_forward(pos - 1);
                return;
            }
        }

        // Prepend to the extent immediately after it.
        if let Some(next) = self.extents.get_mut(pos) {
            if logical_block + 1 == next.logical_block
                && phys_block + 1 == next.physical_start()
                && next.length < u16::MAX >> 1
            {
                next.logical_block = logical_block;
                next.length += 1;
                next.start_hi = (phys_block >> 32) as u16;
                next.start_lo = phys_block as u32;
                if pos > 0 {
                    self.merge_forward(pos - 1);
                }
                return;
            }
        }

        self.extents.insert(
            pos,
            EfsExtent {
                logical_block,
                length: 1,
                start_hi: (phys_block >> 32) as u16,
                start_lo: phys_block as u32,
            },
        );
    }

    /// Fuse extent `i` with its successor when the two have become adjacent.
    /// Filling a one-block hole between two runs is what makes this reachable.
    fn merge_forward(&mut self, i: usize) {
        let Some(&next) = self.extents.get(i + 1) else {
            return;
        };
        let cur = self.extents[i];
        if cur.logical_block + cur.length as u32 == next.logical_block
            && cur.physical_start() + cur.length as u64 == next.physical_start()
            && cur.length as u32 + next.length as u32 <= (u16::MAX >> 1) as u32
        {
            self.extents[i].length += next.length;
            self.extents.remove(i + 1);
        }
    }

    /// Replace the map with `extents`, which must already be sorted.
    pub fn from_sorted(extents: Vec<EfsExtent>) -> Self {
        Self { extents }
    }
}

/// Largest map that can be stored at all, for the driver's block size.
///
/// Reaching it needs 4420 discontiguous runs in one file at 4 KiB blocks, so
/// in practice this is a corruption guard rather than a limit.
pub(super) fn max_extents(block_size: usize) -> usize {
    MAX_INLINE_EXTENTS * entries_per_node(block_size)
}

impl EfsDriver {
    /// Load an inode's complete extent list, following a depth-1 tree if there
    /// is one.
    pub(super) fn load_extent_map(&self, inode: &EfsInode) -> Result<ExtentMap, Error> {
        let hdr = read_node_header(&inode.data_area).ok_or(Error::Corrupted)?;
        if hdr.magic != EXTENT_MAGIC {
            return Err(Error::Corrupted);
        }
        match hdr.depth {
            0 => Ok(ExtentMap::from_sorted(read_leaf_entries(
                &inode.data_area,
                hdr.entries as usize,
                MAX_INLINE_EXTENTS,
            )?)),
            1 => {
                let count = (hdr.entries as usize).min(MAX_INLINE_EXTENTS);
                let block_size = self.block_size() as usize;
                let per_leaf = entries_per_node(block_size);
                let mut extents = Vec::with_capacity(count * per_leaf);
                for i in 0..count {
                    let idx: EfsExtentIndex =
                        read_node_entry(&inode.data_area, i).ok_or(Error::Corrupted)?;
                    let leaf = self.read_block(idx.child_block())?;
                    let leaf_hdr = read_node_header(&leaf).ok_or(Error::Corrupted)?;
                    if leaf_hdr.magic != EXTENT_MAGIC || leaf_hdr.depth != 0 {
                        return Err(Error::Corrupted);
                    }
                    extents.extend_from_slice(&read_leaf_entries(
                        &leaf,
                        leaf_hdr.entries as usize,
                        per_leaf,
                    )?);
                }
                Ok(ExtentMap::from_sorted(extents))
            }
            // Depth 2 would raise the ceiling again and nothing writes it, so
            // an image carrying one did not come from this driver.
            _ => Err(Error::Unsupported),
        }
    }

    /// Encode `map` into `inode.data_area`, allocating or freeing tree blocks
    /// as the shape requires, and refresh `inode.blocks`.
    ///
    /// The caller owns the inode write: most callers also update `size`,
    /// `mtime` and the checksum, and writing here would cost them a second
    /// inode write per operation.
    pub(super) fn store_extent_map(
        &self,
        inode: &mut EfsInode,
        map: &ExtentMap,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let block_size = self.block_size() as usize;
        let per_leaf = entries_per_node(block_size);
        // The ceiling depth 1 cannot pass. Depth 2 is what raises it further;
        // the format has the field for it and nothing writes one yet.
        if map.len() > max_extents(block_size) {
            return Err(Error::Unsupported);
        }

        // Tree blocks the inode holds today. Whatever the new shape does not
        // reuse is freed at the end, so no path leaks them.
        let existing = self.tree_blocks(inode)?;

        let needed_leaves = if map.len() <= MAX_INLINE_EXTENTS {
            0
        } else {
            map.len().div_ceil(per_leaf)
        };

        // Reuse the blocks already held rather than free-then-allocate: a
        // single-page `flush_page` on a heavily fragmented file stores the map
        // again, and churning the tree every time would allocate and free on every
        // page written.
        let mut leaves = Vec::with_capacity(needed_leaves);
        for i in 0..needed_leaves {
            match existing.get(i) {
                Some(&b) => leaves.push(b),
                None => leaves.push(self.alloc_block(tx)?),
            }
        }

        inode.data_area = [0u8; INODE_DATA_AREA_SIZE];
        if needed_leaves == 0 {
            write_node_header(
                &mut inode.data_area,
                &EfsExtentHeader {
                    magic: EXTENT_MAGIC,
                    entries: map.len() as u16,
                    max_entries: MAX_INLINE_EXTENTS as u16,
                    depth: 0,
                    reserved: 0,
                },
            );
            for (i, ext) in map.as_slice().iter().enumerate() {
                write_node_entry(&mut inode.data_area, i, ext);
            }
        } else {
            write_node_header(
                &mut inode.data_area,
                &EfsExtentHeader {
                    magic: EXTENT_MAGIC,
                    entries: needed_leaves as u16,
                    max_entries: MAX_INLINE_EXTENTS as u16,
                    depth: 1,
                    reserved: 0,
                },
            );
            for (i, &leaf_block) in leaves.iter().enumerate() {
                let chunk = &map.as_slice()[i * per_leaf..((i + 1) * per_leaf).min(map.len())];
                let mut node = vec![0u8; block_size];
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
                self.write_block(leaf_block, &node, tx)?;

                write_node_entry(
                    &mut inode.data_area,
                    i,
                    &EfsExtentIndex {
                        logical_block: chunk.first().map(|e| e.logical_block).unwrap_or(0),
                        reserved: 0,
                        leaf_hi: (leaf_block >> 32) as u16,
                        leaf_lo: leaf_block as u32,
                    },
                );
            }
        }

        for &surplus in existing.iter().skip(needed_leaves) {
            self.free_block(surplus, tx)?;
        }

        inode.blocks = map.block_count();
        Ok(())
    }

    /// Block numbers of the inode's extent-tree nodes, empty at depth 0.
    ///
    /// These are metadata, not file data: every caller that frees an inode's
    /// storage has to free them separately from the extents themselves.
    pub(super) fn tree_blocks(&self, inode: &EfsInode) -> Result<Vec<u64>, Error> {
        let Some(hdr) = read_node_header(&inode.data_area) else {
            return Ok(Vec::new());
        };
        if hdr.magic != EXTENT_MAGIC || hdr.depth == 0 {
            return Ok(Vec::new());
        }
        let count = (hdr.entries as usize).min(MAX_INLINE_EXTENTS);
        let mut blocks = Vec::with_capacity(count);
        for i in 0..count {
            let idx: EfsExtentIndex =
                read_node_entry(&inode.data_area, i).ok_or(Error::Corrupted)?;
            blocks.push(idx.child_block());
        }
        Ok(blocks)
    }

    /// Free every block an inode's storage occupies: its data extents and the
    /// tree nodes describing them.
    pub(super) fn free_extent_storage(
        &self,
        inode: &EfsInode,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        // An unrecognised node means there is nothing here to free, not that
        // the unlink should fail: a file created and never written has an
        // all-zero `data_area` and no extent header at all. Refusing to
        // proceed strands the inode *and* every block it does own, which is
        // strictly worse than freeing what we can identify.
        let map = self.load_extent_map(inode).unwrap_or_default();
        for ext in map.as_slice() {
            for i in 0..ext.length as u64 {
                self.free_block(ext.physical_start() + i, tx)?;
            }
        }
        for block in self.tree_blocks(inode).unwrap_or_default() {
            self.free_block(block, tx)?;
        }
        Ok(())
    }
}

/// Read `count` leaf entries out of a node, refusing a count the node cannot
/// hold rather than silently truncating it: a header claiming more entries
/// than fit is corruption, and reading the prefix would hand back a map with
/// blocks missing from it.
fn read_leaf_entries(node: &[u8], count: usize, capacity: usize) -> Result<Vec<EfsExtent>, Error> {
    if count > capacity {
        return Err(Error::Corrupted);
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(read_node_entry(node, i).ok_or(Error::Corrupted)?);
    }
    Ok(out)
}
