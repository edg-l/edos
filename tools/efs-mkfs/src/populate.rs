use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::time::SystemTime;

use efs_common::{
    EFS_ROOT_INO, EfsBlockGroupDesc, EfsInode, FT_DIR, FT_REG_FILE, FT_SYMLINK,
    INODE_DATA_AREA_SIZE, INODE_FLAG_INLINE_DATA, MAX_INLINE_EXTENTS, S_IFDIR, S_IFLNK, S_IFREG,
    checksum_inode,
};

use crate::alloc::Allocator;
use crate::disk::{read_block, write_block, write_inode};
use crate::layout::Layout;
use crate::mkfs::{add_inode_extent, init_extent_tree, now_timestamp, set_inode_extent};

/// A directory being built: its inode number, the data blocks allocated so far,
/// and the write cursor within those blocks.
struct DirState {
    blocks: Vec<u64>,
    cursor: usize, // byte offset within the logical directory data
}

impl DirState {
    fn new(_inode_num: u64, first_block: u64, initial_cursor: usize) -> Self {
        DirState {
            blocks: vec![first_block],
            cursor: initial_cursor,
        }
    }

    fn current_block_index(&self, block_size: usize) -> usize {
        self.cursor / block_size
    }

    fn offset_in_block(&self, block_size: usize) -> usize {
        self.cursor % block_size
    }
}

/// Compute mtime of a host path (seconds, nanoseconds).
fn host_mtime(path: &Path) -> (u64, u32) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if let Ok(mtime) = meta.modified() {
            if let Ok(d) = mtime.duration_since(SystemTime::UNIX_EPOCH) {
                return (d.as_secs(), d.subsec_nanos());
            }
        }
    }
    now_timestamp()
}

/// Add a directory entry to a directory, allocating new blocks as needed.
/// `dir_inode` is updated in-memory; the caller must write it back.
fn add_dir_entry(
    file: &mut File,
    layout: &Layout,
    allocator: &mut Allocator,
    state: &mut DirState,
    dir_inode: &mut EfsInode,
    child_ino: u64,
    name: &[u8],
    file_type: u8,
) -> io::Result<()> {
    let block_size = layout.block_size as usize;
    let name_len = name.len() as u8;
    let entry_min = efs_common::dir_entry_min_size(name_len) as usize;

    // Try to fit the entry into the current last block by splitting slack.
    let block_idx = state.current_block_index(block_size);
    let phys_block = state.blocks[block_idx];
    let mut block_buf = read_block(file, layout, phys_block)?;

    // Find the last entry in this block to check if there's slack we can split.
    let offset_in_blk = state.offset_in_block(block_size);
    if offset_in_blk > 0 && offset_in_blk + entry_min <= block_size {
        // There's room in this block (possibly via slack splitting).
        // Walk entries to find the last real one.
        let mut pos = 0usize;
        let mut last_entry_pos = 0usize;
        while pos < offset_in_blk {
            let rec_len = u16::from_le_bytes([block_buf[pos + 8], block_buf[pos + 9]]) as usize;
            if rec_len == 0 {
                break;
            }
            last_entry_pos = pos;
            pos += rec_len;
            if pos >= offset_in_blk {
                break;
            }
        }

        let last_rec_len =
            u16::from_le_bytes([block_buf[last_entry_pos + 8], block_buf[last_entry_pos + 9]])
                as usize;
        let last_name_len = block_buf[last_entry_pos + 10] as usize;
        let last_min = efs_common::dir_entry_min_size(last_name_len as u8) as usize;
        let slack = last_rec_len.saturating_sub(last_min);

        if slack >= entry_min {
            // Shrink last entry's rec_len to its minimum, write new entry in slack.
            let new_last_rec = last_min as u16;
            block_buf[last_entry_pos + 8..last_entry_pos + 10]
                .copy_from_slice(&new_last_rec.to_le_bytes());

            let new_entry_pos = last_entry_pos + last_min;
            let new_rec_len = (block_size - new_entry_pos) as u16;
            // Write new entry.
            block_buf[new_entry_pos..new_entry_pos + 8].copy_from_slice(&child_ino.to_le_bytes());
            block_buf[new_entry_pos + 8..new_entry_pos + 10]
                .copy_from_slice(&new_rec_len.to_le_bytes());
            block_buf[new_entry_pos + 10] = name_len;
            block_buf[new_entry_pos + 11] = file_type;
            block_buf[new_entry_pos + 12..new_entry_pos + 12 + name.len()].copy_from_slice(name);

            write_block(file, layout, phys_block, &block_buf)?;
            // cursor jumps to start of new entry + its minimum size (pointing past it).
            state.cursor = block_idx * block_size + new_entry_pos + entry_min;
            return Ok(());
        }
    }

    // Need a new block.
    let preferred_group = (phys_block / layout.blocks_per_group as u64) as usize;
    let new_phys = allocator.alloc_block(preferred_group).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            "filesystem full: no block for directory",
        )
    })?;
    state.blocks.push(new_phys);

    // Update directory inode with new extent entry.
    let logical_block = state.blocks.len() as u32 - 1;
    if logical_block == 1 {
        // Second block: inode already has extent for logical 0; add entry 1.
        add_inode_extent(dir_inode, 1, logical_block, new_phys, 1);
    } else {
        add_inode_extent(
            dir_inode,
            logical_block as usize,
            logical_block,
            new_phys,
            1,
        );
    }
    dir_inode.blocks += 1;
    dir_inode.size += block_size as u64;

    // Write entry at start of new block.
    let mut new_buf = vec![0u8; block_size];
    let rec_len = block_size as u16;
    new_buf[..8].copy_from_slice(&child_ino.to_le_bytes());
    new_buf[8..10].copy_from_slice(&rec_len.to_le_bytes());
    new_buf[10] = name_len;
    new_buf[11] = file_type;
    new_buf[12..12 + name.len()].copy_from_slice(name);
    write_block(file, layout, new_phys, &new_buf)?;

    state.cursor = logical_block as usize * block_size + entry_min;
    Ok(())
}

