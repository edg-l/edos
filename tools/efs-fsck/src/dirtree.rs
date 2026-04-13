use std::collections::{HashMap, HashSet, VecDeque};
use std::io;

use efs_common::{
    DIR_ENTRY_HEADER_SIZE, EFS_ROOT_INO, EXTENT_MAGIC, EfsBlockGroupDesc, EfsDirEntryHeader,
    EfsExtent, EfsExtentHeader, EfsInode, EfsSuperblock, FT_DIR, INODE_DATA_AREA_SIZE,
    INODE_FLAG_INLINE_DATA, MAX_INLINE_EXTENTS, dir_entry_min_size,
};

use crate::bitmaps::InodeInfo;
use crate::disk::Disk;
use crate::report::{Category, Finding, Report, Severity};

// ---------------------------------------------------------------------------
// Inode I/O helpers
// ---------------------------------------------------------------------------

/// Read an inode by number from the inode table.
///
/// Mirrors the lookup arithmetic used in `scan::scan_all_inodes`.
fn read_inode_bytes(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
    ino: u64,
) -> io::Result<EfsInode> {
    // Copy packed fields.
    let inodes_per_group = sb.inodes_per_group as u64;
    let inode_size = sb.inode_size as usize;
    let block_size = disk.block_size as usize;

    let group = ((ino - 1) / inodes_per_group) as usize;
    let local_idx = (ino - 1) % inodes_per_group;
    let bgd = &bgds[group];

    let byte_offset = local_idx as usize * inode_size;
    let block = bgd.inode_table_block + (byte_offset / block_size) as u64;
    let offset_in_block = byte_offset % block_size;

    disk.read_struct_at(block, offset_in_block)
}

/// Translate a logical block index for `inode` to physical block data.
///
/// Returns `Some(block_data)` when the logical block is covered by an extent,
/// or `None` for sparse / past-eof regions. Inline-data inodes are not called
/// here (the caller uses `inode.inline_data()` instead).
fn read_inode_data_block(
    disk: &mut Disk,
    inode: &EfsInode,
    logical_block: u32,
) -> io::Result<Option<Vec<u8>>> {
    let hdr_size = std::mem::size_of::<EfsExtentHeader>();
    let ext_size = std::mem::size_of::<EfsExtent>();

    // SAFETY: data_area is [u8; 176] >= size_of::<EfsExtentHeader>() = 12;
    // read_unaligned requires no alignment.
    let hdr: EfsExtentHeader =
        unsafe { std::ptr::read_unaligned(inode.data_area.as_ptr() as *const EfsExtentHeader) };

    if hdr.magic != EXTENT_MAGIC {
        return Ok(None);
    }
    let entries = (hdr.entries as usize).min(MAX_INLINE_EXTENTS);

    for i in 0..entries {
        let off = hdr_size + i * ext_size;
        if off + ext_size > INODE_DATA_AREA_SIZE {
            break;
        }
        // SAFETY: off + size_of::<EfsExtent>() <= data_area.len() checked above;
        // read_unaligned has no alignment requirement.
        let ext: EfsExtent = unsafe {
            std::ptr::read_unaligned(inode.data_area[off..].as_ptr() as *const EfsExtent)
        };

        let logical_start = ext.logical_block;
        let length = ext.length;
        if length == 0 {
            continue;
        }
        if logical_block >= logical_start && logical_block < logical_start + length as u32 {
            let phys = ext.physical_start() + (logical_block - logical_start) as u64;
            return disk.read_block(phys).map(Some);
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Directory block parser
// ---------------------------------------------------------------------------

/// Parse all valid directory entries from a raw block.
///
/// Returns `(header_copy, name_bytes)` for each live entry found. On any
/// structural violation the entry is reported and parsing of THIS block stops
/// (but the caller continues with other blocks).
fn parse_dir_block(
    block: &[u8],
    block_size: u32,
    dir_ino: u64,
    logical_block: u64,
    report: &mut Report,
) -> Vec<(EfsDirEntryHeader, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut offset: usize = 0;
    let bsz = block_size as usize;

    while offset < bsz {
        if offset + DIR_ENTRY_HEADER_SIZE > bsz {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!(
                    "dir inode {dir_ino} logical_block {logical_block}: truncated entry header at offset {offset}"
                ),
                fixable: false,
                context: None,
            });
            break;
        }

        // SAFETY: offset + DIR_ENTRY_HEADER_SIZE <= block.len() checked above;
        // EfsDirEntryHeader is #[repr(C, packed)] so read_unaligned is correct.
        let hdr: EfsDirEntryHeader = unsafe {
            std::ptr::read_unaligned(block[offset..].as_ptr() as *const EfsDirEntryHeader)
        };

        // Copy packed fields to locals before any arithmetic.
        let inode = hdr.inode;
        let rec_len = hdr.rec_len;
        let name_len = hdr.name_len;
        let file_type = hdr.file_type;

        if rec_len == 0 {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!(
                    "dir inode {dir_ino} logical_block {logical_block}: rec_len=0 at offset {offset}"
                ),
                fixable: false,
                context: None,
            });
            break;
        }

        let min_len = dir_entry_min_size(name_len);
        if rec_len < min_len {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!(
                    "dir inode {dir_ino} logical_block {logical_block}: rec_len={rec_len} < min {min_len} at offset {offset}"
                ),
                fixable: false,
                context: None,
            });
            break;
        }

        if rec_len % 4 != 0 {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!(
                    "dir inode {dir_ino} logical_block {logical_block}: rec_len={rec_len} not 4-byte aligned at offset {offset}"
                ),
                fixable: false,
                context: None,
            });
            break;
        }

        let end = offset + rec_len as usize;
        if end > bsz {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!(
                    "dir inode {dir_ino} logical_block {logical_block}: rec_len={rec_len} at offset {offset} exceeds block"
                ),
                fixable: false,
                context: None,
            });
            break;
        }

        offset = end;

        // Skip deleted entries (inode == 0).
        if inode == 0 {
            continue;
        }

        let name_start = offset - rec_len as usize + DIR_ENTRY_HEADER_SIZE;
        let name_end = name_start + name_len as usize;
        if name_end > block.len() {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!(
                    "dir inode {dir_ino} logical_block {logical_block}: name_len={name_len} at offset {name_start} exceeds block"
                ),
                fixable: false,
                context: None,
            });
            break;
        }
        let name = block[name_start..name_end].to_vec();

        // Reconstruct the header copy with the copied fields.
        let hdr_copy = EfsDirEntryHeader {
            inode,
            rec_len,
            name_len,
            file_type,
        };
        entries.push((hdr_copy, name));
    }

    entries
}

