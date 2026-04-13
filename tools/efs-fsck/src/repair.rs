use std::collections::HashMap;
use std::io::{self, Write};

use efs_common::{
    EfsBlockGroupDesc, EfsInode, EfsSuperblock, S_IFDIR, S_IFMT, checksum_block_group_desc,
    checksum_inode, checksum_superblock, has_superblock_backup,
};

use crate::bitmaps::InodeInfo;
use crate::disk::Disk;
use crate::layout::RuntimeLayout;
use crate::report::{Category, Finding, Severity};

/// Summary of what the repair pass did.
pub struct RepairStats {
    pub attempted: u32,
    pub succeeded: u32,
    pub skipped: u32,
}

/// Prompt the user for a fix class unless `-y` was passed.
///
/// If `yes` is true: prints an auto-accept message and returns true.
/// Otherwise: prints a `[y/N]` prompt, reads stdin, returns true on `y`/`Y`.
pub fn prompt(class: &str, count: usize, yes: bool) -> bool {
    if yes {
        println!("auto-accepting {count} fixes for {class}");
        return true;
    }
    print!("Fix {count} {class}? [y/N]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    matches!(line.trim(), "y" | "Y")
}

// ---------------------------------------------------------------------------
// Superblock / BGD on-disk writers
// ---------------------------------------------------------------------------

/// Write the primary superblock (block 1) and all backup copies.
///
/// The caller must have already set `sb.checksum` to the correct value.
fn write_superblock_all(disk: &mut Disk, sb: &EfsSuperblock) -> io::Result<()> {
    let layout = RuntimeLayout::from_superblock(sb);
    let block_size = layout.block_size as usize;

    let mut buf = vec![0u8; block_size];
    let sb_bytes = unsafe {
        std::slice::from_raw_parts(
            sb as *const _ as *const u8,
            std::mem::size_of::<EfsSuperblock>(),
        )
    };
    buf[..sb_bytes.len()].copy_from_slice(sb_bytes);

    // Primary superblock at block 1.
    disk.write_block(1, &buf)?;

    let bgd_table_blocks = layout.bgd_table_blocks;
    let blocks_per_group = layout.blocks_per_group as u64;
    let block_group_count = layout.block_group_count;

    // Group 0 backup: right after primary BGD table.
    let group0_backup = 2 + bgd_table_blocks;
    disk.write_block(group0_backup, &buf)?;

    // Remaining backup groups (groups >= 1 that carry a backup).
    for g in 1u16..block_group_count {
        if has_superblock_backup(g) {
            let group_start = g as u64 * blocks_per_group;
            disk.write_block(group_start, &buf)?;
        }
    }

    Ok(())
}

/// Write the BGD table starting at `start_block` (fills `bgd_table_blocks` blocks).
fn write_bgd_table_at(
    disk: &mut Disk,
    bgds: &[EfsBlockGroupDesc],
    start_block: u64,
    bgd_table_blocks: u64,
    block_size: u32,
) -> io::Result<()> {
    let bgd_size = std::mem::size_of::<EfsBlockGroupDesc>();
    let total_buf_len = bgd_table_blocks as usize * block_size as usize;
    let mut buf = vec![0u8; total_buf_len];

    for (i, bgd) in bgds.iter().enumerate() {
        let bgd_bytes =
            unsafe { std::slice::from_raw_parts(bgd as *const _ as *const u8, bgd_size) };
        let off = i * bgd_size;
        buf[off..off + bgd_size].copy_from_slice(bgd_bytes);
    }

    for b in 0..bgd_table_blocks {
        let off = b as usize * block_size as usize;
        disk.write_block(start_block + b, &buf[off..off + block_size as usize])?;
    }

    Ok(())
}

/// Write the primary BGD table and all backup copies.
fn write_bgd_table_all(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
) -> io::Result<()> {
    let layout = RuntimeLayout::from_superblock(sb);
    let bgd_table_blocks = layout.bgd_table_blocks;
    let block_size = layout.block_size;
    let blocks_per_group = layout.blocks_per_group as u64;
    let block_group_count = layout.block_group_count;

    // Primary BGD table at blocks 2..2+bgd_table_blocks.
    write_bgd_table_at(disk, bgds, 2, bgd_table_blocks, block_size)?;

    // Group 0 backup: right after the primary BGD table.
    let group0_backup_bgd = 2 + bgd_table_blocks + 1; // backup SB is at 2+bgd_table_blocks
    write_bgd_table_at(disk, bgds, group0_backup_bgd, bgd_table_blocks, block_size)?;

    // Remaining backup groups.
    for g in 1u16..block_group_count {
        if has_superblock_backup(g) {
            let group_start = g as u64 * blocks_per_group;
            let backup_bgd_start = group_start + 1;
            write_bgd_table_at(disk, bgds, backup_bgd_start, bgd_table_blocks, block_size)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// fsck_in_progress sentinel
// ---------------------------------------------------------------------------

/// Set or clear the `fsck_in_progress` sentinel in the superblock and persist
/// to disk (primary + all backups).
pub fn set_fsck_in_progress(
    disk: &mut Disk,
    sb: &mut EfsSuperblock,
    value: bool,
) -> io::Result<()> {
    sb.fsck_in_progress = value as u8;
    let csum = checksum_superblock(sb);
    sb.checksum = csum;
    write_superblock_all(disk, sb)?;
    disk.fsync()
}

// ---------------------------------------------------------------------------
// Inode write helper
// ---------------------------------------------------------------------------

fn write_inode(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
    ino: u64,
    inode: &EfsInode,
) -> io::Result<()> {
    let inodes_per_group = sb.inodes_per_group as u64;
    let inode_size = sb.inode_size as usize;
    let block_size = disk.block_size as usize;

    let group = ((ino - 1) / inodes_per_group) as usize;
    let local_idx = (ino - 1) % inodes_per_group;
    let bgd = &bgds[group];

    let byte_offset = local_idx as usize * inode_size;
    let block = bgd.inode_table_block + (byte_offset / block_size) as u64;
    let offset_in_block = byte_offset % block_size;

    disk.write_struct_at(block, offset_in_block, inode)
}

// ---------------------------------------------------------------------------
// apply_repairs — top-level entry point
// ---------------------------------------------------------------------------

/// Apply all fixable findings to the filesystem image.
///
/// `observed_links` and `inode_infos` are threaded in to avoid parsing
/// finding messages for link-count repair (Task 5.7).
pub fn apply_repairs(
    disk: &mut Disk,
    sb: &mut EfsSuperblock,
    bgds: &mut Vec<EfsBlockGroupDesc>,
    findings: &mut [Finding],
    yes: bool,
    observed_links: &HashMap<u64, u32>,
    inode_infos: &[InodeInfo],
) -> io::Result<RepairStats> {
    let mut stats = RepairStats {
        attempted: 0,
        succeeded: 0,
        skipped: 0,
    };

    // Mark fsck in progress before any writes.
    set_fsck_in_progress(disk, sb, true)?;

    // ---- Class 1: leaked block bitmap bits ----
    repair_leaked_bitmap_bits(disk, sb, bgds, findings, yes, &mut stats)?;

    // ---- Class 2: leaked inode bitmap bits ----
    repair_leaked_inode_bits(disk, sb, bgds, findings, yes, &mut stats)?;

    // ---- Class 4: link-count mismatches (non-destructive) ----
    repair_link_counts(
        disk,
        sb,
        bgds,
        findings,
        yes,
        &mut stats,
        observed_links,
        inode_infos,
    )?;

    // ---- Class 5: orphan inodes (destructive) ----
    repair_orphans(disk, sb, bgds, findings, yes, &mut stats, inode_infos)?;

    // ---- Class 3: BGD sums + superblock free counts (runs last) ----
    repair_bgd_sums(disk, sb, bgds, findings, yes, &mut stats)?;

    // Clear the in-progress sentinel on success.
    set_fsck_in_progress(disk, sb, false)?;

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Class 1: leaked block bitmap bits
// ---------------------------------------------------------------------------

fn repair_leaked_bitmap_bits(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &mut Vec<EfsBlockGroupDesc>,
    findings: &mut [Finding],
    yes: bool,
    stats: &mut RepairStats,
) -> io::Result<()> {
    // Collect indices of all leaked block-bitmap findings.
    let leaked: Vec<usize> = findings
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.category == Category::BlockBitmap
                && f.fixable
                && f.message.starts_with("leaked block-bitmap bit at")
        })
        .map(|(i, _)| i)
        .collect();

    if leaked.is_empty() {
        return Ok(());
    }

    stats.attempted += leaked.len() as u32;

    if !prompt("leaked block bitmap bits", leaked.len(), yes) {
        stats.skipped += leaked.len() as u32;
        return Ok(());
    }

    // Parse each global bit index from the message and clear it on disk.
    let blocks_per_group = sb.blocks_per_group as u64;

    for &fi in &leaked {
        let msg = &findings[fi].message;
        // Message format: "leaked block-bitmap bit at {global_idx}"
        let global_idx: u64 = match parse_trailing_u64(msg) {
            Some(v) => v,
            None => {
                eprintln!("repair: could not parse bit index from: {msg}");
                stats.skipped += 1;
                stats.attempted -= 1;
                continue;
            }
        };

        let group = (global_idx / blocks_per_group) as usize;
        if group >= bgds.len() {
            continue;
        }
        let local_idx = global_idx % blocks_per_group;

        let bitmap_block = bgds[group].block_bitmap_block;
        let mut bm = disk.read_block(bitmap_block)?;

        let byte = (local_idx / 8) as usize;
        let bit = local_idx % 8;
        if byte < bm.len() {
            bm[byte] &= !(1u8 << bit);
        }
        disk.write_block(bitmap_block, &bm)?;

        bgds[group].free_blocks_count += 1;
        findings[fi].fixable = false; // mark as handled
        stats.succeeded += 1;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Class 2: leaked inode bitmap bits
// ---------------------------------------------------------------------------

fn repair_leaked_inode_bits(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &mut Vec<EfsBlockGroupDesc>,
    findings: &mut [Finding],
    yes: bool,
    stats: &mut RepairStats,
) -> io::Result<()> {
    let leaked: Vec<usize> = findings
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.category == Category::InodeBitmap
                && f.fixable
                && f.message.starts_with("leaked inode-bitmap bit at")
        })
        .map(|(i, _)| i)
        .collect();

    if leaked.is_empty() {
        return Ok(());
    }

    stats.attempted += leaked.len() as u32;

    if !prompt("leaked inode bitmap bits", leaked.len(), yes) {
        stats.skipped += leaked.len() as u32;
        return Ok(());
    }

    let inodes_per_group = sb.inodes_per_group as u64;

    for &fi in &leaked {
        let msg = &findings[fi].message;
        // Message format: "leaked inode-bitmap bit at {global_idx}"
        let global_idx: u64 = match parse_trailing_u64(msg) {
            Some(v) => v,
            None => {
                eprintln!("repair: could not parse bit index from: {msg}");
                stats.skipped += 1;
                stats.attempted -= 1;
                continue;
            }
        };

        let group = (global_idx / inodes_per_group) as usize;
        if group >= bgds.len() {
            continue;
        }
        let local_idx = global_idx % inodes_per_group;

        let bitmap_block = bgds[group].inode_bitmap_block;
        let mut bm = disk.read_block(bitmap_block)?;

        let byte = (local_idx / 8) as usize;
        let bit = local_idx % 8;
        if byte < bm.len() {
            bm[byte] &= !(1u8 << bit);
        }
        disk.write_block(bitmap_block, &bm)?;

        bgds[group].free_inodes_count += 1;
        findings[fi].fixable = false;
        stats.succeeded += 1;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Class 3: BGD sums + superblock free counts (runs last)
// ---------------------------------------------------------------------------

fn repair_bgd_sums(
    disk: &mut Disk,
    sb: &mut EfsSuperblock,
    bgds: &mut Vec<EfsBlockGroupDesc>,
    findings: &mut [Finding],
    yes: bool,
    stats: &mut RepairStats,
) -> io::Result<()> {
    // Count fixable BGD / superblock sum findings.
    let fixable_count = findings
        .iter()
        .filter(|f| f.category == Category::Bgd && f.fixable)
        .count();

    if fixable_count == 0 {
        return Ok(());
    }

    stats.attempted += fixable_count as u32;

    if !prompt("BGD/superblock free-count mismatches", fixable_count, yes) {
        stats.skipped += fixable_count as u32;
        return Ok(());
    }

    // Recompute SB free counts from BGD sums and update BGD checksums.
    let mut total_free_blocks: u64 = 0;
    let mut total_free_inodes: u64 = 0;

    for bgd in bgds.iter_mut() {
        total_free_blocks += bgd.free_blocks_count;
        total_free_inodes += bgd.free_inodes_count;
        bgd.checksum = checksum_block_group_desc(bgd);
    }

    sb.free_blocks = total_free_blocks;
    sb.free_inodes = total_free_inodes;
    sb.checksum = checksum_superblock(sb);

    write_bgd_table_all(disk, sb, bgds)?;
    write_superblock_all(disk, sb)?;

    // Mark related findings as handled.
    for f in findings.iter_mut() {
        if f.category == Category::Bgd && f.fixable {
            f.fixable = false;
            stats.succeeded += 1;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Class 4: link-count mismatches
// ---------------------------------------------------------------------------

fn repair_link_counts(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
    findings: &mut [Finding],
    yes: bool,
    stats: &mut RepairStats,
    observed_links: &HashMap<u64, u32>,
    inode_infos: &[InodeInfo],
) -> io::Result<()> {
    // Fixable DirTree findings that are NOT orphans (no "destructive" context).
    let mismatch_indices: Vec<usize> = findings
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.category == Category::DirTree
                && f.fixable
                && f.context.as_deref() != Some("destructive")
                && f.message.contains("link_count on-disk=")
        })
        .map(|(i, _)| i)
        .collect();

    if mismatch_indices.is_empty() {
        return Ok(());
    }

    stats.attempted += mismatch_indices.len() as u32;

    if !prompt("link-count mismatches", mismatch_indices.len(), yes) {
        stats.skipped += mismatch_indices.len() as u32;
        return Ok(());
    }

    // Build a fast inode-number -> InodeInfo lookup.
    let info_map: HashMap<u64, &InodeInfo> = inode_infos.iter().map(|i| (i.ino, i)).collect();

    for &fi in &mismatch_indices {
        // Message format: "inode {ino} link_count on-disk={expected} observed={obs}"
        let ino = match parse_first_u64(&findings[fi].message) {
            Some(v) => v,
            None => {
                stats.skipped += 1;
                stats.attempted -= 1;
                continue;
            }
        };

        let observed = match observed_links.get(&ino).copied() {
            Some(v) => v,
            None => {
                stats.skipped += 1;
                stats.attempted -= 1;
                continue;
            }
        };

        // Saturating cast: link_count is u16 in the on-disk inode.
        let new_link_count = observed.min(u16::MAX as u32) as u16;

        if info_map.get(&ino).is_none() {
            stats.skipped += 1;
            stats.attempted -= 1;
            continue;
        }

        // Read current inode, patch link_count, recompute checksum, write back.
        let mut inode: EfsInode = {
            let inodes_per_group = sb.inodes_per_group as u64;
            let inode_size = sb.inode_size as usize;
            let block_size = disk.block_size as usize;

            let group = ((ino - 1) / inodes_per_group) as usize;
            let local_idx = (ino - 1) % inodes_per_group;
            let bgd = &bgds[group];
            let byte_offset = local_idx as usize * inode_size;
            let block = bgd.inode_table_block + (byte_offset / block_size) as u64;
            let offset_in_block = byte_offset % block_size;

            disk.read_struct_at(block, offset_in_block)?
        };

        inode.link_count = new_link_count;
        inode.checksum = checksum_inode(&inode);
        write_inode(disk, sb, bgds, ino, &inode)?;

        findings[fi].fixable = false;
        stats.succeeded += 1;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Class 5: orphan inodes (destructive)
// ---------------------------------------------------------------------------

fn repair_orphans(
    disk: &mut Disk,
    sb: &mut EfsSuperblock,
    bgds: &mut Vec<EfsBlockGroupDesc>,
    findings: &mut [Finding],
    yes: bool,
    stats: &mut RepairStats,
    inode_infos: &[InodeInfo],
) -> io::Result<()> {
    let orphan_indices: Vec<usize> = findings
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.category == Category::DirTree
                && f.fixable
                && f.context.as_deref() == Some("destructive")
        })
        .map(|(i, _)| i)
        .collect();

    if orphan_indices.is_empty() {
        return Ok(());
    }

    stats.attempted += orphan_indices.len() as u32;

    // Separate prompt with a loud warning.
    if yes {
        println!(
            "WARNING: auto-accepting deletion of {} orphan inode(s). This frees their blocks.",
            orphan_indices.len()
        );
    } else {
        println!(
            "WARNING: {} orphan inode(s) found. Deleting them will free their blocks permanently.",
            orphan_indices.len()
        );
        print!(
            "Delete {} orphan inode(s)? This frees their blocks. [y/N]: ",
            orphan_indices.len()
        );
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if !matches!(line.trim(), "y" | "Y") {
            stats.skipped += orphan_indices.len() as u32;
            return Ok(());
        }
    }

    let info_map: HashMap<u64, &InodeInfo> = inode_infos.iter().map(|i| (i.ino, i)).collect();
    let blocks_per_group = sb.blocks_per_group as u64;
    let inodes_per_group = sb.inodes_per_group as u64;

    // Build the set of double-claimed blocks from Phase 3 findings. These
    // blocks are claimed by both an orphan AND a live inode; clearing the
    // bitmap bit would silently corrupt the live inode. Skip them.
    let double_claimed: std::collections::HashSet<u64> = findings
        .iter()
        .filter(|f| f.category == Category::BlockBitmap && f.message.contains("double-claimed"))
        .filter_map(|f| {
            // Message: "block {b} double-claimed by inodes {a} and {b}"
            f.message
                .strip_prefix("block ")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|s| s.parse::<u64>().ok())
        })
        .collect();

    for &fi in &orphan_indices {
        // Message format: "orphan inode {ino} (reachable link count 0)"
        let ino = match parse_first_u64(&findings[fi].message) {
            Some(v) => v,
            None => {
                stats.skipped += 1;
                stats.attempted -= 1;
                continue;
            }
        };

        let info = match info_map.get(&ino) {
            Some(i) => *i,
            None => {
                stats.skipped += 1;
                stats.attempted -= 1;
                continue;
            }
        };

        // Clear block-bitmap bits for all extents, skipping any block also
        // claimed by a live inode (would corrupt the live data).
        for &(phys_start, length) in &info.extents {
            for b in phys_start..phys_start + length as u64 {
                if double_claimed.contains(&b) {
                    continue;
                }
                let group = (b / blocks_per_group) as usize;
                if group >= bgds.len() {
                    continue;
                }
                let local_bit = b % blocks_per_group;
                let bitmap_block = bgds[group].block_bitmap_block;
                let mut bm = disk.read_block(bitmap_block)?;
                let byte = (local_bit / 8) as usize;
                let bit = local_bit % 8;
                if byte < bm.len() {
                    bm[byte] &= !(1u8 << bit);
                }
                disk.write_block(bitmap_block, &bm)?;
                bgds[group].free_blocks_count = bgds[group].free_blocks_count.saturating_add(1);
            }
        }

        // Clear inode-bitmap bit.
        let inode_group = ((ino - 1) / inodes_per_group) as usize;
        if inode_group < bgds.len() {
            let local_bit = (ino - 1) % inodes_per_group;
            let bitmap_block = bgds[inode_group].inode_bitmap_block;
            let mut bm = disk.read_block(bitmap_block)?;
            let byte = (local_bit / 8) as usize;
            let bit = local_bit % 8;
            if byte < bm.len() {
                bm[byte] &= !(1u8 << bit);
            }
            disk.write_block(bitmap_block, &bm)?;
            bgds[inode_group].free_inodes_count =
                bgds[inode_group].free_inodes_count.saturating_add(1);

            // Decrement used_dirs_count if this was a directory.
            if info.mode & S_IFMT == S_IFDIR {
                if bgds[inode_group].used_dirs_count > 0 {
                    bgds[inode_group].used_dirs_count -= 1;
                }
            }
        }

        // Zero the inode table entry.
        let inode_size = sb.inode_size as usize;
        let block_size = disk.block_size as usize;
        if inode_group < bgds.len() {
            let local_idx = (ino - 1) % inodes_per_group;
            let byte_offset = local_idx as usize * inode_size;
            let block = bgds[inode_group].inode_table_block + (byte_offset / block_size) as u64;
            let offset_in_block = byte_offset % block_size;
            // Write a zeroed inode.
            let zero_inode = EfsInode {
                mode: 0,
                uid: 0,
                gid: 0,
                link_count: 0,
                size: 0,
                blocks: 0,
                flags: 0,
                reserved1: 0,
                ctime_sec: 0,
                ctime_nsec: 0,
                reserved2: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                reserved3: 0,
                atime_sec: 0,
                atime_nsec: 0,
                checksum: 0,
                data_area: [0u8; efs_common::INODE_DATA_AREA_SIZE],
            };
            disk.write_struct_at(block, offset_in_block, &zero_inode)?;
        }

        findings[fi].fixable = false;
        stats.succeeded += 1;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Message parsing helpers
// ---------------------------------------------------------------------------

/// Parse the last whitespace-delimited token in `s` as a `u64`.
///
/// Used for finding messages like "leaked block-bitmap bit at 42".
fn parse_trailing_u64(s: &str) -> Option<u64> {
    s.split_whitespace().last()?.parse().ok()
}

/// Parse the first whitespace-delimited u64 token in `s` that parses
/// successfully.
///
/// Used for messages like "inode 7 link_count on-disk=..." or
/// "orphan inode 7 (...)".
fn parse_first_u64(s: &str) -> Option<u64> {
    s.split_whitespace().find_map(|tok| tok.parse::<u64>().ok())
}

// ---------------------------------------------------------------------------
// Severity helper (used by main.rs to re-evaluate the report after repair)
// ---------------------------------------------------------------------------

/// Count Error-severity findings in a list (used both pre- and post-repair).
///
/// After repair, "remaining errors" = this count MINUS `RepairStats::succeeded`,
/// since each successful repair corresponds to exactly one Error finding.
pub fn count_error_findings(findings: &[Finding]) -> usize {
    findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count()
}