/// Create and write a regular file inode.  Returns the inode number.
fn create_file_inode(
    file: &mut File,
    layout: &Layout,
    allocator: &mut Allocator,
    content: &[u8],
    ts: (u64, u32),
    preferred_group: usize,
) -> io::Result<u64> {
    let inode_num = allocator
        .alloc_inode(preferred_group)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "filesystem full: no inodes"))?;

    let mut inode = EfsInode {
        mode: S_IFREG | 0o644,
        uid: 0,
        gid: 0,
        link_count: 1,
        size: content.len() as u64,
        blocks: 0,
        flags: 0,
        reserved1: 0,
        ctime_sec: ts.0,
        ctime_nsec: ts.1,
        reserved2: 0,
        mtime_sec: ts.0,
        mtime_nsec: ts.1,
        reserved3: 0,
        atime_sec: ts.0,
        atime_nsec: ts.1,
        checksum: 0,
        data_area: [0u8; INODE_DATA_AREA_SIZE],
    };

    if content.len() <= INODE_DATA_AREA_SIZE {
        // Inline data.
        inode.flags = INODE_FLAG_INLINE_DATA;
        inode.data_area[..content.len()].copy_from_slice(content);
    } else {
        // Extent mode: allocate blocks.
        let block_size = layout.block_size as usize;
        let num_blocks = content.len().div_ceil(block_size);

        init_extent_tree(&mut inode);

        // Try to allocate contiguous run.
        let mut written_blocks = 0usize;
        let mut logical = 0u32;
        while written_blocks < num_blocks {
            let remaining = num_blocks - written_blocks;
            let (phys_start, got) = allocator
                .alloc_blocks(remaining as u32, preferred_group)
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "filesystem full"))?;

            // Write data blocks.
            for i in 0..got as usize {
                let data_off = (written_blocks + i) * block_size;
                let data_end = (data_off + block_size).min(content.len());
                let chunk = &content[data_off..data_end];
                let mut block_buf = vec![0u8; block_size];
                block_buf[..chunk.len()].copy_from_slice(chunk);
                write_block(file, layout, phys_start + i as u64, &block_buf)?;
            }

            let entry_idx = if logical == 0 { 0 } else { logical as usize };
            if logical == 0 {
                set_inode_extent(&mut inode, logical, phys_start, got as u16);
            } else {
                if entry_idx >= MAX_INLINE_EXTENTS {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "file too large: exceeds inline extent limit",
                    ));
                }
                add_inode_extent(&mut inode, entry_idx, logical, phys_start, got as u16);
            }

            inode.blocks += got as u64;
            written_blocks += got as usize;
            logical += got;
        }
    }

    inode.checksum = checksum_inode(&inode);
    write_inode(file, layout, inode_num, &inode)?;
    Ok(inode_num)
}

