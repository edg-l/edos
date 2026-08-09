use std::io;

use efs_common::{
    EXTENT_MAGIC, EfsBlockGroupDesc, EfsExtent, EfsExtentIndex, EfsInode, EfsSuperblock,
    INODE_DATA_AREA_SIZE, INODE_FLAG_INLINE_DATA, MAX_INLINE_EXTENTS, checksum_inode,
    entries_per_node, read_node_entry, read_node_header,
};

use crate::bitmaps::InodeInfo;
use crate::disk::Disk;
use crate::report::{Category, Finding, Report, Severity};

/// Scan all allocated inodes across every block group.
///
/// Reads each group's inode bitmap, then for each set bit reads and verifies
/// the inode. Returns a `Vec<InodeInfo>` for all structurally valid inodes.
pub fn scan_all_inodes(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
    report: &mut Report,
) -> io::Result<Vec<InodeInfo>> {
    // Copy packed fields.
    let inodes_per_group = sb.inodes_per_group as u64;
    let inode_size = sb.inode_size as usize;
    let total_inodes = sb.total_inodes;

    let mut all_infos: Vec<InodeInfo> = Vec::new();

    for (g, bgd) in bgds.iter().enumerate() {
        // Read the inode bitmap for this group (one block).
        let inode_bm = disk.read_block(bgd.inode_bitmap_block)?;

        // Determine how many inodes are in this group (last group may be partial).
        let group_inode_base = g as u64 * inodes_per_group;
        let group_inode_count = inodes_per_group.min(total_inodes.saturating_sub(group_inode_base));

        let mut group_inode_count_found = 0u64;
        let mut group_block_count_found = 0u64;

        for local_idx in 0..group_inode_count {
            let byte = (local_idx / 8) as usize;
            let bit = local_idx % 8;
            if byte >= inode_bm.len() || inode_bm[byte] & (1 << bit) == 0 {
                continue;
            }

            // Inode numbers are 1-based globally.
            let ino = group_inode_base + local_idx + 1;

            // Read the inode from the inode table.
            let inode_table_block = bgd.inode_table_block;
            let block_size = disk.block_size as usize;
            let byte_offset_in_table = local_idx as usize * inode_size;
            let block_offset = byte_offset_in_table / block_size;
            let offset_in_block = byte_offset_in_table % block_size;
            let inode_block = inode_table_block + block_offset as u64;

            let inode: EfsInode = disk.read_struct_at(inode_block, offset_in_block)?;

            if let Some(info) = verify_inode(disk, &inode, ino, sb, report) {
                group_block_count_found += info.extents.iter().map(|&(_, l)| l as u64).sum::<u64>();
                group_inode_count_found += 1;
                all_infos.push(info);
            }
        }

        // Verbose logging handled by the caller via the returned infos count.
        // Store group stats as an Info finding so verbose mode can show them.
        report.push(Finding {
            severity: Severity::Info,
            category: Category::Inode,
            message: format!(
                "group {g}: {} inodes, {} blocks",
                group_inode_count_found, group_block_count_found
            ),
            fixable: false,
            context: None,
        });
    }

    Ok(all_infos)
}

