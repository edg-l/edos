use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::time::SystemTime;

use efs_common::{
    EFS_MAGIC, EFS_ROOT_INO, EFS_VERSION, EXTENT_MAGIC, EfsBlockGroupDesc, EfsExtent,
    EfsExtentHeader, EfsInode, EfsSuperblock, FT_DIR, INCOMPAT_JOURNAL, JOURNAL_MAGIC,
    JournalSuperblock, MAX_INLINE_EXTENTS, S_IFDIR, checksum_block_group_desc, checksum_inode,
    checksum_superblock, journal_sb_checksum,
};
use uuid::Uuid;

use crate::alloc::Allocator;
use crate::disk::{write_block, write_inode, zero_blocks};
use crate::layout::Layout;

/// Write the directory entry bytes for `name` pointing to `inode_num` of type
/// `file_type` into `buf` at `offset`.  Returns the record length written.
fn write_dir_entry(
    buf: &mut [u8],
    offset: usize,
    inode_num: u64,
    name: &[u8],
    file_type: u8,
) -> usize {
    let name_len = name.len() as u8;
    let min_rec = efs_common::dir_entry_min_size(name_len) as usize;
    let rec_len = min_rec as u16;

    // inode (8 bytes LE)
    buf[offset..offset + 8].copy_from_slice(&inode_num.to_le_bytes());
    // rec_len (2 bytes LE)
    buf[offset + 8..offset + 10].copy_from_slice(&rec_len.to_le_bytes());
    // name_len (1 byte)
    buf[offset + 10] = name_len;
    // file_type (1 byte)
    buf[offset + 11] = file_type;
    // name
    buf[offset + 12..offset + 12 + name.len()].copy_from_slice(name);
    // padding already zero (caller zeros the buffer)
    min_rec
}

/// Build the two root "." and ".." entries in a block-sized buffer and return
/// the buffer.  Both entries point to `root_ino`.
fn build_root_dir_block(layout: &Layout, root_ino: u64) -> Vec<u8> {
    let mut buf = vec![0u8; layout.block_size as usize];

    // "." entry
    let dot_min = efs_common::dir_entry_min_size(1) as usize;
    write_dir_entry(&mut buf, 0, root_ino, b".", FT_DIR);

    // ".." entry: set rec_len to reach end of block.
    let dotdot_name_len = 2u8;
    let dotdot_min = efs_common::dir_entry_min_size(dotdot_name_len) as usize;
    let dotdot_rec_len = (layout.block_size as usize - dot_min) as u16;

    let off = dot_min;
    buf[off..off + 8].copy_from_slice(&root_ino.to_le_bytes());
    buf[off + 8..off + 10].copy_from_slice(&dotdot_rec_len.to_le_bytes());
    buf[off + 10] = dotdot_name_len;
    buf[off + 11] = FT_DIR;
    buf[off + 12..off + 14].copy_from_slice(b"..");
    let _ = dotdot_min;

    buf
}

/// Unix timestamp for right now (seconds, nanoseconds).
pub fn now_timestamp() -> (u64, u32) {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_secs(), d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

/// Build an `EfsInode` for a directory.
pub fn make_dir_inode(
    mode_perm: u16,
    link_count: u16,
    size: u64,
    blocks: u64,
    ts: (u64, u32),
) -> EfsInode {
    let mut inode = EfsInode {
        mode: S_IFDIR | mode_perm,
        uid: 0,
        gid: 0,
        link_count,
        size,
        blocks,
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
        data_area: [0u8; 176],
    };
    inode.checksum = checksum_inode(&inode);
    inode
}

/// Store an extent tree with a single extent in the inode's data_area.
pub fn set_inode_extent(inode: &mut EfsInode, logical_block: u32, phys_start: u64, length: u16) {
    let header = EfsExtentHeader {
        magic: EXTENT_MAGIC,
        entries: 1,
        max_entries: MAX_INLINE_EXTENTS as u16,
        depth: 0,
        reserved: 0,
    };
    let extent = EfsExtent {
        logical_block,
        length,
        start_hi: (phys_start >> 32) as u16,
        start_lo: phys_start as u32,
    };
    let header_size = std::mem::size_of::<EfsExtentHeader>();
    let extent_size = std::mem::size_of::<EfsExtent>();
    let hdr_bytes =
        unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, header_size) };
    let ext_bytes =
        unsafe { std::slice::from_raw_parts(&extent as *const _ as *const u8, extent_size) };
    inode.data_area[..header_size].copy_from_slice(hdr_bytes);
    inode.data_area[header_size..header_size + extent_size].copy_from_slice(ext_bytes);
}

