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
    JsbCursors, TxSpec, build_fixture_clean, corrupt_block_bitmap_bit, corrupt_link_count,
    force_dirty_journal, plant_journal_txs, plant_unnamed_inode, read_bytes_at, read_jsb,
    read_orphan_head, run_fsck, scribble_journal_commit, set_orphan_head,
};

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

// ---- Journal: what decides that a ring holds work ---------------------------
//
// The image efs-mkfs produces has tail_seq == head_seq == 1 and an empty ring,
// and its journal starts at block 7168 of 8192. Home blocks in these tests go
// to the block just below the journal: free space, whose contents no fsck phase
// reads, so a planted block cannot turn into an unrelated finding.

/// Block a planted transaction carries, and the fill byte identifying it.
const HOME_BLOCK: u64 = 7167;
const LIVE_FILL: u8 = 0xA5;
const STALE_FILL: u8 = 0x5A;
const BLOCK_SIZE: usize = 4096;

/// A committed transaction in the ring is replayed even when the superblock's
/// head never got the news.
///
/// This is the window between a commit block reaching the platter and the
/// journal superblock being written: the cursors say tail == head, and the ring
/// holds durable work anyway. Deciding dirtiness from those cursors made fsck
/// call this image clean and then check it against home blocks the journal was
/// still holding the current contents of.
#[test]
fn committed_tx_is_found_when_the_head_never_reached_the_superblock() {
    let img = build_fixture_clean("efs_fsck_test_journal_committed_tx.img");
    let before = read_jsb(&img.path, 0).expect("read_jsb failed");

    plant_journal_txs(
        &img.path,
        0,
        before.tail_block,
        &[TxSpec::one_block(
            before.tail_seq,
            HOME_BLOCK,
            LIVE_FILL,
            BLOCK_SIZE,
        )],
        true,
        JsbCursors {
            tail_seq: before.tail_seq,
            tail_block: before.tail_block,
            head_seq: before.tail_seq,
            head_block: before.tail_block,
        },
    )
    .expect("plant_journal_txs failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 4,
        "expected exit 4 for a ring holding a committed tx; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("journal is dirty"),
        "expected 'journal is dirty' in stdout; got:\n{stdout}"
    );

    let (code, stdout, stderr) = run_fsck(&img.path, &["--repair", "--yes"]);
    assert_eq!(
        code, 1,
        "expected exit 1 (errors fixed) after replaying; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    let home = read_bytes_at(&img.path, HOME_BLOCK * BLOCK_SIZE as u64, BLOCK_SIZE)
        .expect("read home block");
    assert!(
        home.iter().all(|&b| b == LIVE_FILL),
        "replay did not write the journalled block to its home location"
    );

    // Both cursors come from the ring walk, not from the head that was stale.
    let after = read_jsb(&img.path, 0).expect("read_jsb failed");
    assert_eq!(
        after.tail_seq,
        before.tail_seq + 1,
        "tail_seq must be one past the replayed tx"
    );
    assert_eq!(after.head_seq, after.tail_seq, "cursors must agree");
    assert_eq!(
        after.tail_block,
        before.tail_block + 3,
        "descriptor + data + commit is three ring blocks"
    );
    assert_eq!(after.head_block, after.tail_block, "cursors must agree");

    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 0,
        "a replayed journal must be clean on the next run; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("journal is dirty"),
        "second run should not report a dirty journal; got:\n{stdout}"
    );
}

/// An advanced head over an empty ring is not a dirty journal.
///
/// `head_seq` names the open transaction, which is never committed, so a
/// healthy journal routinely sits a sequence ahead of its tail. Treating that
/// as damage reported a clean filesystem as broken and, under `--repair`,
/// counted a no-op replay as a fix.
#[test]
fn advanced_head_over_an_empty_ring_is_clean() {
    let img = build_fixture_clean("efs_fsck_test_journal_advisory_head.img");

    force_dirty_journal(&img.path, 0).expect("force_dirty_journal failed");
    let cursors = read_jsb(&img.path, 0).expect("read_jsb failed");
    assert_ne!(
        cursors.tail_seq, cursors.head_seq,
        "the fixture should leave head ahead of tail"
    );

    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 0,
        "an empty ring is clean whatever the head says; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("journal is dirty"),
        "expected no dirty-journal finding; got:\n{stdout}"
    );
}

