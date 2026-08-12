/// Shared test helpers for efs-fsck integration tests.
///
/// Mutator helpers (`corrupt_block_bitmap_bit`, `corrupt_link_count`,
/// `force_dirty_journal`, `scribble_journal_commit`) live here as test-only
/// code. There is intentionally no separate `efs-corrupter` tool; keeping
/// mutators inside the test crate minimises surface area and avoids shipping
/// a corruption utility alongside the filesystem tools.
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use efs_common::{
    DESC_FLAG_ESCAPED, DescriptorEntry, JOURNAL_BLOCK_MAGIC, RevokeEntry, build_commit_block,
    build_descriptor_block, build_revoke_block, commit_block_checksum,
};

// ---------------------------------------------------------------------------
// Binary locators
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    // tests/ is inside tools/efs-fsck/; go up three levels to repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

pub fn mkfs_bin() -> PathBuf {
    workspace_root().join("tools/efs-mkfs/target/release/efs-mkfs")
}

pub fn fsck_bin() -> PathBuf {
    workspace_root().join("tools/efs-fsck/target/release/efs-fsck")
}

// ---------------------------------------------------------------------------
// TempImage: a temp file that deletes itself on drop
// ---------------------------------------------------------------------------

/// Owns a temporary image file. The file is deleted when this is dropped.
pub struct TempImage {
    pub path: PathBuf,
}

impl TempImage {
    /// Create a new TempImage at a unique path under the system temp dir.
    pub fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(name);
        TempImage { path }
    }
}

impl Drop for TempImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Fixture builders
// ---------------------------------------------------------------------------

/// Build a small clean EFS image using efs-mkfs and return a TempImage.
///
/// Panics (skips with a message) if efs-mkfs is not built.
pub fn build_fixture_clean(name: &str) -> TempImage {
    let mkfs = mkfs_bin();
    if !mkfs.exists() {
        panic!(
            "SKIP: efs-mkfs not found at {}; build it first",
            mkfs.display()
        );
    }
    let img = TempImage::new(name);
    // Remove stale file from a previous failed run.
    let _ = std::fs::remove_file(&img.path);
    let status = Command::new(&mkfs)
        .args(["--size", "32M", "--journal-size-mib", "4"])
        .arg(&img.path)
        .status()
        .expect("failed to spawn efs-mkfs");
    assert!(status.success(), "efs-mkfs failed: {status}");
    img
}

// ---------------------------------------------------------------------------
// fsck runner
// ---------------------------------------------------------------------------

/// Run efs-fsck with the given extra arguments against `image`.
///
/// Returns `(exit_code, stdout, stderr)`.
pub fn run_fsck(image: &Path, args: &[&str]) -> (i32, String, String) {
    let fsck = fsck_bin();
    if !fsck.exists() {
        panic!(
            "SKIP: efs-fsck not found at {}; build it first",
            fsck.display()
        );
    }
    let output = Command::new(&fsck)
        .args(args)
        .arg(image)
        .output()
        .expect("failed to spawn efs-fsck");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (code, stdout, stderr)
}

// ---------------------------------------------------------------------------
// On-disk layout helpers. Every one takes the filesystem's byte offset within
// the image: most fixtures sit at 0, but a partition offset is exactly what the
// journal's two block-numbering domains differ by, so a fixture that tests them
// has to be able to place the filesystem elsewhere.
// ---------------------------------------------------------------------------

/// Superblock field offsets (within block 1).
const SB_BLOCK_SIZE_LOG2_OFF: u64 = 4 + 4 + 8 + 8 + 8 + 8; // 40
const SB_BLOCKS_PER_GROUP_OFF: u64 = SB_BLOCK_SIZE_LOG2_OFF + 4; // 44
const SB_INODES_PER_GROUP_OFF: u64 = SB_BLOCKS_PER_GROUP_OFF + 4; // 48
const SB_INODE_SIZE_OFF: u64 = SB_INODES_PER_GROUP_OFF + 4; // 52
const SB_BLOCK_GROUP_COUNT_OFF: u64 = SB_INODE_SIZE_OFF + 2; // 54
const SB_JOURNAL_FIRST_BLOCK_OFF: u64 =
    4 + 4 + 8 + 8 + 8 + 8 + 4 + 4 + 4 + 2 + 2 + 8 + 8 + 8 + 16 + 64 + 8 + 8 + 2 + 2 + 8 + 4; // 192

