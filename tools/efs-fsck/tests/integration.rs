/// Integration tests for efs-fsck.
///
/// Each test is self-contained: it creates a fresh EFS image via efs-mkfs,
/// optionally corrupts it using helpers from `common`, runs the fsck binary,
/// and asserts on exit code and output.  Temp files are cleaned up via
/// `TempImage`'s Drop impl.
///
/// Requirements:
///   - `efs-mkfs` binary must be built (`cargo build --release` in tools/efs-mkfs)
///   - `efs-fsck` binary must be built (`cargo build --release` in tools/efs-fsck)
///
/// Run with: cargo test --release --manifest-path tools/efs-fsck/Cargo.toml
mod common;

use common::{
    TempImage, build_fixture_clean, corrupt_block_bitmap_bit, corrupt_link_count, dirty_jsb,
    force_dirty_journal, fsck_bin, mkfs_bin, read_jsb_seqs, read_jsb_seqs_full,
    read_sb_journal_info, run_fsck, scribble_journal_commit,
};

use std::process::Command;

// ---- Task 6.2: clean image exits 0 -----------------------------------------

/// A freshly created image with no corruption must exit 0 and report no issues.
///
/// Non-verbose mode only prints WARN/ERROR findings; a clean image has none,
/// so stdout is empty.  We assert exit 0 and that no WARN/ERROR lines appear.
#[test]
fn clean_image_exits_0() {
    let img = build_fixture_clean("efs_fsck_test_clean.img");
    let (code, stdout, stderr) = run_fsck(&img.path, &["--verbose"]);
    assert_eq!(
        code, 0,
        "expected exit 0 on a clean image; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    // No WARN or ERROR lines should appear on a clean image.
    let has_problem = stdout
        .lines()
        .any(|l| l.starts_with("[WARN]") || l.starts_with("[ERROR]"));
    assert!(
        !has_problem,
        "expected no WARN/ERROR findings on a clean image; got:\n{stdout}"
    );
}

// ---- Task 6.3: leaked block bitmap bit -------------------------------------

/// Corrupt a block bitmap bit for an unreferenced block and verify fsck detects
/// it without --repair: exit 4 (errors remain), stdout reports the leaked bit.
///
/// Block 500 is in the free data area of a 32M image (metadata ends ~block 263,
/// journal starts at block 7168).
#[test]
fn leaked_block_reported() {
    let img = build_fixture_clean("efs_fsck_test_leaked_block_reported.img");

    corrupt_block_bitmap_bit(&img.path, 0, 500).expect("corrupt_block_bitmap_bit failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 4,
        "expected exit 4 (errors remain) for leaked block; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("leaked block-bitmap bit at 500"),
        "expected leaked block-bitmap finding in stdout; got:\n{stdout}"
    );
}

/// Corrupt a block bitmap bit, repair it with --repair --yes (exit 1), then
/// verify the second run is clean (exit 0, no WARN/ERROR).
#[test]
fn leaked_block_repaired() {
    let img = build_fixture_clean("efs_fsck_test_leaked_block_repaired.img");

    corrupt_block_bitmap_bit(&img.path, 0, 500).expect("corrupt_block_bitmap_bit failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &["--repair", "--yes"]);
    assert_eq!(
        code, 1,
        "expected exit 1 (errors fixed) after --repair; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    let (code2, stdout2, stderr2) = run_fsck(&img.path, &["--verbose"]);
    assert_eq!(
        code2, 0,
        "expected exit 0 on second run after leaked-block repair; got {code2}\nstdout: {stdout2}\nstderr: {stderr2}"
    );
    let has_problem = stdout2
        .lines()
        .any(|l| l.starts_with("[WARN]") || l.starts_with("[ERROR]"));
    assert!(
        !has_problem,
        "expected no WARN/ERROR findings on second run after repair; got:\n{stdout2}"
    );
}

// ---- Task 6.4: bad link count -----------------------------------------------

/// Corrupt root inode's link_count and verify fsck detects the mismatch
/// (exit 4, "link_count on-disk=99" in output).
///
/// Root inode is inode 1 on EFS (EFS_ROOT_INO = 1).
#[test]
fn bad_link_count_reported() {
    let img = build_fixture_clean("efs_fsck_test_lc_reported.img");

    corrupt_link_count(&img.path, 0, 1, 99).expect("corrupt_link_count failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 4,
        "expected exit 4 for bad link count; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("link_count on-disk=99"),
        "expected 'link_count on-disk=99' in stdout; got:\n{stdout}"
    );
}

/// Corrupt root inode's link_count, repair, then verify the second run is clean.
#[test]
fn bad_link_count_repaired() {
    let img = build_fixture_clean("efs_fsck_test_lc_repaired.img");

    corrupt_link_count(&img.path, 0, 1, 99).expect("corrupt_link_count failed");

    // First run with repair.
    let (code, stdout, stderr) = run_fsck(&img.path, &["--repair", "--yes"]);
    assert_eq!(
        code, 1,
        "expected exit 1 (errors fixed) after --repair; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Second run must be clean: exit 0 and no WARN/ERROR lines.
    let (code2, stdout2, stderr2) = run_fsck(&img.path, &["--verbose"]);
    assert_eq!(
        code2, 0,
        "expected exit 0 on second run after link-count repair; got {code2}\nstdout: {stdout2}\nstderr: {stderr2}"
    );
    let has_problem = stdout2
        .lines()
        .any(|l| l.starts_with("[WARN]") || l.starts_with("[ERROR]"));
    assert!(
        !has_problem,
        "expected no WARN/ERROR findings on second run after repair; got:\n{stdout2}"
    );
}

// ---- Task 6.5: partial transaction discarded --------------------------------

/// Verify fsck handles a corrupted JSB checksum gracefully.
///
/// Testing true mid-ring commit corruption (a partially-written commit block
/// inside the journal ring) requires a kernel-generated image that actually
/// wrote transactions.  efs-mkfs produces a clean image with no transactions,
/// so there is no commit block to corrupt.  This test is therefore marked
/// `#[ignore]`; it can be enabled once a test fixture with real kernel
/// transactions is available.
#[test]
#[ignore = "requires kernel-generated image with real journal transactions; \
            scribble_journal_commit only corrupts the JSB checksum, not a \
            mid-ring commit block"]
fn partial_tx_discarded() {
    let img = build_fixture_clean("efs_fsck_test_partial_tx.img");

    scribble_journal_commit(&img.path, 0).expect("scribble_journal_commit failed");

    // fsck should detect the invalid JSB and report an error.
    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 4,
        "expected exit 4 for corrupted JSB; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("journal") || stderr.contains("journal"),
        "expected journal-related message; stdout: {stdout}\nstderr: {stderr}"
    );
}

// ---- Task 6.6: dirty journal ------------------------------------------------

/// A dirty journal (tail_seq != head_seq, no real txs) without --repair must
/// emit an Error and exit 4.
#[test]
fn dirty_journal_without_repair() {
    let img = build_fixture_clean("efs_fsck_test_dirty_journal_ro.img");

    force_dirty_journal(&img.path, 0).expect("force_dirty_journal failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 4,
        "expected exit 4 for dirty journal in read-only mode; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("journal is dirty"),
        "expected 'journal is dirty' in stdout; got:\n{stdout}"
    );
}

/// A dirty journal with --repair --yes must exit 1 (ErrorsFixed); a second run
/// must exit 0 (journal clean).
#[test]
fn dirty_journal_with_repair() {
    let img = build_fixture_clean("efs_fsck_test_dirty_journal_repair.img");

    force_dirty_journal(&img.path, 0).expect("force_dirty_journal failed");

    let (tail_before, head_before) = read_jsb_seqs(&img.path, 0).expect("read_jsb_seqs failed");
    assert_ne!(
        tail_before, head_before,
        "journal should be dirty after patching"
    );

    let (code, stdout, stderr) = run_fsck(&img.path, &["--repair", "--yes"]);
    assert_eq!(
        code, 1,
        "expected exit 1 (errors fixed) after journal --repair; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Second run: must be clean (journal was actually replayed).
    let (code2, stdout2, stderr2) = run_fsck(&img.path, &[]);
    assert_eq!(
        code2, 0,
        "expected exit 0 on second run after journal repair; got {code2}\nstdout: {stdout2}\nstderr: {stderr2}"
    );
    assert!(
        !stdout2.contains("journal is dirty"),
        "second run should not report dirty journal; got:\n{stdout2}"
    );
}

// ---- Existing test (preserved) ---------------------------------------------

/// Verify that running fsck --repair on a dirty-journal image resets the JSB,
/// and that a second run sees a clean journal (tail_seq == head_seq).
#[test]
fn test_journal_replay_idempotent() {
    let mkfs = mkfs_bin();
    let fsck = fsck_bin();
    if !mkfs.exists() {
        eprintln!("SKIP: efs-mkfs not found at {}", mkfs.display());
        return;
    }
    if !fsck.exists() {
        eprintln!("SKIP: efs-fsck not found at {}", fsck.display());
        return;
    }

    // Use TempImage so the file is cleaned up even on panic.
    let img = TempImage::new("efs_fsck_test_replay_idempotent.img");
    // Remove stale file.
    let _ = std::fs::remove_file(&img.path);

    // Create a fresh image.
    let status = Command::new(&mkfs)
        .args(["--size", "32M", "--journal-size-mib", "4"])
        .arg(&img.path)
        .status()
        .expect("failed to run efs-mkfs");
    assert!(status.success(), "efs-mkfs failed: {status}");

    let (journal_first_block, block_size) = read_sb_journal_info(&img.path);

    // Dirty the journal so tail_seq != head_seq.
    dirty_jsb(&img.path, journal_first_block, block_size);

    let (tail_before, head_before, _, _) =
        read_jsb_seqs_full(&img.path, journal_first_block, block_size);
    assert_ne!(
        tail_before, head_before,
        "journal should be dirty after patching"
    );

    // First run: --repair should replay and reset the JSB.
    let status = Command::new(&fsck)
        .args(["--repair", "--verbose"])
        .arg(&img.path)
        .status()
        .expect("failed to run efs-fsck (first pass)");
    assert!(
        status.code().unwrap_or(99) < 2,
        "efs-fsck --repair exited with error code: {status}"
    );

    // After repair the JSB must have tail == head on both seq and block cursors.
    let (tail_seq, head_seq, tail_block, head_block) =
        read_jsb_seqs_full(&img.path, journal_first_block, block_size);
    assert_eq!(
        tail_seq, head_seq,
        "after --repair tail_seq must equal head_seq"
    );
    assert_eq!(
        tail_block, head_block,
        "after --repair tail_block must equal head_block"
    );

    // Second run (read-only): must be clean and exit 0.
    let output = Command::new(&fsck)
        .args(["--verbose"])
        .arg(&img.path)
        .output()
        .expect("failed to run efs-fsck (second pass)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("journal is dirty"),
        "second run should not report dirty journal; got: {stdout}"
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "second run must exit 0 (clean); got: {:?}\nstdout: {stdout}",
        output.status.code(),
    );
}