/// Create a symlink inode with the given target.  Returns the inode number.
fn create_symlink_inode(
    file: &mut File,
    layout: &Layout,
    allocator: &mut Allocator,
    target: &[u8],
    ts: (u64, u32),
    preferred_group: usize,
) -> io::Result<u64> {
    let inode_num = allocator
        .alloc_inode(preferred_group)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "filesystem full: no inodes"))?;

    let mut inode = EfsInode {
        mode: S_IFLNK | 0o777,
        uid: 0,
        gid: 0,
        link_count: 1,
        size: target.len() as u64,
        blocks: 0,
        flags: 0,
        reserved1: 0,
        ctime_sec: ts.0,
        ctime_nsec: ts.1,
        reserved2: 0,
        mtime_sec: ts.0,
        mtime_nsec: ts.1,
        reserved3: 0,
        atime_sec: ts.0,
        atime_nsec: ts.1,
        checksum: 0,
        data_area: [0u8; INODE_DATA_AREA_SIZE],
    };

    if target.len() <= INODE_DATA_AREA_SIZE {
        inode.flags = INODE_FLAG_INLINE_DATA;
        inode.data_area[..target.len()].copy_from_slice(target);
    } else {
        let block_size = layout.block_size as usize;
        let num_blocks = target.len().div_ceil(block_size);
        init_extent_tree(&mut inode);

        let (phys_start, got) = allocator
            .alloc_blocks(num_blocks as u32, preferred_group)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "filesystem full"))?;

        for i in 0..got as usize {
            let off = i * block_size;
            let end = (off + block_size).min(target.len());
            let mut block_buf = vec![0u8; block_size];
            block_buf[..end - off].copy_from_slice(&target[off..end]);
            write_block(file, layout, phys_start + i as u64, &block_buf)?;
        }
        set_inode_extent(&mut inode, 0, phys_start, got as u16);
        inode.blocks = got as u64;
    }

    inode.checksum = checksum_inode(&inode);
    write_inode(file, layout, inode_num, &inode)?;
    Ok(inode_num)
}

