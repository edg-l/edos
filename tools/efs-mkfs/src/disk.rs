use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::layout::Layout;
use efs_common::EfsInode;

/// Write `data` to block `block_num`.  `data` must be exactly `block_size` bytes or
/// shorter; remaining bytes are zero-padded.
pub fn write_block(
    file: &mut File,
    layout: &Layout,
    block_num: u64,
    data: &[u8],
) -> io::Result<()> {
    let offset = layout.block_offset(block_num);
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)?;
    // Zero-pad to fill the block.
    let pad = layout.block_size as usize - data.len();
    if pad > 0 {
        let zeros = vec![0u8; pad];
        file.write_all(&zeros)?;
    }
    Ok(())
}

/// Read one full block from `block_num`.
pub fn read_block(file: &mut File, layout: &Layout, block_num: u64) -> io::Result<Vec<u8>> {
    let offset = layout.block_offset(block_num);
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; layout.block_size as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write a struct `T` at `offset_in_block` bytes within block `block_num`.
///
/// # Safety
/// `T` must be `repr(C)` with no uninitialized padding bytes.
pub fn write_struct_at<T>(
    file: &mut File,
    layout: &Layout,
    block_num: u64,
    offset_in_block: usize,
    val: &T,
) -> io::Result<()> {
    let offset = layout.block_offset(block_num) + offset_in_block as u64;
    file.seek(SeekFrom::Start(offset))?;
    let bytes = unsafe {
        std::slice::from_raw_parts(val as *const T as *const u8, std::mem::size_of::<T>())
    };
    file.write_all(bytes)?;
    Ok(())
}

/// Write an inode to its correct location in the inode table.
pub fn write_inode(
    file: &mut File,
    layout: &Layout,
    inode_num: u64,
    inode: &EfsInode,
) -> io::Result<()> {
    let (group, index) = layout.inode_location(inode_num);
    let inode_table_block = layout.groups[group].inode_table_block;
    let byte_offset_in_table = index * 256;
    let block_offset_in_table = byte_offset_in_table / layout.block_size as u64;
    let offset_in_block = (byte_offset_in_table % layout.block_size as u64) as usize;
    let block_num = inode_table_block + block_offset_in_table;
    write_struct_at(file, layout, block_num, offset_in_block, inode)
}

/// Bytes written per call when zeroing a run of blocks.
///
/// One seek and large writes, rather than a seek and a write per block. The
/// run is contiguous, so the device path turns each call into a single
/// command; a block at a time is one command per block, and issuing a command
/// costs far more than the sectors it carries. Formatting a 5 GB filesystem
/// zeroes about 13000 blocks (inode tables plus the journal), which is the
/// difference between a handful of commands and thirteen thousand.
const ZERO_CHUNK_BYTES: usize = 1 << 20;

/// Zero out `count` full blocks starting at `start_block`.
pub fn zero_blocks(
    file: &mut File,
    layout: &Layout,
    start_block: u64,
    count: u64,
) -> io::Result<()> {
    if count == 0 {
        return Ok(());
    }
    let block_size = layout.block_size as usize;
    let blocks_per_chunk = (ZERO_CHUNK_BYTES / block_size).max(1);
    let zeros = vec![0u8; blocks_per_chunk * block_size];

    file.seek(SeekFrom::Start(layout.block_offset(start_block)))?;
    let mut left = count as usize;
    while left > 0 {
        let n = left.min(blocks_per_chunk);
        file.write_all(&zeros[..n * block_size])?;
        left -= n;
    }
    Ok(())
}