/// Block group descriptor size on disk.
const BGD_SIZE: u64 = 64;

/// Read a u32 LE at a given byte offset in the file.
fn read_u32_at(f: &mut File, offset: u64) -> io::Result<u32> {
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Read a u64 LE at a given byte offset in the file.
fn read_u64_at(f: &mut File, offset: u64) -> io::Result<u64> {
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; 8];
    f.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Read a u16 LE at a given byte offset in the file.
fn read_u16_at(f: &mut File, offset: u64) -> io::Result<u16> {
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; 2];
    f.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

/// Write a u32 LE at a given byte offset in the file.
#[allow(dead_code)]
fn write_u32_at(f: &mut File, offset: u64, val: u32) -> io::Result<()> {
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&val.to_le_bytes())
}

/// Write a u16 LE at a given byte offset in the file.
#[allow(dead_code)]
fn write_u16_at(f: &mut File, offset: u64, val: u16) -> io::Result<()> {
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&val.to_le_bytes())
}

/// Superblock layout extracted from an image.
pub struct SbInfo {
    pub block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u16,
    #[allow(dead_code)]
    pub block_group_count: u16,
    pub journal_first_block: u64,
}

/// Read the fields we need from the primary superblock of a filesystem that
/// starts `partition_offset` bytes into the image.
pub fn read_sb_info_at(path: &Path, partition_offset: u64) -> io::Result<SbInfo> {
    let mut f = File::open(path)?;
    // Block 1 starts at offset = block_size; but we don't know block_size yet.
    // Block size is at block 1 offset SB_BLOCK_SIZE_LOG2_OFF; block 1 is at
    // 4096 for the default mkfs (4 KiB blocks). We read with the assumption
    // of 4096-byte blocks to get block_size_log2, then re-read if different.
    let default_block: u64 = 4096;
    let bsl = read_u32_at(
        &mut f,
        partition_offset + default_block + SB_BLOCK_SIZE_LOG2_OFF,
    )?;
    let block_size = 1u32 << bsl;
    let sb_base = partition_offset + block_size as u64;

    let blocks_per_group = read_u32_at(&mut f, sb_base + SB_BLOCKS_PER_GROUP_OFF)?;
    let inodes_per_group = read_u32_at(&mut f, sb_base + SB_INODES_PER_GROUP_OFF)?;
    let inode_size = read_u16_at(&mut f, sb_base + SB_INODE_SIZE_OFF)?;
    let block_group_count = read_u16_at(&mut f, sb_base + SB_BLOCK_GROUP_COUNT_OFF)?;
    let journal_first_block = read_u64_at(&mut f, sb_base + SB_JOURNAL_FIRST_BLOCK_OFF)?;

    Ok(SbInfo {
        block_size,
        blocks_per_group,
        inodes_per_group,
        inode_size,
        block_group_count,
        journal_first_block,
    })
}

/// BGD table starts at block 2.  Each entry is 64 bytes.
///
/// Layout of EfsBlockGroupDesc (repr(C), 64 bytes):
///   block_bitmap_block(8) inode_bitmap_block(8) inode_table_block(8)
///   free_blocks_count(4) free_inodes_count(4) used_dirs_count(4)
///   reserved(32) checksum(4)
const BGD_BLOCK_BITMAP_OFF: u64 = 0;
#[allow(dead_code)]
const BGD_INODE_BITMAP_OFF: u64 = 8;
const BGD_INODE_TABLE_OFF: u64 = 16;

fn bgd_byte_offset(partition_offset: u64, block_size: u32, group: u64) -> u64 {
    // BGD table is at block 2.
    partition_offset + 2 * block_size as u64 + group * BGD_SIZE
}

fn read_bgd_field_u64(
    f: &mut File,
    partition_offset: u64,
    block_size: u32,
    group: u64,
    field_off: u64,
) -> io::Result<u64> {
    let off = bgd_byte_offset(partition_offset, block_size, group) + field_off;
    read_u64_at(f, off)
}

// ---------------------------------------------------------------------------
// Mutator: corrupt_block_bitmap_bit
// ---------------------------------------------------------------------------