/// A wrapped ring's stale far side must not be replayed.
///
/// The block after the live region can hold an older transaction that still
/// parses perfectly. Applying it rolls metadata back to what it was before the
/// ring wrapped, which is why the walk stops at the first break in sequence
/// continuity rather than at the first block that fails to parse.
#[test]
fn stale_far_side_of_a_wrapped_ring_is_not_replayed() {
    let img = build_fixture_clean("efs_fsck_test_journal_wrapped.img");
    let stale_home = HOME_BLOCK - 1;

    // A tail well above the fixture's own sequence numbers, so the stale
    // transaction's lower one is unambiguously behind the live region.
    plant_journal_txs(
        &img.path,
        0,
        0,
        &[
            TxSpec::one_block(100, HOME_BLOCK, LIVE_FILL, BLOCK_SIZE),
            TxSpec::one_block(97, stale_home, STALE_FILL, BLOCK_SIZE),
        ],
        true,
        JsbCursors {
            tail_seq: 100,
            tail_block: 0,
            head_seq: 100,
            head_block: 0,
        },
    )
    .expect("plant_journal_txs failed");

    // --verbose so the Info finding naming the replayed count is printed.
    let (code, stdout, stderr) = run_fsck(&img.path, &["--repair", "--yes", "--verbose"]);
    assert!(
        code < 2,
        "replay should have succeeded; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    let live = read_bytes_at(&img.path, HOME_BLOCK * BLOCK_SIZE as u64, BLOCK_SIZE)
        .expect("read live home block");
    assert!(
        live.iter().all(|&b| b == LIVE_FILL),
        "the live transaction should have been replayed"
    );

    let stale = read_bytes_at(&img.path, stale_home * BLOCK_SIZE as u64, BLOCK_SIZE)
        .expect("read stale home block");
    assert!(
        stale.iter().all(|&b| b != STALE_FILL),
        "the stale transaction beyond the sequence break was replayed"
    );
    assert!(
        stdout.contains("replayed 1 transaction"),
        "expected exactly one transaction replayed; got:\n{stdout}"
    );
}

/// A transaction with no commit block is in flight, not durable, and is
/// discarded along with everything the walk would otherwise reach past it.
#[test]
fn partial_tx_is_discarded() {
    let img = build_fixture_clean("efs_fsck_test_journal_partial_tx.img");
    let before = read_jsb(&img.path, 0).expect("read_jsb failed");

    plant_journal_txs(
        &img.path,
        0,
        before.tail_block,
        &[TxSpec::one_block(
            before.tail_seq,
            HOME_BLOCK,
            LIVE_FILL,
            BLOCK_SIZE,
        )],
        false, // no commit block: the power went mid-transaction
        JsbCursors {
            tail_seq: before.tail_seq,
            tail_block: before.tail_block,
            head_seq: before.tail_seq + 1,
            head_block: before.tail_block + 2,
        },
    )
    .expect("plant_journal_txs failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &["--repair", "--yes"]);
    assert_eq!(
        code, 0,
        "an uncommitted tx is nothing to fix; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    let home = read_bytes_at(&img.path, HOME_BLOCK * BLOCK_SIZE as u64, BLOCK_SIZE)
        .expect("read home block");
    assert!(
        home.iter().all(|&b| b != LIVE_FILL),
        "an uncommitted transaction was replayed"
    );
}

/// Home blocks land at their device-absolute location when the filesystem sits
/// at a partition offset.
///
/// A descriptor entry's `fs_block` already includes the partition's starting
/// LBA, so adding the offset again puts every replayed block
/// `partition_offset / block_size` blocks too high — the kernel had exactly
/// this bug, and it turned a repairable filesystem into one whose root
/// directory could not be read. Every other fixture here sits at offset 0,
/// where the two addressing domains coincide and the mistake is invisible.
#[test]
fn home_blocks_land_at_their_absolute_location_under_a_partition_offset() {
    const PREFIX: u64 = 1024 * 1024;
    let base = build_fixture_clean("efs_fsck_test_journal_offset_base.img");
    let img = common::with_partition_prefix(&base.path, "efs_fsck_test_journal_offset.img", PREFIX)
        .expect("with_partition_prefix failed");

    let before = read_jsb(&img.path, PREFIX).expect("read_jsb failed");
    let prefix_blocks = PREFIX / BLOCK_SIZE as u64;
    // What the kernel would record for partition-relative block HOME_BLOCK.
    let absolute = prefix_blocks + HOME_BLOCK;

    plant_journal_txs(
        &img.path,
        PREFIX,
        before.tail_block,
        &[TxSpec::one_block(
            before.tail_seq,
            absolute,
            LIVE_FILL,
            BLOCK_SIZE,
        )],
        true,
        JsbCursors {
            tail_seq: before.tail_seq,
            tail_block: before.tail_block,
            head_seq: before.tail_seq,
            head_block: before.tail_block,
        },
    )
    .expect("plant_journal_txs failed");

    let offset_arg = PREFIX.to_string();
    let (code, stdout, stderr) = run_fsck(
        &img.path,
        &["--repair", "--yes", "--partition-offset", &offset_arg],
    );
    assert_eq!(
        code, 1,
        "expected exit 1 (errors fixed) after replaying; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    let landed = read_bytes_at(&img.path, absolute * BLOCK_SIZE as u64, BLOCK_SIZE)
        .expect("read home block");
    assert!(
        landed.iter().all(|&b| b == LIVE_FILL),
        "replay did not write the home block at its device-absolute location"
    );

    // Where the double-counted offset would have put it.
    let wrong = read_bytes_at(&img.path, PREFIX + absolute * BLOCK_SIZE as u64, BLOCK_SIZE)
        .expect("read the doubly-offset location");
    assert!(
        wrong.iter().all(|&b| b != LIVE_FILL),
        "replay added the partition offset to a block number that already carried it"
    );
}

/// A JSB whose checksum does not match is reported rather than replayed, and
/// `--force` downgrades that to a warning so the rest of the check can run.
#[test]
fn corrupt_jsb_checksum_is_reported() {
    let img = build_fixture_clean("efs_fsck_test_journal_bad_jsb.img");

    scribble_journal_commit(&img.path, 0).expect("scribble_journal_commit failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 4,
        "expected exit 4 for a corrupt JSB; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("journal superblock invalid"),
        "expected the JSB to be named as invalid; got:\n{stdout}"
    );

    let (code, stdout, _) = run_fsck(&img.path, &["--force"]);
    assert_eq!(
        code, 0,
        "--force should skip journal replay and finish the check; got {code}\nstdout: {stdout}"
    );
}

// ---- The orphan chain -------------------------------------------------------
//
// An inode on the chain has no name by design: it lost its last one and its
// deletion did not finish before the filesystem went away. Finishing it is what
// a mount does, so the checker completes it without asking. An unnamed inode
// that is *not* on the chain is a leak of unknown provenance and still needs the
// prompt. These two tests are that distinction.

/// Inodes clear of the root and of any metadata in a 32M fixture.
const ORPHAN_INO_A: u64 = 10;
const ORPHAN_INO_B: u64 = 11;

#[test]
fn orphan_chain_deletions_are_finished_without_prompting() {
    let img = build_fixture_clean("efs_fsck_test_orphan_chain.img");

    // Two inodes, chained A -> B, with the superblock pointing at A.
    plant_unnamed_inode(&img.path, 0, ORPHAN_INO_A, ORPHAN_INO_B as u32)
        .expect("plant_unnamed_inode failed");
    plant_unnamed_inode(&img.path, 0, ORPHAN_INO_B, 0).expect("plant_unnamed_inode failed");
    set_orphan_head(&img.path, 0, ORPHAN_INO_A as u32).expect("set_orphan_head failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &["--verbose"]);
    assert_eq!(
        code, 4,
        "expected exit 4 while the deletions are outstanding; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("2 inode(s) pending deletion on the orphan chain"),
        "expected the chain to be reported as pending deletions; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("orphan inode"),
        "a chained inode must not be reported as an orphan leak; got:\n{stdout}"
    );

    // No --yes, and stdin is closed: a prompt would be declined. The chain's
    // deletions are finished anyway, because they need no permission.
    let (code, stdout, stderr) = run_fsck(&img.path, &["--repair"]);
    assert!(
        code < 2,
        "expected the pending deletions to be finished; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("finishing 2 deletion(s)"),
        "expected the repair to say what it finished; got:\n{stdout}"
    );

    let head = read_orphan_head(&img.path, 0).expect("read_orphan_head failed");
    assert_eq!(head, 0, "the chain must be empty once its inodes are freed");

    let (code, stdout, stderr) = run_fsck(&img.path, &["--verbose"]);
    assert_eq!(
        code, 0,
        "expected a clean image after the deletions finished; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn unnamed_inode_off_the_chain_still_needs_the_prompt() {
    let img = build_fixture_clean("efs_fsck_test_orphan_leak.img");

    // Allocated, unnamed, and on no chain: provenance unknown.
    plant_unnamed_inode(&img.path, 0, ORPHAN_INO_A, 0).expect("plant_unnamed_inode failed");

    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 4,
        "expected exit 4 for an orphan leak; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(&format!("orphan inode {ORPHAN_INO_A}")),
        "expected it to be reported as an orphan leak; got:\n{stdout}"
    );

    // stdin is closed, so the prompt is declined and nothing is freed.
    let (_, stdout, _) = run_fsck(&img.path, &["--repair"]);
    assert!(
        stdout.contains("Delete 1 orphan inode(s)?"),
        "expected the destructive prompt; got:\n{stdout}"
    );
    let (code, stdout, stderr) = run_fsck(&img.path, &[]);
    assert_eq!(
        code, 4,
        "a declined prompt must leave the orphan in place; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    // --yes accepts it, and then the image is clean.
    let (_, stdout, stderr) = run_fsck(&img.path, &["--repair", "--yes"]);
    assert!(
        stdout.contains("auto-accepting deletion of 1 orphan inode(s)"),
        "expected --yes to accept the deletion; got:\n{stdout}\nstderr: {stderr}"
    );
    let (code, stdout, stderr) = run_fsck(&img.path, &["--verbose"]);
    assert_eq!(
        code, 0,
        "expected a clean image after the orphan was freed; got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
}
