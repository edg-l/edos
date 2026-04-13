use std::io;

use efs_common::{EfsSuperblock, JOURNAL_MAGIC, JournalSuperblock, journal_sb_checksum};

use crate::disk::Disk;
use crate::report::{Category, Finding, Report, Severity};

/// Load and validate the `JournalSuperblock` from the journal's first block.
///
/// Returns the parsed JSB and a report of any issues found.
/// CRC mismatch or bad magic return `Err` — these are unrecoverable without
/// `--force` and the caller must decide how to proceed.
pub fn load_jsb(disk: &mut Disk, sb: &EfsSuperblock) -> io::Result<(JournalSuperblock, Report)> {
    let journal_first_block = sb.journal_first_block;
    let jsb: JournalSuperblock = disk.read_struct_at(journal_first_block, 0)?;
    let mut report = Report::new();

    // Copy packed fields to locals to avoid unaligned reference UB.
    let magic = jsb.magic;
    let version = jsb.version;
    let crc32 = jsb.crc32;
    let jsb_block_size = jsb.block_size;
    let jsb_block_count = jsb.block_count;
    let head_seq = jsb.head_seq;
    let tail_seq = jsb.tail_seq;
    let fs_block_size = sb.block_size();
    let sb_journal_block_count = sb.journal_block_count;

    // Magic check: hard failure — cannot interpret the journal without it.
    if magic != JOURNAL_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "journal superblock magic mismatch: got {:#010x}, expected {:#010x}",
                magic, JOURNAL_MAGIC
            ),
        ));
    }

    // Version check.
    if version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported journal version: {version}"),
        ));
    }

    // CRC check: hard failure — a corrupt JSB cannot be safely replayed.
    let computed = journal_sb_checksum(&jsb);
    if crc32 != computed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "journal superblock CRC mismatch: on-disk {:#010x}, computed {:#010x}",
                crc32, computed
            ),
        ));
    }

    // Block size must match the filesystem block size.
    if jsb_block_size != fs_block_size {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Journal,
            message: format!(
                "journal block_size {jsb_block_size} does not match filesystem block_size {fs_block_size}"
            ),
            fixable: false,
            context: None,
        });
    }

    // block_count must not exceed the space reserved in the FS superblock.
    if jsb_block_count as u64 > sb_journal_block_count as u64 {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Journal,
            message: format!(
                "journal block_count {jsb_block_count} exceeds reserved journal_block_count {sb_journal_block_count}"
            ),
            fixable: false,
            context: None,
        });
    }

    // block_count == 0 would make the ring degenerate (ring_size underflow).
    if jsb_block_count == 0 {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Journal,
            message: "journal block_count is 0".to_string(),
            fixable: false,
            context: None,
        });
    }

    // head_seq must be >= tail_seq.
    if head_seq < tail_seq {
        report.push(Finding {
            severity: Severity::Error,
            category: Category::Journal,
            message: format!(
                "journal head_seq {head_seq} < tail_seq {tail_seq} (sequence invariant violated)"
            ),
            fixable: false,
            context: None,
        });
    }

    Ok((jsb, report))
}
