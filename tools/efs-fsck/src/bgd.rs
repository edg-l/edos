use std::io;

use efs_common::{EfsBlockGroupDesc, EfsSuperblock, checksum_block_group_desc};

use crate::disk::Disk;
use crate::report::{Category, Finding, Report, Severity};

/// Load all block group descriptors from block 2 onward and verify them.
///
/// Returns the parsed BGDs and a report of any issues found.
pub fn load_and_verify(
    disk: &mut Disk,
    sb: &EfsSuperblock,
) -> io::Result<(Vec<EfsBlockGroupDesc>, Report)> {
    let mut report = Report::new();
    let count = sb.block_group_count as usize;
    let bgd_size = std::mem::size_of::<EfsBlockGroupDesc>(); // 64 bytes
    let block_size = sb.block_size() as usize;

    let mut bgds = Vec::with_capacity(count);

    // BGDs are stored consecutively starting at block 2.
    let mut block_num: u64 = 2;
    let mut offset_in_block: usize = 0;

    for i in 0..count {
        // Advance to next block if the current BGD would cross a block boundary.
        if offset_in_block + bgd_size > block_size {
            block_num += 1;
            offset_in_block = 0;
        }

        let bgd: EfsBlockGroupDesc = disk.read_struct_at(block_num, offset_in_block)?;

        // Verify per-BGD checksum.
        let expected = checksum_block_group_desc(&bgd);
        if bgd.checksum != expected {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::Bgd,
                message: format!(
                    "block group {} checksum mismatch: on-disk {:#010x}, computed {:#010x}",
                    i, bgd.checksum, expected
                ),
                fixable: true,
                context: None,
            });
        }

        bgds.push(bgd);
        offset_in_block += bgd_size;
    }

    // Cross-check summed free counts against superblock totals.
    let sum_free_blocks: u64 = bgds.iter().map(|b| b.free_blocks_count).sum();
    let sum_free_inodes: u64 = bgds.iter().map(|b| b.free_inodes_count).sum();

    let sb_free_blocks = sb.free_blocks;
    let sb_free_inodes = sb.free_inodes;

    if sum_free_blocks != sb_free_blocks {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Bgd,
            message: format!(
                "BGD free_blocks sum {} != superblock free_blocks {}",
                sum_free_blocks, sb_free_blocks
            ),
            fixable: true,
            context: None,
        });
    }

    if sum_free_inodes != sb_free_inodes {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Bgd,
            message: format!(
                "BGD free_inodes sum {} != superblock free_inodes {}",
                sum_free_inodes, sb_free_inodes
            ),
            fixable: true,
            context: None,
        });
    }

    Ok((bgds, report))
}
