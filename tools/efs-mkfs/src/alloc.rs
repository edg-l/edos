use std::fs::File;
use std::io;

use crate::disk::write_block;
use crate::layout::Layout;

pub struct Allocator {
    /// Block bitmaps for each group, each `block_size` bytes.
    block_bitmaps: Vec<Vec<u8>>,
    /// Inode bitmaps for each group, each `block_size` bytes.
    inode_bitmaps: Vec<Vec<u8>>,
    layout: Layout,
}

impl Allocator {
    pub fn new(layout: Layout) -> Self {
        let block_size = layout.block_size as usize;
        let group_count = layout.block_group_count as usize;
        let block_bitmaps = vec![vec![0u8; block_size]; group_count];
        let inode_bitmaps = vec![vec![0u8; block_size]; group_count];
        Allocator {
            block_bitmaps,
            inode_bitmaps,
            layout,
        }
    }

    // ---- block allocation ----

    /// Mark a specific absolute block number as used.
    pub fn mark_block_used(&mut self, block_num: u64) {
        let group = (block_num / self.layout.blocks_per_group as u64) as usize;
        let bit = (block_num % self.layout.blocks_per_group as u64) as usize;
        self.block_bitmaps[group][bit / 8] |= 1 << (bit % 8);
    }

    /// Allocate one free block, preferring `preferred_group`.
    /// Returns the absolute block number, or `None` if the filesystem is full.
    pub fn alloc_block(&mut self, preferred_group: usize) -> Option<u64> {
        let group_count = self.layout.block_group_count as usize;
        // Try preferred group first, then others.
        let order = std::iter::once(preferred_group)
            .chain((0..group_count).filter(|&g| g != preferred_group));
        for group in order {
            if let Some(bit) = self.find_free_block_bit(group) {
                self.block_bitmaps[group][bit / 8] |= 1 << (bit % 8);
                let abs = self.layout.groups[group].group_start + bit as u64;
                return Some(abs);
            }
        }
        None
    }

    /// Allocate `count` blocks, trying to keep them contiguous.
    /// Returns `(start_block, actual_count)` where `actual_count == count` on
    /// success.  Returns `None` if the filesystem is full.
    pub fn alloc_blocks(&mut self, count: u32, preferred_group: usize) -> Option<(u64, u32)> {
        let group_count = self.layout.block_group_count as usize;
        let order = std::iter::once(preferred_group)
            .chain((0..group_count).filter(|&g| g != preferred_group));

        for group in order {
            if let Some(start_bit) = self.find_free_run(group, count as usize) {
                // Mark them all used.
                for i in 0..count as usize {
                    let bit = start_bit + i;
                    self.block_bitmaps[group][bit / 8] |= 1 << (bit % 8);
                }
                let abs = self.layout.groups[group].group_start + start_bit as u64;
                return Some((abs, count));
            }
        }

        // Fall back: allocate a single block. Caller must retry for remaining blocks.
        let first = self.alloc_block(preferred_group)?;
        Some((first, 1))
    }

    fn find_free_run(&self, group: usize, count: usize) -> Option<usize> {
        let bitmap = &self.block_bitmaps[group];
        let max_bits = self.layout.groups[group].total_blocks_in_group as usize;
        let mut run_start = 0usize;
        let mut run_len = 0usize;
        for bit in 0..max_bits {
            if (bitmap[bit / 8] >> (bit % 8)) & 1 == 0 {
                if run_len == 0 {
                    run_start = bit;
                }
                run_len += 1;
                if run_len >= count {
                    return Some(run_start);
                }
            } else {
                run_len = 0;
            }
        }
        None
    }

    fn find_free_block_bit(&self, group: usize) -> Option<usize> {
        let bitmap = &self.block_bitmaps[group];
        let max_bits = self.layout.groups[group].total_blocks_in_group as usize;
        for bit in 0..max_bits {
            if (bitmap[bit / 8] >> (bit % 8)) & 1 == 0 {
                return Some(bit);
            }
        }
        None
    }

    // ---- inode allocation ----

    /// Mark a specific inode number (1-based) as used.
    pub fn mark_inode_used(&mut self, inode_num: u64) {
        let (group, index) = self.layout.inode_location(inode_num);
        let bit = index as usize;
        self.inode_bitmaps[group][bit / 8] |= 1 << (bit % 8);
    }

    /// Allocate one inode, preferring `preferred_group`.
    /// Returns the 1-based inode number, or `None` if all inodes are used.
    pub fn alloc_inode(&mut self, preferred_group: usize) -> Option<u64> {
        let group_count = self.layout.block_group_count as usize;
        let order = std::iter::once(preferred_group)
            .chain((0..group_count).filter(|&g| g != preferred_group));
        for group in order {
            if let Some(bit) = self.find_free_inode_bit(group) {
                self.inode_bitmaps[group][bit / 8] |= 1 << (bit % 8);
                let inode_num = group as u64 * self.layout.inodes_per_group as u64 + bit as u64 + 1;
                return Some(inode_num);
            }
        }
        None
    }

    fn find_free_inode_bit(&self, group: usize) -> Option<usize> {
        let bitmap = &self.inode_bitmaps[group];
        let max_bits = self.layout.inodes_per_group as usize;
        for bit in 0..max_bits {
            if (bitmap[bit / 8] >> (bit % 8)) & 1 == 0 {
                return Some(bit);
            }
        }
        None
    }

    // ---- free counts ----

    pub fn free_blocks_in_group(&self, group: usize) -> u64 {
        let bitmap = &self.block_bitmaps[group];
        let max_bits = self.layout.groups[group].total_blocks_in_group as usize;
        let mut free = 0u64;
        for bit in 0..max_bits {
            if (bitmap[bit / 8] >> (bit % 8)) & 1 == 0 {
                free += 1;
            }
        }
        free
    }

    pub fn free_inodes_in_group(&self, group: usize) -> u64 {
        let bitmap = &self.inode_bitmaps[group];
        let max_bits = self.layout.inodes_per_group as usize;
        let mut free = 0u64;
        for bit in 0..max_bits {
            if (bitmap[bit / 8] >> (bit % 8)) & 1 == 0 {
                free += 1;
            }
        }
        free
    }

    /// Write all block and inode bitmaps to disk.
    pub fn write_bitmaps(&self, file: &mut File) -> io::Result<()> {
        for g in &self.layout.groups {
            write_block(
                file,
                &self.layout,
                g.block_bitmap_block,
                &self.block_bitmaps[g.group as usize],
            )?;
            write_block(
                file,
                &self.layout,
                g.inode_bitmap_block,
                &self.inode_bitmaps[g.group as usize],
            )?;
        }
        Ok(())
    }
}
