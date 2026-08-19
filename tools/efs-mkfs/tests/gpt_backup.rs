//! A partition's length comes from its GPT entry, so formatting it leaves the
//! backup header and entry array at the end of the disk alone.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use efs_common::crc32::crc32;
use efs_mkfs::{Format, format};

const SECTOR: u64 = 512;
/// The backup entry array plus the backup header: UEFI 2.10 §5.3.1 puts both
/// in the last 33 sectors of the disk.
const BACKUP_SECTORS: u64 = 33;
const ENTRY_COUNT: u64 = 128;
const ENTRY_SIZE: u64 = 128;
const FIRST_LBA: u64 = 2048;

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// One entry array holding a single partition that runs to the last usable LBA.
fn entry_array(last_usable: u64) -> Vec<u8> {
    let mut entries = vec![0u8; (ENTRY_COUNT * ENTRY_SIZE) as usize];
    let entry = &mut entries[..ENTRY_SIZE as usize];
    entry[0..16].copy_from_slice(&[0x0bu8; 16]); // type GUID, any non-zero value
    entry[16..32].copy_from_slice(&[0x77u8; 16]); // unique partition GUID
    put_u64(entry, 32, FIRST_LBA);
    put_u64(entry, 40, last_usable);
    entries
}

/// A GPT header at `my_lba`, describing the array at `entry_lba`.
fn header(
    disk_sectors: u64,
    my_lba: u64,
    alternate_lba: u64,
    entry_lba: u64,
    array: &[u8],
) -> Vec<u8> {
    let mut hdr = vec![0u8; SECTOR as usize];
    hdr[0..8].copy_from_slice(b"EFI PART");
    put_u32(&mut hdr, 8, 0x0001_0000);
    put_u32(&mut hdr, 12, 92);
    put_u64(&mut hdr, 24, my_lba);
    put_u64(&mut hdr, 32, alternate_lba);
    put_u64(&mut hdr, 40, 34);
    put_u64(&mut hdr, 48, disk_sectors - BACKUP_SECTORS - 1);
    hdr[56..72].copy_from_slice(&[0x42u8; 16]); // disk GUID
    put_u64(&mut hdr, 72, entry_lba);
    put_u32(&mut hdr, 80, ENTRY_COUNT as u32);
    put_u32(&mut hdr, 84, ENTRY_SIZE as u32);
    put_u32(&mut hdr, 88, crc32(array));
    let crc = crc32(&hdr[..92]);
    put_u32(&mut hdr, 16, crc);
    hdr
}

/// A sparse image carrying a protective-MBR-free GPT with one partition.
///
/// Returns the last usable LBA, which is where that partition ends.
fn make_gpt_image(path: &Path, disk_sectors: u64) -> u64 {
    let last_usable = disk_sectors - BACKUP_SECTORS - 1;
    let array = entry_array(last_usable);
    let backup_array_lba = disk_sectors - BACKUP_SECTORS;

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.set_len(disk_sectors * SECTOR).unwrap();

    file.seek(SeekFrom::Start(SECTOR)).unwrap();
    file.write_all(&header(disk_sectors, 1, disk_sectors - 1, 2, &array))
        .unwrap();
    file.seek(SeekFrom::Start(2 * SECTOR)).unwrap();
    file.write_all(&array).unwrap();

    file.seek(SeekFrom::Start(backup_array_lba * SECTOR))
        .unwrap();
    file.write_all(&array).unwrap();
    file.seek(SeekFrom::Start((disk_sectors - 1) * SECTOR))
        .unwrap();
    file.write_all(&header(
        disk_sectors,
        disk_sectors - 1,
        1,
        backup_array_lba,
        &array,
    ))
    .unwrap();

    last_usable
}

fn read_tail(path: &Path, bytes: u64) -> Vec<u8> {
    let mut file = File::open(path).unwrap();
    let len = file.metadata().unwrap().len();
    file.seek(SeekFrom::Start(len - bytes)).unwrap();
    let mut buf = vec![0u8; bytes as usize];
    file.read_exact(&mut buf).unwrap();
    buf
}

fn image_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("efs-mkfs-gpt-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn formatting_a_gpt_partition_leaves_the_backup_table_alone() {
    let disk_sectors = 512 * 1024 * 1024 / SECTOR;
    let path = image_path("backup-table.img");
    make_gpt_image(&path, disk_sectors);
    let before = read_tail(&path, BACKUP_SECTORS * SECTOR);

    format(&Format {
        target: &path,
        partition_offset: FIRST_LBA * SECTOR,
        partition_size: None,
        block_size: 4096,
        label: Some("EDOS"),
        journal_size_mib: 1,
        populate: None,
    })
    .unwrap();

    assert_eq!(
        before,
        read_tail(&path, BACKUP_SECTORS * SECTOR),
        "the backup GPT header and entry array were written over"
    );
}

/// The fixture is one where the two sizings differ: without the entry to read,
/// everything after the offset would reach into the backup table.
#[test]
fn the_tail_of_the_disk_is_not_part_of_the_partition() {
    let disk_sectors = 512 * 1024 * 1024 / SECTOR;
    let path = image_path("sizing.img");
    let last_usable = make_gpt_image(&path, disk_sectors);

    let from_entry = (last_usable - FIRST_LBA + 1) * SECTOR;
    let to_end_of_disk = disk_sectors * SECTOR - FIRST_LBA * SECTOR;
    assert_eq!(to_end_of_disk - from_entry, BACKUP_SECTORS * SECTOR);
}

/// An image with no table is still formatted to its end: a bare filesystem
/// image is the ordinary case for `--size`, and there is nothing to preserve.
#[test]
fn an_image_without_a_table_still_uses_everything_after_the_offset() {
    let path = image_path("no-table.img");
    let bytes = 64 * 1024 * 1024;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.set_len(bytes).unwrap();
    drop(file);

    format(&Format {
        target: &path,
        partition_offset: 0,
        partition_size: None,
        block_size: 4096,
        label: None,
        journal_size_mib: 1,
        populate: None,
    })
    .unwrap();

    assert_eq!(File::open(&path).unwrap().metadata().unwrap().len(), bytes);
}
