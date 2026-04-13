use std::io;

use efs_common::{EfsSuperblock, INCOMPAT_JOURNAL, checksum_superblock};

use crate::disk::Disk;
use crate::report::{Category, Finding, Report, Severity};

/// All incompatible feature bits known by this version of fsck.
const KNOWN_INCOMPAT_FEATURES: u64 = INCOMPAT_JOURNAL;

/// Load the primary superblock from block 1 and verify its fields.
///
/// Returns the parsed superblock and a report of any issues found.
/// Only hard structural failures (bad magic, unsupported version) return `Err`.
pub fn load_and_verify(disk: &mut Disk) -> io::Result<(EfsSuperblock, Report)> {
    let sb: EfsSuperblock = disk.read_struct_at(1, 0)?;
    let mut report = Report::new();

    // Hard structural checks via sb.validate().
    if let Err(e) = sb.validate() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, e));
    }

    // CRC check: warn but continue (spec says warn, not fatal).
    let expected = checksum_superblock(&sb);
    let on_disk_checksum = sb.checksum;
    if on_disk_checksum != expected {
        report.push(Finding {
            severity: Severity::Warning,
            category: Category::Superblock,
            message: format!(
                "superblock checksum mismatch: on-disk {:#010x}, computed {:#010x}",
                on_disk_checksum, expected
            ),
            fixable: true,
            context: None,
        });
    }

    // Unknown incompatible feature bits: error, cannot safely check.
    let incompat = sb.incompatible_features;
    let unknown_incompat = incompat & !KNOWN_INCOMPAT_FEATURES;
    if unknown_incompat != 0 {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Superblock,
            message: format!(
                "unknown incompatible feature bits: {:#018x}",
                unknown_incompat
            ),
            fixable: false,
            context: Some("fsck cannot safely proceed with unknown incompat features".to_string()),
        });
    }

    // total_blocks * block_size must not exceed device size.
    let block_size = sb.block_size() as u64;
    let total_blocks = sb.total_blocks;
    let required_bytes = total_blocks.saturating_mul(block_size);
    let device_bytes = disk.file_size()?;
    let available = device_bytes.saturating_sub(disk.partition_offset);
    if required_bytes > available {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Superblock,
            message: format!(
                "filesystem claims {} blocks ({} bytes) but partition has only {} bytes available",
                total_blocks, required_bytes, available
            ),
            fixable: false,
            context: None,
        });
    }

    // fsck_in_progress sentinel check.
    let fsck_in_progress = sb.fsck_in_progress;
    if fsck_in_progress != 0 {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Superblock,
            message: "fsck_in_progress sentinel set: previous fsck did not complete cleanly"
                .to_string(),
            fixable: false,
            context: Some("run with --force to override".to_string()),
        });
    }

    Ok((sb, report))
}