/// Recursively populate the EFS image from `source_dir` into the directory
/// with inode `parent_ino`.
fn populate_dir(
    file: &mut File,
    layout: &Layout,
    allocator: &mut Allocator,
    source_dir: &Path,
    parent_ino: u64,
    dir_data_block: u64,
    initial_cursor: usize,
) -> io::Result<u32> {
    // Read the parent inode from disk.
    let (group, index) = layout.inode_location(parent_ino);
    let inode_table_block = layout.groups[group].inode_table_block;
    let byte_offset = index * 256;
    let block_in_table = byte_offset / layout.block_size as u64;
    let off_in_block = (byte_offset % layout.block_size as u64) as usize;
    let table_block_buf = read_block(file, layout, inode_table_block + block_in_table)?;
    let mut dir_inode: EfsInode = unsafe {
        std::ptr::read_unaligned(
            table_block_buf[off_in_block..off_in_block + 256].as_ptr() as *const EfsInode
        )
    };

    let mut dir_state = DirState::new(parent_ino, dir_data_block, initial_cursor);

    let mut child_dirs = 0u32; // count of subdirectories added (for link_count)

    let mut entries: Vec<_> = fs::read_dir(source_dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let entry_path = entry.path();
        let fname = entry.file_name();
        let fname_bytes = fname.as_encoded_bytes();

        if fname_bytes.len() > 255 {
            eprintln!(
                "warning: skipping '{}': filename too long",
                entry_path.display()
            );
            continue;
        }

        let meta = fs::symlink_metadata(&entry_path)?;
        let ts = host_mtime(&entry_path);

        let preferred_group = (dir_data_block / layout.blocks_per_group as u64) as usize;

        if meta.is_symlink() {
            let target = fs::read_link(&entry_path)?;
            let target_bytes = target.as_os_str().as_encoded_bytes();
            let child_ino =
                create_symlink_inode(file, layout, allocator, target_bytes, ts, preferred_group)?;
            add_dir_entry(
                file,
                layout,
                allocator,
                &mut dir_state,
                &mut dir_inode,
                child_ino,
                fname_bytes,
                FT_SYMLINK,
            )?;
        } else if meta.is_dir() {
            // Allocate inode and first data block for the subdirectory.
            let child_ino = allocator.alloc_inode(preferred_group).ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "filesystem full: no inodes")
            })?;

            let child_data_block = allocator.alloc_block(preferred_group).ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "filesystem full: no blocks")
            })?;

            // Write "." and ".." for child dir.
            let block_size = layout.block_size as usize;
            let mut child_buf = vec![0u8; block_size];
            let dot_min = efs_common::dir_entry_min_size(1) as usize;
            // "." -> child_ino
            child_buf[..8].copy_from_slice(&child_ino.to_le_bytes());
            child_buf[8..10].copy_from_slice(&(dot_min as u16).to_le_bytes());
            child_buf[10] = 1;
            child_buf[11] = FT_DIR;
            child_buf[12] = b'.';
            // ".." -> parent_ino
            let dotdot_rec_len = (block_size - dot_min) as u16;
            child_buf[dot_min..dot_min + 8].copy_from_slice(&parent_ino.to_le_bytes());
            child_buf[dot_min + 8..dot_min + 10].copy_from_slice(&dotdot_rec_len.to_le_bytes());
            child_buf[dot_min + 10] = 2;
            child_buf[dot_min + 11] = FT_DIR;
            child_buf[dot_min + 12..dot_min + 14].copy_from_slice(b"..");
            write_block(file, layout, child_data_block, &child_buf)?;

            // Build initial child dir inode.
            let dot_min_usize = efs_common::dir_entry_min_size(1) as usize;
            let dotdot_min_usize = efs_common::dir_entry_min_size(2) as usize;
            let initial_cursor_child = dot_min_usize + dotdot_min_usize;
            let child_inode_initial = {
                let mut ino = EfsInode {
                    mode: S_IFDIR | 0o755,
                    uid: 0,
                    gid: 0,
                    link_count: 2, // "." + parent entry
                    size: block_size as u64,
                    blocks: 1,
                    flags: 0,
                    reserved1: 0,
                    ctime_sec: ts.0,
                    ctime_nsec: ts.1,
                    reserved2: 0,
                    mtime_sec: ts.0,
                    mtime_nsec: ts.1,
                    reserved3: 0,
                    atime_sec: ts.0,
                    atime_nsec: ts.1,
                    checksum: 0,
                    data_area: [0u8; INODE_DATA_AREA_SIZE],
                };
                set_inode_extent(&mut ino, 0, child_data_block, 1);
                ino.checksum = checksum_inode(&ino);
                ino
            };
            write_inode(file, layout, child_ino, &child_inode_initial)?;

            // Mark child as directory in parent BGD.
            // (bgd used_dirs_count update happens in finalize via bgds passed back)

            // Recurse.
            let subdir_count = populate_dir(
                file,
                layout,
                allocator,
                &entry_path,
                child_ino,
                child_data_block,
                initial_cursor_child,
            )?;

            let _ = subdir_count;

            // Add entry in parent dir.
            add_dir_entry(
                file,
                layout,
                allocator,
                &mut dir_state,
                &mut dir_inode,
                child_ino,
                fname_bytes,
                FT_DIR,
            )?;

            child_dirs += 1;
        } else if meta.is_file() {
            let mut content = Vec::new();
            fs::File::open(&entry_path)?.read_to_end(&mut content)?;
            let child_ino =
                create_file_inode(file, layout, allocator, &content, ts, preferred_group)?;
            add_dir_entry(
                file,
                layout,
                allocator,
                &mut dir_state,
                &mut dir_inode,
                child_ino,
                fname_bytes,
                FT_REG_FILE,
            )?;
        } else {
            eprintln!(
                "warning: skipping '{}': unsupported file type",
                entry_path.display()
            );
        }
    }

    // Update parent dir inode: size may have grown, link_count gets +1 per subdir ".." back-ref.
    dir_inode.link_count += child_dirs as u16;
    dir_inode.checksum = checksum_inode(&dir_inode);
    write_inode(file, layout, parent_ino, &dir_inode)?;

    Ok(child_dirs)
}