// ---------------------------------------------------------------------------
// BFS directory tree walk
// ---------------------------------------------------------------------------

/// Walk the directory tree starting from the root inode.
///
/// Returns a map of `inode_number -> observed_link_count` across all directory
/// entries seen. Directories that form cycles (other than via `..`) are
/// reported and not recursed into.
pub fn walk(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
    infos: &HashMap<u64, &InodeInfo>,
    report: &mut Report,
) -> io::Result<HashMap<u64, u32>> {
    let block_size = disk.block_size as u64;
    let total_inodes = sb.total_inodes;

    // observed_links: inode -> how many directory entries point to it.
    let mut observed_links: HashMap<u64, u32> = HashMap::new();
    // visited: inodes we have already entered (for cycle detection on dirs).
    let mut visited: HashSet<u64> = HashSet::new();

    // Stats for verbose Info finding.
    let mut stat_dirs: u64 = 0;
    let mut stat_files: u64 = 0;
    let mut stat_symlinks: u64 = 0;

    // BFS queue: (dir_ino, parent_ino).
    let mut queue: VecDeque<(u64, u64)> = VecDeque::new();
    queue.push_back((EFS_ROOT_INO, EFS_ROOT_INO));

    while let Some((ino, parent_ino)) = queue.pop_front() {
        if visited.contains(&ino) {
            continue;
        }
        visited.insert(ino);
        stat_dirs += 1;

        let inode = match read_inode_bytes(disk, sb, bgds, ino) {
            Ok(i) => i,
            Err(e) => {
                report.push(Finding {
                    severity: Severity::Error,
                    category: Category::DirTree,
                    message: format!("dir inode {ino}: failed to read inode: {e}"),
                    fixable: false,
                    context: None,
                });
                continue;
            }
        };

        // Collect all directory entries from this inode.
        let is_inline = inode.flags & INODE_FLAG_INLINE_DATA != 0;
        let dir_entries: Vec<(EfsDirEntryHeader, Vec<u8>)> = if is_inline {
            let data = inode.inline_data();
            parse_dir_block(data, data.len() as u32, ino, 0, report)
        } else {
            let size = inode.size;
            let num_blocks = size.div_ceil(block_size) as u32;
            let mut all_entries = Vec::new();
            for lb in 0..num_blocks {
                match read_inode_data_block(disk, &inode, lb) {
                    Ok(Some(block_data)) => {
                        // Use the logical block number for diagnostics; physical
                        // block is not threaded back from read_inode_data_block.
                        let parsed =
                            parse_dir_block(&block_data, disk.block_size, ino, lb as u64, report);
                        all_entries.extend(parsed);
                    }
                    Ok(None) => {
                        // Sparse block — no entries.
                    }
                    Err(e) => {
                        report.push(Finding {
                            severity: Severity::Error,
                            category: Category::DirTree,
                            message: format!(
                                "dir inode {ino}: I/O error reading logical block {lb}: {e}"
                            ),
                            fixable: false,
                            context: None,
                        });
                    }
                }
            }
            all_entries
        };

        // Process each entry.
        for (hdr, name_bytes) in &dir_entries {
            let entry_ino = hdr.inode;
            let file_type = hdr.file_type;
            let name = String::from_utf8_lossy(name_bytes);
            let is_dot = name_bytes == b".";
            let is_dotdot = name_bytes == b"..";

            // --- Task 4.4: Validate . and .. targets ---
            if is_dot {
                if entry_ino != ino {
                    report.push(Finding {
                        severity: Severity::Error,
                        category: Category::DirTree,
                        message: format!(
                            "dir inode {ino}: '.' points to inode {entry_ino}, expected {ino}"
                        ),
                        fixable: false,
                        context: None,
                    });
                }
                // Count the link regardless.
                *observed_links.entry(entry_ino).or_insert(0) += 1;
                continue;
            }

            if is_dotdot {
                if entry_ino != parent_ino {
                    report.push(Finding {
                        severity: Severity::Error,
                        category: Category::DirTree,
                        message: format!(
                            "dir inode {ino}: '..' points to inode {entry_ino}, expected parent {parent_ino}"
                        ),
                        fixable: false,
                        context: None,
                    });
                }
                *observed_links.entry(entry_ino).or_insert(0) += 1;
                continue;
            }

            // --- Task 4.4: Validate entry inode is in range and allocated ---
            if entry_ino < 1 || entry_ino > total_inodes {
                report.push(Finding {
                    severity: Severity::Error,
                    category: Category::DirTree,
                    message: format!(
                        "dir inode {ino}: entry '{}' points to out-of-range inode {entry_ino}",
                        name
                    ),
                    fixable: false,
                    context: None,
                });
                continue;
            }

            if !infos.contains_key(&entry_ino) {
                report.push(Finding {
                    severity: Severity::Error,
                    category: Category::DirTree,
                    message: format!(
                        "dir inode {ino}: entry '{}' points to unallocated inode {entry_ino}",
                        name
                    ),
                    fixable: false,
                    context: None,
                });
                continue;
            }

            // Count observed link.
            *observed_links.entry(entry_ino).or_insert(0) += 1;

            // --- Task 4.5: Cycle detection for subdirectories ---
            if file_type == FT_DIR {
                if visited.contains(&entry_ino) {
                    report.push(Finding {
                        severity: Severity::Error,
                        category: Category::DirTree,
                        message: format!(
                            "directory cycle: inode {ino} entry '{}' points to already-visited inode {entry_ino}",
                            name
                        ),
                        fixable: false,
                        context: None,
                    });
                    // Do not recurse.
                    continue;
                }
                queue.push_back((entry_ino, ino));
            } else if let Some(info) = infos.get(&entry_ino) {
                if info.is_dir {
                    // file_type hint says not-dir but the inode mode says dir;
                    // still enqueue but note the inconsistency.
                    if !visited.contains(&entry_ino) {
                        queue.push_back((entry_ino, ino));
                    }
                } else if info.mode & efs_common::S_IFMT == efs_common::S_IFLNK {
                    stat_symlinks += 1;
                } else {
                    stat_files += 1;
                }
            }
        }
    }

    // Verbose Info summary.
    report.push(Finding {
        severity: Severity::Info,
        category: Category::DirTree,
        message: format!(
            "dir-tree walk: {stat_dirs} directories, {stat_files} files, {stat_symlinks} symlinks"
        ),
        fixable: false,
        context: None,
    });

    Ok(observed_links)
}

// ---------------------------------------------------------------------------
// Link count reconciliation
// ---------------------------------------------------------------------------

/// Compare on-disk link counts against observed directory-entry counts.
///
/// Mismatches are reported as fixable errors. Allocated inodes with zero
/// observed links (orphans) are reported with context `"destructive"` so the
/// repair phase can prompt before deleting.
pub fn check_link_counts(
    infos: &HashMap<u64, &InodeInfo>,
    observed: &HashMap<u64, u32>,
    report: &mut Report,
) {
    let mut inos: Vec<u64> = infos.keys().copied().collect();
    inos.sort_unstable();

    for ino in inos {
        let info = infos[&ino];
        let expected = info.link_count;
        let obs = observed.get(&ino).copied().unwrap_or(0);

        if obs == 0 {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!("orphan inode {ino} (reachable link count 0)"),
                fixable: true,
                context: Some("destructive".to_string()),
            });
        } else if obs != expected as u32 {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!("inode {ino} link_count on-disk={expected} observed={obs}"),
                fixable: true,
                context: None,
            });
        }
    }
}