/// Set a bit in the on-disk block bitmap for `absolute_block`, making it appear
/// as allocated even though no inode references it (a leaked block bitmap bit).
///
/// `absolute_block` must be a data block that is not referenced by any inode
/// (e.g. near the end of the image).  `partition_offset` is 0 for test images.
pub fn corrupt_block_bitmap_bit(
    image: &Path,
    partition_offset: u64,
    absolute_block: u64,
) -> io::Result<()> {
    let sb = read_sb_info_at(image, partition_offset)?;
    let block_size = sb.block_size;
    let blocks_per_group = sb.blocks_per_group as u64;

    // Determine which group and which bit within the group.
    let group = absolute_block / blocks_per_group;
    let bit_in_group = absolute_block % blocks_per_group;

    // Read the block bitmap block number from the BGD.
    let mut f = OpenOptions::new().read(true).write(true).open(image)?;
    let bitmap_block = read_bgd_field_u64(
        &mut f,
        partition_offset,
        block_size,
        group,
        BGD_BLOCK_BITMAP_OFF,
    )?;

    // Compute byte offset of the bitmap block.
    let bitmap_byte_offset = partition_offset + bitmap_block * block_size as u64;

    // Set the bit.
    let byte_idx = (bit_in_group / 8) as u64;
    let bit_pos = (bit_in_group % 8) as u8;

    f.seek(SeekFrom::Start(bitmap_byte_offset + byte_idx))?;
    let mut byte_buf = [0u8; 1];
    f.read_exact(&mut byte_buf)?;
    byte_buf[0] |= 1 << bit_pos;
    f.seek(SeekFrom::Start(bitmap_byte_offset + byte_idx))?;
    f.write_all(&byte_buf)?;
    f.sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Mutator: corrupt_link_count
// ---------------------------------------------------------------------------

/// Overwrite the link_count field of inode `inode_num` (1-based) on disk
/// with `new_count`, and recompute the inode checksum.
///
/// Inode layout (repr(C), 256 bytes; all fields naturally aligned, no padding):
///   mode(u16) uid(u16) gid(u16) link_count(u16) size(u64) blocks(u64)
///   flags(u32) reserved1(u32) ctime_sec(u64) ctime_nsec(u32) reserved2(u32)
///   mtime_sec(u64) mtime_nsec(u32) reserved3(u32) atime_sec(u64)
///   atime_nsec(u32) checksum(u32) data_area([u8;176])
pub fn corrupt_link_count(
    image: &Path,
    partition_offset: u64,
    inode_num: u64,
    new_count: u16,
) -> io::Result<()> {
    const INODE_SIZE: usize = 256;
    const LINK_COUNT_OFF: usize = 6;
    const CHECKSUM_OFF: usize = 76;

    let sb = read_sb_info_at(image, partition_offset)?;
    let block_size = sb.block_size;
    let inodes_per_group = sb.inodes_per_group as u64;
    let inode_size = sb.inode_size as u64;

    let group = (inode_num - 1) / inodes_per_group;
    let local_idx = (inode_num - 1) % inodes_per_group;

    let mut f = OpenOptions::new().read(true).write(true).open(image)?;

    let inode_table_block = read_bgd_field_u64(
        &mut f,
        partition_offset,
        block_size,
        group,
        BGD_INODE_TABLE_OFF,
    )?;

    let byte_off_in_table = local_idx * inode_size;
    let block_off = byte_off_in_table / block_size as u64;
    let off_in_block = byte_off_in_table % block_size as u64;
    let inode_file_offset =
        partition_offset + (inode_table_block + block_off) * block_size as u64 + off_in_block;

    // Read the full 256-byte inode.
    f.seek(SeekFrom::Start(inode_file_offset))?;
    let mut inode_bytes = [0u8; INODE_SIZE];
    f.read_exact(&mut inode_bytes)?;

    // Patch link_count.
    inode_bytes[LINK_COUNT_OFF..LINK_COUNT_OFF + 2].copy_from_slice(&new_count.to_le_bytes());

    // Recompute checksum: CRC32 of the 256-byte inode with checksum field zeroed.
    inode_bytes[CHECKSUM_OFF..CHECKSUM_OFF + 4].copy_from_slice(&0u32.to_le_bytes());
    let new_crc = crc32_of(&inode_bytes);
    inode_bytes[CHECKSUM_OFF..CHECKSUM_OFF + 4].copy_from_slice(&new_crc.to_le_bytes());

    // Write back.
    f.seek(SeekFrom::Start(inode_file_offset))?;
    f.write_all(&inode_bytes)?;
    f.sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Mutator: force_dirty_journal
// ---------------------------------------------------------------------------

/// JSB layout at journal_first_block * block_size (64 bytes):
///   magic(4) version(4) block_count(4) block_size(4)
///   tail_seq(8) head_seq(8) tail_block(8) head_block(8)
///   crc32(4) reserved(12)
const JSB_SIZE: usize = 64;
const TAIL_SEQ_OFF: usize = 4 + 4 + 4 + 4; // 16
const HEAD_SEQ_OFF: usize = TAIL_SEQ_OFF + 8; // 24
const TAIL_BLOCK_OFF: usize = HEAD_SEQ_OFF + 8; // 32
const HEAD_BLOCK_OFF: usize = TAIL_BLOCK_OFF + 8; // 40
const CRC_OFF: usize = HEAD_BLOCK_OFF + 8; // 48

/// Make the journal appear dirty by advancing head_seq past tail_seq.
/// No real transactions are written; this simulates a clean-ish dirty state
/// where the journal ring contains no valid commit blocks.
pub fn force_dirty_journal(image: &Path, partition_offset: u64) -> io::Result<()> {
    let sb = read_sb_info_at(image, partition_offset)?;
    let jsb_offset = partition_offset + sb.journal_first_block * sb.block_size as u64;

    let mut f = OpenOptions::new().read(true).write(true).open(image)?;
    f.seek(SeekFrom::Start(jsb_offset))?;
    let mut buf = [0u8; JSB_SIZE];
    f.read_exact(&mut buf)?;

    let head_seq = u64::from_le_bytes(buf[HEAD_SEQ_OFF..HEAD_SEQ_OFF + 8].try_into().unwrap());
    let new_head_seq = head_seq + 1;
    buf[HEAD_SEQ_OFF..HEAD_SEQ_OFF + 8].copy_from_slice(&new_head_seq.to_le_bytes());

    // Recompute CRC.
    buf[CRC_OFF..CRC_OFF + 4].copy_from_slice(&0u32.to_le_bytes());
    let crc = crc32_of(&buf);
    buf[CRC_OFF..CRC_OFF + 4].copy_from_slice(&crc.to_le_bytes());

    f.seek(SeekFrom::Start(jsb_offset))?;
    f.write_all(&buf)?;
    f.sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fixture builder: plant real journal transactions
// ---------------------------------------------------------------------------

/// One transaction to write into a journal ring, described the way the kernel's
/// committer describes it.
pub struct TxSpec {
    /// Sequence number for the descriptor and commit headers.
    pub seq: u64,
    /// `(device-absolute home block, contents)` pairs. Device-absolute means
    /// the partition's offset is already included, which is what the kernel
    /// records; see `DescriptorEntry::fs_block`.
    pub blocks: Vec<(u64, Vec<u8>)>,
    /// `(device-absolute block, seq)` revokes carried by this transaction.
    pub revokes: Vec<(u64, u64)>,
}

impl TxSpec {
    /// A transaction carrying one home block filled with `fill`.
    pub fn one_block(seq: u64, home_block: u64, fill: u8, block_size: usize) -> Self {
        TxSpec {
            seq,
            blocks: vec![(home_block, vec![fill; block_size])],
            revokes: Vec::new(),
        }
    }
}

/// Cursors to leave in the journal superblock after planting.
pub struct JsbCursors {
    pub tail_seq: u64,
    pub tail_block: u64,
    pub head_seq: u64,
    pub head_block: u64,
}

/// Write `txs` into the journal ring starting at ring index `start_index`, then
/// set the journal superblock's cursors to `cursors`.
///
/// Ring blocks are built with `efs-common`'s builders, the same code the kernel
/// writes the ring with, so a test that passes says the checker agrees with the
/// real on-disk format rather than with this file's idea of it.
///
/// `commit` controls whether each transaction gets its commit block. Planting
/// one without it is how a torn, in-flight transaction is reproduced.
///
/// Returns the ring index just past the last block written.
pub fn plant_journal_txs(
    image: &Path,
    partition_offset: u64,
    start_index: u64,
    txs: &[TxSpec],
    commit: bool,
    cursors: JsbCursors,
) -> io::Result<u64> {
    let sb = read_sb_info_at(image, partition_offset)?;
    let block_size = sb.block_size as usize;
    let jsb_offset = partition_offset + sb.journal_first_block * block_size as u64;

    let mut f = OpenOptions::new().read(true).write(true).open(image)?;
    let ring_size = read_u32_at(&mut f, jsb_offset + 8)? as u64 - 1;

    let mut ring_index = start_index;
    let put = |f: &mut File, index: u64, block: &[u8]| -> io::Result<()> {
        let region_block = sb.journal_first_block + (index % ring_size) + 1;
        f.seek(SeekFrom::Start(
            partition_offset + region_block * block_size as u64,
        ))?;
        f.write_all(block)
    };

    for tx in txs {
        let mut entries = Vec::with_capacity(tx.blocks.len());
        let mut payload = Vec::with_capacity(tx.blocks.len() * block_size);
        for (home_block, data) in &tx.blocks {
            // Escaping, as the committer does it: a data block whose first word
            // would be mistaken for a journal header is zeroed there and flagged.
            let mut copy = data.clone();
            let first_word = u32::from_le_bytes(copy[..4].try_into().unwrap());
            let escaped = first_word == JOURNAL_BLOCK_MAGIC;
            if escaped {
                copy[..4].fill(0);
            }
            entries.push(DescriptorEntry {
                fs_block: *home_block,
                flags: if escaped { DESC_FLAG_ESCAPED } else { 0 },
                _reserved: 0,
            });
            payload.extend_from_slice(&copy);
        }

        put(
            &mut f,
            ring_index,
            &build_descriptor_block(block_size, tx.seq, tx.seq, &entries),
        )?;
        ring_index += 1;

        for chunk in payload.chunks(block_size) {
            put(&mut f, ring_index, chunk)?;
            ring_index += 1;
        }

        if !tx.revokes.is_empty() {
            let revokes: Vec<RevokeEntry> = tx
                .revokes
                .iter()
                .map(|&(fs_block, seq)| RevokeEntry { fs_block, seq })
                .collect();
            put(
                &mut f,
                ring_index,
                &build_revoke_block(block_size, tx.seq, tx.seq, &revokes),
            )?;
            ring_index += 1;
        }

        if commit {
            let crc = commit_block_checksum(&payload);
            put(
                &mut f,
                ring_index,
                &build_commit_block(block_size, tx.seq, tx.seq, crc),
            )?;
            ring_index += 1;
        }
    }

    // Patch the four cursors and re-checksum the JSB.
    f.seek(SeekFrom::Start(jsb_offset))?;
    let mut jsb = [0u8; JSB_SIZE];
    f.read_exact(&mut jsb)?;
    jsb[TAIL_SEQ_OFF..TAIL_SEQ_OFF + 8].copy_from_slice(&cursors.tail_seq.to_le_bytes());
    jsb[HEAD_SEQ_OFF..HEAD_SEQ_OFF + 8].copy_from_slice(&cursors.head_seq.to_le_bytes());
    jsb[TAIL_BLOCK_OFF..TAIL_BLOCK_OFF + 8].copy_from_slice(&cursors.tail_block.to_le_bytes());
    jsb[HEAD_BLOCK_OFF..HEAD_BLOCK_OFF + 8].copy_from_slice(&cursors.head_block.to_le_bytes());
    jsb[CRC_OFF..CRC_OFF + 4].copy_from_slice(&0u32.to_le_bytes());
    let crc = crc32_of(&jsb);
    jsb[CRC_OFF..CRC_OFF + 4].copy_from_slice(&crc.to_le_bytes());
    f.seek(SeekFrom::Start(jsb_offset))?;
    f.write_all(&jsb)?;
    f.sync_all()?;

    Ok(ring_index)
}

/// Copy `image` into a new one preceded by `prefix_bytes` of zeros, so the
/// filesystem sits at a partition offset the way it does on a real GPT disk.
///
/// A partition offset of 0 hides any confusion between the device-absolute and
/// partition-relative block domains, because the two coincide there.
pub fn with_partition_prefix(image: &Path, name: &str, prefix_bytes: u64) -> io::Result<TempImage> {
    let out = TempImage::new(name);
    let _ = std::fs::remove_file(&out.path);
    let body = std::fs::read(image)?;
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&out.path)?;
    f.write_all(&vec![0u8; prefix_bytes as usize])?;
    f.write_all(&body)?;
    f.sync_all()?;
    Ok(out)
}

/// Read one block of the image at a byte offset.
pub fn read_bytes_at(image: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut f = File::open(image)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Mutator: scribble_journal_commit
// ---------------------------------------------------------------------------

/// Flip a byte in the CRC field of the JSB itself, simulating a corrupt
/// journal superblock (not a corrupt commit block within the ring).
///
/// Note: hand-crafting a real commit block within the journal ring requires a
/// kernel-side runtime to write actual transactions. This mutator instead
/// simulates a corrupted JSB checksum so fsck sees an invalid journal state.
/// The test `partial_tx_discarded` is marked `#[ignore]` because testing true
/// mid-ring commit corruption requires a kernel-generated image; see that test
/// for the rationale.
pub fn scribble_journal_commit(image: &Path, partition_offset: u64) -> io::Result<()> {
    let sb = read_sb_info_at(image, partition_offset)?;
    let jsb_offset = partition_offset + sb.journal_first_block * sb.block_size as u64;

    let mut f = OpenOptions::new().read(true).write(true).open(image)?;
    // Flip one byte in the CRC field (offset 48 in JSB).
    let crc_byte_offset = jsb_offset + CRC_OFF as u64;
    f.seek(SeekFrom::Start(crc_byte_offset))?;
    let mut byte_buf = [0u8; 1];
    f.read_exact(&mut byte_buf)?;
    byte_buf[0] ^= 0xFF;
    f.seek(SeekFrom::Start(crc_byte_offset))?;
    f.write_all(&byte_buf)?;
    f.sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// JSB seq reader (used by tests to verify repair)
// ---------------------------------------------------------------------------

/// The journal superblock's four ring cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsbState {
    pub tail_seq: u64,
    pub head_seq: u64,
    pub tail_block: u64,
    pub head_block: u64,
}

/// Read the ring cursors from the on-disk JSB.
pub fn read_jsb(image: &Path, partition_offset: u64) -> io::Result<JsbState> {
    let sb = read_sb_info_at(image, partition_offset)?;
    let jsb_offset = partition_offset + sb.journal_first_block * sb.block_size as u64;

    let mut f = File::open(image)?;
    f.seek(SeekFrom::Start(jsb_offset))?;
    let mut buf = [0u8; JSB_SIZE];
    f.read_exact(&mut buf)?;

    let at = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
    Ok(JsbState {
        tail_seq: at(TAIL_SEQ_OFF),
        head_seq: at(HEAD_SEQ_OFF),
        tail_block: at(TAIL_BLOCK_OFF),
        head_block: at(HEAD_BLOCK_OFF),
    })
}

// ---------------------------------------------------------------------------
// CRC32
// ---------------------------------------------------------------------------

/// CRC32 (ISO 3309 / PKZIP polynomial) matching `libs/efs-common/src/crc32.rs`.
pub fn crc32_of(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// Fixture builder: inodes with no name, on and off the orphan chain
// ---------------------------------------------------------------------------

/// Inode field offsets within the 256-byte on-disk struct.
const INODE_SIZE: u64 = 256;
const INODE_MODE_OFF: usize = 0;
const INODE_LINK_COUNT_OFF: usize = 6;
const INODE_FLAGS_OFF: usize = 24;
const INODE_ORPHAN_NEXT_OFF: usize = 28;
const INODE_CHECKSUM_OFF: usize = 76;
/// Superblock offsets. `EfsSuperblock` is `repr(C, packed)`, so these are the
/// running sums of the field sizes before them with no padding anywhere.
const SB_CHECKSUM_OFF: usize = 188;
const SB_LAST_ORPHAN_OFF: u64 = 209;

/// Byte offset of inode `ino` within the image.
fn inode_file_offset(
    f: &mut File,
    partition_offset: u64,
    sb: &SbInfo,
    ino: u64,
) -> io::Result<u64> {
    let inodes_per_group = sb.inodes_per_group as u64;
    let group = (ino - 1) / inodes_per_group;
    let local_idx = (ino - 1) % inodes_per_group;
    let inode_table_block = read_bgd_field_u64(
        f,
        partition_offset,
        sb.block_size,
        group,
        BGD_INODE_TABLE_OFF,
    )?;
    let byte_off_in_table = local_idx * INODE_SIZE;
    Ok(partition_offset
        + (inode_table_block + byte_off_in_table / sb.block_size as u64) * sb.block_size as u64
        + byte_off_in_table % sb.block_size as u64)
}

/// Set the inode-bitmap bit for `ino`.
fn set_inode_bitmap_bit(
    f: &mut File,
    partition_offset: u64,
    sb: &SbInfo,
    ino: u64,
) -> io::Result<()> {
    let inodes_per_group = sb.inodes_per_group as u64;
    let group = (ino - 1) / inodes_per_group;
    let local_bit = (ino - 1) % inodes_per_group;
    let bitmap_block = read_bgd_field_u64(
        f,
        partition_offset,
        sb.block_size,
        group,
        BGD_INODE_BITMAP_OFF,
    )?;
    let byte_offset = partition_offset + bitmap_block * sb.block_size as u64 + local_bit / 8;
    f.seek(SeekFrom::Start(byte_offset))?;
    let mut byte = [0u8; 1];
    f.read_exact(&mut byte)?;
    byte[0] |= 1 << (local_bit % 8);
    f.seek(SeekFrom::Start(byte_offset))?;
    f.write_all(&byte)
}

/// Write a regular-file inode with no name and no data blocks at `ino`, and mark
/// it allocated.
///
/// This is the on-disk state of a file whose last directory entry is gone but
/// whose inode has not been freed: what the checker sees as an orphan. Inline
/// data with size 0 keeps it out of the extent checks, which have nothing to say
/// here.
pub fn plant_unnamed_inode(
    image: &Path,
    partition_offset: u64,
    ino: u64,
    orphan_next: u32,
) -> io::Result<()> {
    let sb = read_sb_info_at(image, partition_offset)?;
    let mut f = OpenOptions::new().read(true).write(true).open(image)?;
    let offset = inode_file_offset(&mut f, partition_offset, &sb, ino)?;

    let mut inode = [0u8; INODE_SIZE as usize];
    // S_IFREG | 0644, link_count 0, INODE_FLAG_INLINE_DATA, size 0.
    inode[INODE_MODE_OFF..INODE_MODE_OFF + 2]
        .copy_from_slice(&(efs_common::S_IFREG | 0o644).to_le_bytes());
    inode[INODE_LINK_COUNT_OFF..INODE_LINK_COUNT_OFF + 2].copy_from_slice(&0u16.to_le_bytes());
    inode[INODE_FLAGS_OFF..INODE_FLAGS_OFF + 4]
        .copy_from_slice(&efs_common::INODE_FLAG_INLINE_DATA.to_le_bytes());
    inode[INODE_ORPHAN_NEXT_OFF..INODE_ORPHAN_NEXT_OFF + 4]
        .copy_from_slice(&orphan_next.to_le_bytes());
    let crc = crc32_of(&inode);
    inode[INODE_CHECKSUM_OFF..INODE_CHECKSUM_OFF + 4].copy_from_slice(&crc.to_le_bytes());

    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&inode)?;
    set_inode_bitmap_bit(&mut f, partition_offset, &sb, ino)?;
    f.sync_all()
}

/// Point the superblock's orphan chain head at `ino` (0 empties the chain).
pub fn set_orphan_head(image: &Path, partition_offset: u64, ino: u32) -> io::Result<()> {
    let sb = read_sb_info_at(image, partition_offset)?;
    let sb_offset = partition_offset + sb.block_size as u64;

    let mut f = OpenOptions::new().read(true).write(true).open(image)?;
    f.seek(SeekFrom::Start(sb_offset))?;
    let mut bytes = [0u8; 256];
    f.read_exact(&mut bytes)?;

    let off = SB_LAST_ORPHAN_OFF as usize;
    bytes[off..off + 4].copy_from_slice(&ino.to_le_bytes());
    bytes[SB_CHECKSUM_OFF..SB_CHECKSUM_OFF + 4].copy_from_slice(&0u32.to_le_bytes());
    let crc = crc32_of(&bytes);
    bytes[SB_CHECKSUM_OFF..SB_CHECKSUM_OFF + 4].copy_from_slice(&crc.to_le_bytes());

    f.seek(SeekFrom::Start(sb_offset))?;
    f.write_all(&bytes)?;
    f.sync_all()
}

/// Read the superblock's orphan chain head.
pub fn read_orphan_head(image: &Path, partition_offset: u64) -> io::Result<u32> {
    let sb = read_sb_info_at(image, partition_offset)?;
    let mut f = File::open(image)?;
    f.seek(SeekFrom::Start(
        partition_offset + sb.block_size as u64 + SB_LAST_ORPHAN_OFF,
    ))?;
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}
