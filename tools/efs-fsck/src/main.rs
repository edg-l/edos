mod bgd;
mod bitmaps;
mod cli;
mod dirtree;
mod disk;
mod exit_code;
mod journal;
mod layout;
mod repair;
mod replay;
mod report;
mod scan;
mod superblock;

use std::collections::HashMap;
use std::process;

use cli::parse_args;
use disk::Disk;
use efs_common::INCOMPAT_JOURNAL;
use exit_code::FsckExitCode;
use report::{Category, Finding, Report, Severity};

fn main() {
    let args = parse_args();

    // Open the disk image (read-only unless --repair).
    let mut disk = Disk::open(&args.image, args.repair, args.partition_offset, 4096)
        .unwrap_or_else(|e| {
            eprintln!("failed to open '{}': {e}", args.image.display());
            process::exit(FsckExitCode::OperationalError.code());
        });

    // --- Phase 1: Superblock ---
    let (sb, sb_report) = superblock::load_and_verify(&mut disk).unwrap_or_else(|e| {
        eprintln!("superblock read failed: {e}");
        process::exit(FsckExitCode::OperationalError.code());
    });

    // Check for fsck_in_progress sentinel before proceeding.
    let has_sentinel = sb_report.findings.iter().any(|f| {
        f.category == Category::Superblock && f.message.starts_with("fsck_in_progress sentinel set")
    });
    if has_sentinel && !args.force {
        eprintln!(
            "error: previous fsck did not complete cleanly (fsck_in_progress sentinel set). \
             Run with --force to override."
        );
        process::exit(FsckExitCode::OperationalError.code());
    }

    // Now that we have a valid superblock, re-open with the correct block size.
    let block_size = sb.block_size();
    drop(disk);
    let mut disk = Disk::open(&args.image, args.repair, args.partition_offset, block_size)
        .unwrap_or_else(|e| {
            eprintln!("failed to reopen '{}': {e}", args.image.display());
            process::exit(FsckExitCode::OperationalError.code());
        });

    // --- Phase 1: BGDs ---
    let (mut bgds, bgd_report) = bgd::load_and_verify(&mut disk, &sb).unwrap_or_else(|e| {
        eprintln!("BGD read failed: {e}");
        process::exit(FsckExitCode::OperationalError.code());
    });

    // --- Phase 2: Journal ---
    let mut journal_report = Report::new();
    // Counts as one "errors fixed" outcome for exit-code purposes if a dirty
    // journal was replayed in --repair mode.
    let mut journal_replayed: u32 = 0;

    if sb.incompatible_features & INCOMPAT_JOURNAL != 0 {
        match journal::load_jsb(&mut disk, &sb) {
            Err(e) => {
                if args.force {
                    journal_report.push(Finding {
                        severity: Severity::Warning,
                        category: Category::Journal,
                        message: format!(
                            "journal superblock invalid: {e}; skipping replay (--force)"
                        ),
                        fixable: false,
                        context: None,
                    });
                } else {
                    journal_report.push(Finding {
                        severity: Severity::Error,
                        category: Category::Journal,
                        message: format!("journal superblock invalid: {e}"),
                        fixable: false,
                        context: Some("use --force to skip journal replay".to_string()),
                    });
                }
            }
            Ok((jsb, jsb_report)) => {
                let jsb_has_errors = jsb_report.has_errors();
                for f in jsb_report.findings {
                    journal_report.push(f);
                }

                // Copy packed fields to locals before use.
                let tail_seq = jsb.tail_seq;
                let head_seq = jsb.head_seq;

                if jsb_has_errors {
                    journal_report.push(Finding {
                        severity: Severity::Warning,
                        category: Category::Journal,
                        message: "skipping replay due to JSB validation errors".to_string(),
                        fixable: false,
                        context: None,
                    });
                } else if tail_seq != head_seq {
                    if args.repair {
                        match replay::replay(&mut disk, &sb) {
                            Ok(result) => {
                                journal_replayed = 1;
                                journal_report.push(Finding {
                                    severity: Severity::Info,
                                    category: Category::Journal,
                                    message: format!(
                                        "replayed {} transaction(s), wrote {} block(s)",
                                        result.tx_count, result.blocks_written
                                    ),
                                    fixable: false,
                                    context: None,
                                });
                            }
                            Err(e) => {
                                journal_report.push(Finding {
                                    severity: Severity::Error,
                                    category: Category::Journal,
                                    message: format!("journal replay failed: {e}"),
                                    fixable: false,
                                    context: None,
                                });
                            }
                        }
                    } else {
                        journal_report.push(Finding {
                            severity: Severity::Error,
                            category: Category::Journal,
                            message: "journal is dirty; run with --repair to replay".to_string(),
                            fixable: true,
                            context: Some(format!("tail_seq={tail_seq} head_seq={head_seq}")),
                        });
                    }
                }
            }
        }
    }

    // --- Phase 3: Inode scan + bitmap rebuild ---
    let mut scan_report = Report::new();

    let inode_infos = scan::scan_all_inodes(&mut disk, &sb, &bgds, &mut scan_report)
        .unwrap_or_else(|e| {
            eprintln!("inode scan failed: {e}");
            process::exit(FsckExitCode::OperationalError.code());
        });

    // Copy packed fields to locals.
    let total_blocks = sb.total_blocks;
    let total_inodes = sb.total_inodes;
    let block_group_count = sb.block_group_count as usize;
    let blocks_per_group = sb.blocks_per_group as u64;
    let inodes_per_group = sb.inodes_per_group as u64;

    let mut block_bm = bitmaps::RebuiltBitmap::new(total_blocks);
    let mut inode_bm = bitmaps::RebuiltBitmap::new(total_inodes);

    bitmaps::seed_static_metadata(&mut block_bm, &sb, &bgds);
    bitmaps::apply_inode_info(&mut block_bm, &mut inode_bm, &inode_infos, &mut scan_report);

    // Per-group bitmap comparison.
    for (g, bgd) in bgds.iter().enumerate() {
        let group_block_start = g as u64 * blocks_per_group;
        let group_block_end = (group_block_start + blocks_per_group).min(total_blocks);
        let group_block_len = group_block_end - group_block_start;

        let group_inode_start = g as u64 * inodes_per_group;
        let group_inode_end = (group_inode_start + inodes_per_group).min(total_inodes);
        let group_inode_len = group_inode_end - group_inode_start;

        // Read on-disk bitmaps (one block each).
        let disk_block_bm = disk.read_block(bgd.block_bitmap_block).unwrap_or_else(|e| {
            eprintln!("failed to read block bitmap for group {g}: {e}");
            process::exit(FsckExitCode::OperationalError.code());
        });
        let disk_inode_bm = disk.read_block(bgd.inode_bitmap_block).unwrap_or_else(|e| {
            eprintln!("failed to read inode bitmap for group {g}: {e}");
            process::exit(FsckExitCode::OperationalError.code());
        });

        bitmaps::compare(
            &block_bm,
            &disk_block_bm,
            group_block_start,
            group_block_len,
            Category::BlockBitmap,
            &mut scan_report,
        );

        bitmaps::compare(
            &inode_bm,
            &disk_inode_bm,
            group_inode_start,
            group_inode_len,
            Category::InodeBitmap,
            &mut scan_report,
        );

        if args.verbose {
            let inode_count = inode_infos
                .iter()
                .filter(|info| {
                    let idx = info.ino - 1;
                    idx >= group_inode_start && idx < group_inode_end
                })
                .count();
            let block_count = inode_infos
                .iter()
                .filter(|info| {
                    let idx = info.ino - 1;
                    idx >= group_inode_start && idx < group_inode_end
                })
                .map(|info| info.extents.iter().map(|&(_, l)| l as u64).sum::<u64>())
                .sum::<u64>();
            println!("group {g}: {inode_count} inodes, {block_count} blocks");
        }
    }

    let _ = block_group_count; // used via bgds.iter().enumerate()

    // --- Phase 4: Directory tree + link counts ---
    let mut dirtree_report = Report::new();

    // Build a map from inode number to &InodeInfo for O(1) lookup.
    let infos_map: HashMap<u64, &bitmaps::InodeInfo> =
        inode_infos.iter().map(|info| (info.ino, info)).collect();

    let observed_links = dirtree::walk(&mut disk, &sb, &bgds, &infos_map, &mut dirtree_report)
        .unwrap_or_else(|e| {
            eprintln!("directory tree walk failed: {e}");
            process::exit(FsckExitCode::OperationalError.code());
        });

    dirtree::check_link_counts(&infos_map, &observed_links, &mut dirtree_report);

    // Merge all reports.
    let mut full_report = Report::new();
    for f in sb_report.findings {
        full_report.push(f);
    }
    for f in bgd_report.findings {
        full_report.push(f);
    }
    for f in journal_report.findings {
        full_report.push(f);
    }
    for f in scan_report.findings {
        full_report.push(f);
    }
    for f in dirtree_report.findings {
        full_report.push(f);
    }

    // Snapshot the error count before any repair so we can compute the
    // post-repair "remaining" count as (initial - succeeded). Journal replay
    // happened earlier (in the --repair branch above) and emits an Info
    // finding rather than an Error, so we add it to the initial-error count
    // explicitly so the exit code can report ErrorsFixed.
    let initial_errors =
        repair::count_error_findings(&full_report.findings) + journal_replayed as usize;

    // --- Phase 5: Repair ---
    let mut repair_succeeded: u32 = journal_replayed;
    if args.repair {
        let fixable_count = full_report.findings.iter().filter(|f| f.fixable).count();

        if fixable_count > 0 {
            let mut sb_mut = sb;

            match repair::apply_repairs(
                &mut disk,
                &mut sb_mut,
                &mut bgds,
                &mut full_report.findings,
                args.yes,
                &observed_links,
                &inode_infos,
            ) {
                Ok(stats) => {
                    repair_succeeded += stats.succeeded;
                    if stats.succeeded > 0 {
                        println!(
                            "repair: {}/{} fixes applied ({} skipped)",
                            stats.succeeded, stats.attempted, stats.skipped
                        );
                    }
                }
                Err(e) => {
                    eprintln!("repair failed: {e}");
                    full_report.print(args.verbose);
                    process::exit(FsckExitCode::OperationalError.code());
                }
            }
        }
    }

    full_report.print(args.verbose);

    let exit_code = compute_exit_code(initial_errors, repair_succeeded, args.repair);
    process::exit(exit_code.code());
}

/// Compute the final exit code from pre-repair error count and repair outcome.
///
/// Each successful repair pass resolves exactly one Error-severity finding, so
/// `remaining = initial_errors - repair_succeeded`.
fn compute_exit_code(initial_errors: usize, repair_succeeded: u32, repair: bool) -> FsckExitCode {
    let remaining = initial_errors.saturating_sub(repair_succeeded as usize);

    if remaining > 0 {
        FsckExitCode::ErrorsRemain
    } else if repair && initial_errors > 0 {
        FsckExitCode::ErrorsFixed
    } else {
        FsckExitCode::Clean
    }
}