/// Read an inode from disk.
fn read_inode(file: &mut File, layout: &Layout, inode_num: u64) -> io::Result<EfsInode> {
    let (group, index) = layout.inode_location(inode_num);
    let inode_table_block = layout.groups[group].inode_table_block;
    let byte_offset = index * 256;
    let block_in_table = byte_offset / layout.block_size as u64;
    let off_in_block = (byte_offset % layout.block_size as u64) as usize;
    let block_buf = read_block(file, layout, inode_table_block + block_in_table)?;
    let inode: EfsInode = unsafe {
        std::ptr::read_unaligned(
            block_buf[off_in_block..off_in_block + 256].as_ptr() as *const EfsInode
        )
    };
    Ok(inode)
}

/// Public entry point: recursively copy `source_dir` into the root of the EFS.
pub fn populate(
    file: &mut File,
    allocator: &mut Allocator,
    layout: &Layout,
    bgds: &mut Vec<EfsBlockGroupDesc>,
    source_dir: &Path,
) -> io::Result<()> {
    // Find the root directory's data block by reading inode 1.
    let root_inode = read_inode(file, layout, EFS_ROOT_INO)?;

    // Parse the first extent to get the data block.
    let hdr_size = std::mem::size_of::<efs_common::EfsExtentHeader>();
    let ext_size = std::mem::size_of::<efs_common::EfsExtent>();
    let root_data_block = {
        let ext: efs_common::EfsExtent = unsafe {
            std::ptr::read_unaligned(root_inode.data_area[hdr_size..hdr_size + ext_size].as_ptr()
                as *const efs_common::EfsExtent)
        };
        ext.physical_start()
    };

    // Initial cursor: after "." and ".." entries.
    let dot_min = efs_common::dir_entry_min_size(1) as usize;
    let dotdot_min = efs_common::dir_entry_min_size(2) as usize;
    let initial_cursor = dot_min + dotdot_min;

    populate_dir(
        file,
        layout,
        allocator,
        source_dir,
        EFS_ROOT_INO,
        root_data_block,
        initial_cursor,
    )?;

    // Update used_dirs_count in BGDs based on allocated inodes.
    // We do a simple pass: read every allocated inode and count directories.
    for g in &layout.groups {
        let gi = g.group as usize;
        let ipg = layout.inodes_per_group as usize;
        let mut dir_count = 0u64;
        for i in 0..ipg {
            let inode_num = gi as u64 * ipg as u64 + i as u64 + 1;
            if inode_num > layout.total_inodes {
                break;
            }
            match read_inode(file, layout, inode_num) {
                Ok(ino) => {
                    if ino.link_count > 0 && (ino.mode & 0xF000) == S_IFDIR {
                        dir_count += 1;
                    }
                }
                Err(_) => break,
            }
        }
        bgds[gi].used_dirs_count = dir_count;
    }

    Ok(())
}
