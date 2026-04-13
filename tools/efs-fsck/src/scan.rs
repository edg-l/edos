use std::io;

use efs_common::{
    EXTENT_MAGIC, EfsBlockGroupDesc, EfsExtent, EfsExtentHeader, EfsInode, EfsSuperblock,
    INODE_DATA_AREA_SIZE, INODE_FLAG_INLINE_DATA, MAX_INLINE_EXTENTS, checksum_inode,
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

            if let Some(info) = verify_inode(&inode, ino, sb, report) {
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
/// Returns `None` on a fatal structural error (header magic wrong, depth != 0,
/// etc.) so the caller skips bitmap seeding for this inode.
fn verify_inode(
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
        // (d) Parse extent header from the first 12 bytes of data_area.
        // SAFETY: data_area is [u8; 176] (>= size_of::<EfsExtentHeader>() = 12);
        // read_unaligned has no alignment requirement.
        let hdr: EfsExtentHeader =
            unsafe { std::ptr::read_unaligned(inode.data_area.as_ptr() as *const EfsExtentHeader) };

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
        if hdr.depth != 0 {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Inode,
                message: format!(
                    "inode {ino}: extent tree depth {} != 0 (v1 only supports leaf nodes)",
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

        // (task 3.5) Validate each leaf extent.
        let hdr_size = std::mem::size_of::<EfsExtentHeader>();
        let ext_size = std::mem::size_of::<EfsExtent>();
        let mut collected: Vec<(u64, u16)> = Vec::with_capacity(hdr.entries as usize);
        let mut fatal = false;

        for i in 0..hdr.entries as usize {
            let off = hdr_size + i * ext_size;
            if off + ext_size > INODE_DATA_AREA_SIZE {
                // Should not happen given entries <= MAX_INLINE_EXTENTS, but guard.
                report.push(Finding {
                    severity: Severity::Error,
                    category: Category::Inode,
                    message: format!("inode {ino}: extent index {i} out of data_area bounds"),
                    fixable: false,
                    context: None,
                });
                fatal = true;
                break;
            }
            // SAFETY: the `off + size_of::<EfsExtent>() > data_area.len()` check
            // above guarantees the read stays in bounds; read_unaligned has no
            // alignment requirement.
            let ext: EfsExtent = unsafe {
                std::ptr::read_unaligned(inode.data_area[off..].as_ptr() as *const EfsExtent)
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
                fatal = true;
                continue;
            }

            // Out-of-bounds check.
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
                // Mark as unreferenceable but still record it if we can.
                // Don't seed bitmap for out-of-bounds extents.
                continue;
            }

            collected.push((phys, len));
        }

        if fatal {
            return None;
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