/// Add an extent entry to an inode that already has an extent tree initialised.
/// `entry_index` is 0-based (after the header).
pub fn add_inode_extent(
    inode: &mut EfsInode,
    entry_index: usize,
    logical_block: u32,
    phys_start: u64,
    length: u16,
) {
    let header_size = std::mem::size_of::<EfsExtentHeader>();
    let extent_size = std::mem::size_of::<EfsExtent>();
    let extent = EfsExtent {
        logical_block,
        length,
        start_hi: (phys_start >> 32) as u16,
        start_lo: phys_start as u32,
    };
    let off = header_size + entry_index * extent_size;
    let ext_bytes =
        unsafe { std::slice::from_raw_parts(&extent as *const _ as *const u8, extent_size) };
    inode.data_area[off..off + extent_size].copy_from_slice(ext_bytes);

    // Update entries count in the header.
    let entries_off = 2usize; // offset of `entries` field inside EfsExtentHeader
    let cur = u16::from_le_bytes([
        inode.data_area[entries_off],
        inode.data_area[entries_off + 1],
    ]);
    let new_entries = cur.max((entry_index + 1) as u16);
    inode.data_area[entries_off..entries_off + 2].copy_from_slice(&new_entries.to_le_bytes());
}

/// Initialise an empty extent tree header in the inode's data_area.
pub fn init_extent_tree(inode: &mut EfsInode) {
    let header = EfsExtentHeader {
        magic: EXTENT_MAGIC,
        entries: 0,
        max_entries: MAX_INLINE_EXTENTS as u16,
        depth: 0,
        reserved: 0,
    };
    let header_size = std::mem::size_of::<EfsExtentHeader>();
    let hdr_bytes =
        unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, header_size) };
    inode.data_area[..header_size].copy_from_slice(hdr_bytes);
}

/// Write a superblock to `block_num`.
fn write_superblock(
    file: &mut File,
    layout: &Layout,
    block_num: u64,
    sb: &EfsSuperblock,
) -> io::Result<()> {
    let mut buf = vec![0u8; layout.block_size as usize];
    let sb_bytes = unsafe {
        std::slice::from_raw_parts(
            sb as *const _ as *const u8,
            std::mem::size_of::<EfsSuperblock>(),
        )
    };
    buf[..sb_bytes.len()].copy_from_slice(sb_bytes);
    write_block(file, layout, block_num, &buf)
}

/// Write the BGD table starting at `start_block`.
fn write_bgd_table(
    file: &mut File,
    layout: &Layout,
    bgds: &[EfsBlockGroupDesc],
    start_block: u64,
) -> io::Result<()> {
    let bgd_size = std::mem::size_of::<EfsBlockGroupDesc>(); // 64
    let total_bytes = bgds.len() * bgd_size;
    let mut buf = vec![0u8; layout.bgd_table_blocks as usize * layout.block_size as usize];
    for (i, bgd) in bgds.iter().enumerate() {
        let bgd_bytes =
            unsafe { std::slice::from_raw_parts(bgd as *const _ as *const u8, bgd_size) };
        buf[i * bgd_size..i * bgd_size + bgd_size].copy_from_slice(bgd_bytes);
    }
    let _ = total_bytes;
    // Write block by block.
    for b in 0..layout.bgd_table_blocks {
        let offset = b as usize * layout.block_size as usize;
        let chunk = &buf[offset..offset + layout.block_size as usize];
        write_block(file, layout, start_block + b, chunk)?;
    }
    Ok(())
}