/// Verify a single inode and return an `InodeInfo` if it is structurally sound.
///
/// Returns `None` on a fatal structural error (header magic wrong, depth out
/// of range, etc.) so the caller skips bitmap seeding for this inode.
///
/// The returned extents cover the inode's data blocks *and*, for a depth-1
/// tree, its leaf blocks: both are allocated, so both have to be seeded into
/// the rebuilt bitmap or the leaves are reported as leaked.
fn verify_inode(
    disk: &mut Disk,
    inode: &EfsInode,
    ino: u64,
    sb: &EfsSuperblock,
    report: &mut Report,
) -> Option<InodeInfo> {
    let total_blocks = sb.total_blocks;

    // (a) Checksum.
    let expected_csum = checksum_inode(inode);
    if expected_csum != inode.checksum {
        report.push(Finding {
            severity: Severity::Warning,
            category: Category::Inode,
            message: format!(
                "inode {ino}: CRC mismatch (on-disk {:#010x}, computed {:#010x})",
                inode.checksum, expected_csum
            ),
            fixable: false,
            context: None,
        });
        // Continue — CRC mismatch is a warning, not fatal.
    }

    // (b) link_count > 0. Report-only: still seed the inode's extents into the
    // rebuilt bitmap so we don't double-count the same blocks as "leaked".
    if inode.link_count == 0 {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Inode,
            message: format!("inode {ino}: allocated but link_count == 0 (orphan)"),
            fixable: false,
            context: None,
        });
    }

    let is_inline = inode.flags & INODE_FLAG_INLINE_DATA != 0;

    let extents = if is_inline {
        // (c) Inline size must fit in data_area.
        if inode.size > INODE_DATA_AREA_SIZE as u64 {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: inline-data size {} exceeds data_area capacity {}",
                    inode.size, INODE_DATA_AREA_SIZE
                ),
                fixable: false,
                context: None,
            });
            return None;
        }
        // Inline inodes have no extent blocks to track.
        Vec::new()
    } else {
        // (d) Parse the extent node in data_area. `depth == 0` puts the
        // file's extents there directly; `depth == 1` puts index entries
        // there, each naming a leaf block that holds the extents.
        let Some(hdr) = read_node_header(&inode.data_area) else {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!("inode {ino}: data_area too small for an extent header"),
                fixable: false,
                context: None,
            });
            return None;
        };

        if hdr.magic != EXTENT_MAGIC {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: bad extent header magic {:#06x} (expected {:#06x})",
                    hdr.magic, EXTENT_MAGIC
                ),
                fixable: false,
                context: None,
            });
            return None;
        }
        if hdr.depth > 1 {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: extent tree depth {} > 1 (v1 supports depth 0 and 1)",
                    hdr.depth
                ),
                fixable: false,
                context: None,
            });
            return None;
        }
        if hdr.entries as usize > MAX_INLINE_EXTENTS {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: extent entries {} > MAX_INLINE_EXTENTS {}",
                    hdr.entries, MAX_INLINE_EXTENTS
                ),
                fixable: false,
                context: None,
            });
            return None;
        }
        if hdr.max_entries as usize != MAX_INLINE_EXTENTS {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: extent max_entries {} != expected {}",
                    hdr.max_entries, MAX_INLINE_EXTENTS
                ),
                fixable: false,
                context: None,
            });
            return None;
        }

        let mut collected: Vec<(u64, u16)> = Vec::with_capacity(hdr.entries as usize);

        if hdr.depth == 0 {
            if !collect_leaf_extents(
                &inode.data_area,
                hdr.entries as usize,
                ino,
                total_blocks,
                report,
                &mut collected,
            ) {
                return None;
            }
        } else {
            report.push(Finding {
                severity: Severity::Info,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: depth-1 extent tree with {} leaf block(s)",
                    hdr.entries
                ),
                fixable: false,
                context: None,
            });
            let per_leaf = entries_per_node(disk.block_size as usize);
            for i in 0..hdr.entries as usize {
                let Some(idx) = read_node_entry::<EfsExtentIndex>(&inode.data_area, i) else {
                    report.push(Finding {
                        severity: Severity::Error,
                        category: Category::Inode,
                        message: format!("inode {ino}: index entry {i} out of data_area bounds"),
                        fixable: false,
                        context: None,
                    });
                    return None;
                };
                let child = idx.child_block();
                if child == 0 || child >= total_blocks {
                    report.push(Finding {
                        severity: Severity::Error,
                        category: Category::Inode,
                        message: format!(
                            "inode {ino}: extent index {i} child block {child} outside 1..{total_blocks}"
                        ),
                        fixable: false,
                        context: None,
                    });
                    return None;
                }

                let leaf = match disk.read_block(child) {
                    Ok(b) => b,
                    Err(e) => {
                        report.push(Finding {
                            severity: Severity::Error,
                            category: Category::Inode,
                            message: format!(
                                "inode {ino}: cannot read extent leaf block {child}: {e}"
                            ),
                            fixable: false,
                            context: None,
                        });
                        return None;
                    }
                };
                let Some(leaf_hdr) = read_node_header(&leaf) else {
                    report.push(Finding {
                        severity: Severity::Error,
                        category: Category::Inode,
                        message: format!("inode {ino}: extent leaf block {child} is too small"),
                        fixable: false,
                        context: None,
                    });
                    return None;
                };
                if leaf_hdr.magic != EXTENT_MAGIC
                    || leaf_hdr.depth != 0
                    || leaf_hdr.entries as usize > per_leaf
                {
                    report.push(Finding {
                        severity: Severity::Error,
                        category: Category::Inode,
                        message: format!(
                            "inode {ino}: extent leaf block {child} malformed (magic {:#06x}, depth {}, entries {})",
                            leaf_hdr.magic, leaf_hdr.depth, leaf_hdr.entries
                        ),
                        fixable: false,
                        context: None,
                    });
                    return None;
                }

                // The leaf block is itself allocated storage.
                collected.push((child, 1));

                if !collect_leaf_extents(
                    &leaf,
                    leaf_hdr.entries as usize,
                    ino,
                    total_blocks,
                    report,
                    &mut collected,
                ) {
                    return None;
                }
            }
        }

        collected
    };

    Some(InodeInfo {
        ino,
        mode: inode.mode,
        link_count: inode.link_count,
        is_dir: inode.is_dir(),
        extents,
    })
}

/// Validate the leaf extents in `node` and append them to `collected`.
///
/// Returns `false` when the node is structurally broken and the inode should
/// be skipped entirely. An extent that is merely out of bounds is reported and
/// dropped, so the rest of the file is still accounted for.
fn collect_leaf_extents(
    node: &[u8],
    count: usize,
    ino: u64,
    total_blocks: u64,
    report: &mut Report,
    collected: &mut Vec<(u64, u16)>,
) -> bool {
    let mut ok = true;
    for i in 0..count {
        let Some(ext) = read_node_entry::<EfsExtent>(node, i) else {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!("inode {ino}: extent index {i} out of node bounds"),
                fixable: false,
                context: None,
            });
            return false;
        };

        let phys = ext.physical_start();
        let len = ext.length;

        // Bit 15 of length is reserved; valid range 1..=32767.
        if len == 0 || len >= 32768 {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: extent {i} has invalid length {len} (must be 1..=32767)"
                ),
                fixable: false,
                context: None,
            });
            ok = false;
            continue;
        }

        if phys + len as u64 > total_blocks {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: extent {i} physical range {phys}..{} exceeds total_blocks {total_blocks}",
                    phys + len as u64
                ),
                fixable: false,
                context: None,
            });
            // Unreferenceable: do not seed the bitmap with it.
            continue;
        }

        collected.push((phys, len));
    }
    ok
}