/// Format the partition: write superblock, BGD table, bitmaps, inode tables,
/// root directory inode, and journal region.
///
/// `journal_blocks` is the number of 4 KiB blocks to reserve for the journal.
/// The journal is placed at the tail of the partition (last `journal_blocks`
/// blocks), so it never overlaps group metadata.
///
/// Returns the `Allocator` with all metadata marked as used so callers can
/// continue allocating for file population.
pub fn format(
    file: &mut File,
    layout: &Layout,
    label: Option<&str>,
    journal_blocks: u64,
) -> io::Result<(Allocator, Vec<EfsBlockGroupDesc>)> {
    // ---- First, extend/zero the partition area so seeks never go past EOF ----
    let end = layout.partition_offset + layout.total_blocks * layout.block_size as u64;
    let cur_len = file.seek(SeekFrom::End(0))?;
    if cur_len < end {
        file.seek(SeekFrom::Start(end - 1))?;
        file.write_all(&[0u8])?;
    }

    // ---- Build the superblock ----
    let (now_sec, now_nsec) = now_timestamp();
    let _ = now_nsec;
    let uuid = Uuid::new_v4();

    let mut volume_name = [0u8; 64];
    if let Some(lbl) = label {
        let bytes = lbl.as_bytes();
        let len = bytes.len().min(63);
        volume_name[..len].copy_from_slice(&bytes[..len]);
    }

    // first_data_block: first block after BGD table.
    let first_data_block = 2 + layout.bgd_table_blocks;

    // Journal is placed at the tail of the partition: last `journal_blocks` blocks.
    let journal_first_block = layout.total_blocks - journal_blocks;

    let mut sb = EfsSuperblock {
        magic: EFS_MAGIC,
        version: EFS_VERSION,
        total_blocks: layout.total_blocks,
        total_inodes: layout.total_inodes,
        free_blocks: 0, // filled in later
        free_inodes: 0, // filled in later
        block_size_log2: layout.block_size_log2,
        blocks_per_group: layout.blocks_per_group,
        inodes_per_group: layout.inodes_per_group,
        inode_size: 256,
        block_group_count: layout.block_group_count,
        compatible_features: 0,
        incompatible_features: INCOMPAT_JOURNAL,
        read_only_features: 0,
        uuid: *uuid.as_bytes(),
        volume_name,
        mount_time: 0,
        write_time: now_sec,
        mount_count: 0,
        max_mount_count: 0,
        first_data_block,
        checksum: 0,
        journal_first_block,
        journal_block_count: journal_blocks as u32,
        journal_sb_checksum: 0, // filled in after writing journal SB
        fsck_in_progress: 0,
        reserved: [0u8; 47],
    };

    // ---- Mark all partition blocks as zero ----
    // (the image file may already be zeroed, but write reserved block 0)
    write_block(file, layout, 0, &vec![0u8; layout.block_size as usize])?;

    // ---- Build allocator and mark metadata blocks ----
    let mut allocator = Allocator::new(layout.clone());

    // Mark block 0 (reserved), block 1 (superblock), blocks 2..2+bgd_table_blocks (BGD table).
    allocator.mark_block_used(0);
    allocator.mark_block_used(1);
    for b in 0..layout.bgd_table_blocks {
        allocator.mark_block_used(2 + b);
    }

    // Mark journal blocks as used so the allocator never hands them to file data.
    for b in 0..journal_blocks {
        allocator.mark_block_used(journal_first_block + b);
    }

    // ---- Per-group: mark metadata blocks, build BGD entries ----
    let mut bgds: Vec<EfsBlockGroupDesc> = Vec::with_capacity(layout.block_group_count as usize);

    for g in &layout.groups {
        // Mark backup superblock blocks if present (non-group-0 qualified groups).
        if g.has_backup && g.group != 0 {
            // Backup SB at group_start, BGD at group_start+1..
            allocator.mark_block_used(g.group_start);
            for b in 0..layout.bgd_table_blocks {
                allocator.mark_block_used(g.group_start + 1 + b);
            }
        }
        if g.group == 0 {
            // Group 0 backup: comes right after the primary BGD table.
            // blocks 0(reserved), 1(SB), 2..2+bgd(BGD), then backup_SB, backup_BGD.
            let backup_sb_block = 2 + layout.bgd_table_blocks;
            allocator.mark_block_used(backup_sb_block);
            for b in 0..layout.bgd_table_blocks {
                allocator.mark_block_used(backup_sb_block + 1 + b);
            }
        }

        // Mark block bitmap, inode bitmap, inode table blocks.
        allocator.mark_block_used(g.block_bitmap_block);
        allocator.mark_block_used(g.inode_bitmap_block);
        for b in 0..layout.inode_table_blocks as u64 {
            allocator.mark_block_used(g.inode_table_block + b);
        }

        // Zero the inode table.
        zero_blocks(
            file,
            layout,
            g.inode_table_block,
            layout.inode_table_blocks as u64,
        )?;

        let mut bgd = EfsBlockGroupDesc {
            block_bitmap_block: g.block_bitmap_block,
            inode_bitmap_block: g.inode_bitmap_block,
            inode_table_block: g.inode_table_block,
            free_blocks_count: 0, // computed after alloc
            free_inodes_count: 0, // computed after alloc
            used_dirs_count: 0,
            checksum: 0,
            reserved: [0u8; 12],
        };
        bgd.checksum = checksum_block_group_desc(&bgd);
        bgds.push(bgd);
    }

    // ---- Mark inode 0 (invalid, never allocated) as used in group 0 bit 0 ----
    // By convention bit 0 of the inode bitmap is the "invalid" slot.
    // We keep bit 0 free since inodes are 1-based and inode 1 is root.
    // Actually: inode 1 maps to group=0, index=0.  We'll mark it used when we create root.

    // ---- Create root directory inode (inode 1) ----
    allocator.mark_inode_used(EFS_ROOT_INO);

    // Allocate one data block for root dir contents.
    let root_data_block = allocator
        .alloc_block(0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no free blocks for root directory"))?;

    // Write the "." and ".." entries.
    let root_dir_buf = build_root_dir_block(layout, EFS_ROOT_INO);
    write_block(file, layout, root_data_block, &root_dir_buf)?;

    let ts = (now_sec, 0u32);
    let dir_size = layout.block_size as u64;
    let mut root_inode = make_dir_inode(0o755, 2, dir_size, 1, ts);
    set_inode_extent(&mut root_inode, 0, root_data_block, 1);
    root_inode.checksum = checksum_inode(&root_inode);
    write_inode(file, layout, EFS_ROOT_INO, &root_inode)?;

    // Group 0 has one directory now (root).
    bgds[0].used_dirs_count = 1;

    // ---- Write backup superblocks + BGD tables ----
    // Group 0 backup: right after primary BGD table.
    {
        let backup_sb_block = 2 + layout.bgd_table_blocks;
        // We'll compute checksum on the sb after free counts are set.
        // For now write a placeholder; rewrite at the end.
        write_superblock(file, layout, backup_sb_block, &sb)?;
        write_bgd_table(file, layout, &bgds, backup_sb_block + 1)?;
    }

    for g in layout
        .groups
        .iter()
        .filter(|g| g.has_backup && g.group != 0)
    {
        write_superblock(file, layout, g.group_start, &sb)?;
        write_bgd_table(file, layout, &bgds, g.group_start + 1)?;
    }

    // ---- Write bitmaps ----
    allocator.write_bitmaps(file)?;

    // ---- Compute final free counts ----
    let mut total_free_blocks = 0u64;
    let mut total_free_inodes = 0u64;

    for g in &layout.groups {
        let gi = g.group as usize;
        let fb = allocator.free_blocks_in_group(gi);
        let fi = allocator.free_inodes_in_group(gi);
        bgds[gi].free_blocks_count = fb;
        bgds[gi].free_inodes_count = fi;
        bgds[gi].checksum = checksum_block_group_desc(&bgds[gi]);
        total_free_blocks += fb;
        total_free_inodes += fi;
    }

    sb.free_blocks = total_free_blocks;
    sb.free_inodes = total_free_inodes;

    // ---- Write journal region ----
    // Zero all journal blocks first so partial reads during replay never see
    // stale data from a previous format.
    zero_blocks(file, layout, journal_first_block, journal_blocks)?;

    // Build and write the journal superblock at journal_first_block.
    let jsb = JournalSuperblock {
        magic: JOURNAL_MAGIC,
        version: 1,
        block_count: journal_blocks as u32,
        block_size: layout.block_size,
        tail_seq: 1,
        head_seq: 1,
        tail_block: 0,
        head_block: 0,
        crc32: 0, // computed below
        reserved: [0u8; 12],
    };
    let jsb_crc = journal_sb_checksum(&jsb);
    let jsb = JournalSuperblock {
        crc32: jsb_crc,
        ..jsb
    };

    // Store the journal SB checksum in the EFS superblock so the kernel can
    // verify it without re-reading the journal block during fast mount.
    sb.journal_sb_checksum = jsb_crc;

    // Write journal superblock at the first journal block (first 64 bytes;
    // the rest of the block is already zeroed from zero_blocks above).
    let mut jsb_block = vec![0u8; layout.block_size as usize];
    let jsb_bytes = unsafe {
        std::slice::from_raw_parts(
            &jsb as *const JournalSuperblock as *const u8,
            std::mem::size_of::<JournalSuperblock>(),
        )
    };
    jsb_block[..jsb_bytes.len()].copy_from_slice(jsb_bytes);
    write_block(file, layout, journal_first_block, &jsb_block)?;

    sb.checksum = checksum_superblock(&sb);

    // ---- Write primary superblock and BGD table ----
    write_superblock(file, layout, 1, &sb)?;
    write_bgd_table(file, layout, &bgds, 2)?;

    // ---- Rewrite backup SB/BGD with correct free counts ----
    {
        let backup_sb_block = 2 + layout.bgd_table_blocks;
        write_superblock(file, layout, backup_sb_block, &sb)?;
        write_bgd_table(file, layout, &bgds, backup_sb_block + 1)?;
    }
    for g in layout
        .groups
        .iter()
        .filter(|g| g.has_backup && g.group != 0)
    {
        write_superblock(file, layout, g.group_start, &sb)?;
        write_bgd_table(file, layout, &bgds, g.group_start + 1)?;
    }

    // ---- Write primary BGD table again (free counts updated) ----
    write_bgd_table(file, layout, &bgds, 2)?;

    Ok((allocator, bgds))
}

/// Rewrite BGD table and superblock with updated free counts after population.
pub fn finalize(
    file: &mut File,
    layout: &Layout,
    allocator: &Allocator,
    bgds: &mut Vec<EfsBlockGroupDesc>,
) -> io::Result<()> {
    // Re-read and update the primary superblock.
    let mut sb_buf = crate::disk::read_block(file, layout, 1)?;
    let sb: &mut EfsSuperblock = unsafe { &mut *(sb_buf.as_mut_ptr() as *mut EfsSuperblock) };

    let mut total_free_blocks = 0u64;
    let mut total_free_inodes = 0u64;

    for g in &layout.groups {
        let gi = g.group as usize;
        let fb = allocator.free_blocks_in_group(gi);
        let fi = allocator.free_inodes_in_group(gi);
        bgds[gi].free_blocks_count = fb;
        bgds[gi].free_inodes_count = fi;
        bgds[gi].checksum = checksum_block_group_desc(&bgds[gi]);
        total_free_blocks += fb;
        total_free_inodes += fi;
    }

    sb.free_blocks = total_free_blocks;
    sb.free_inodes = total_free_inodes;
    sb.checksum = checksum_superblock(sb);

    write_block(file, layout, 1, &sb_buf)?;
    write_bgd_table(file, layout, bgds, 2)?;

    // Update backups.
    {
        let backup_sb_block = 2 + layout.bgd_table_blocks;
        write_block(file, layout, backup_sb_block, &sb_buf)?;
        write_bgd_table(file, layout, bgds, backup_sb_block + 1)?;
    }
    for g in layout
        .groups
        .iter()
        .filter(|g| g.has_backup && g.group != 0)
    {
        write_block(file, layout, g.group_start, &sb_buf)?;
        write_bgd_table(file, layout, bgds, g.group_start + 1)?;
    }

    // Write updated bitmaps.
    allocator.write_bitmaps(file)?;

    Ok(())
}
