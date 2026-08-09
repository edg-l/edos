//! Writing a GPT: protective MBR, both headers, both copies of the entry
//! array. Follows UEFI 2.10 §5.3.

use std::io::{self, Seek, SeekFrom, Write};

use efs_common::crc32;

use crate::guid;

pub const SECTOR: u64 = 512;
const ENTRY_SIZE: usize = 128;
const ENTRY_COUNT: usize = 128;
const ENTRY_SECTORS: u64 = (ENTRY_SIZE * ENTRY_COUNT) as u64 / SECTOR;
/// LBA 1 header, LBAs 2..33 entries, and the same again at the far end.
pub const FIRST_USABLE_LBA: u64 = 2 + ENTRY_SECTORS;

pub struct PartitionSpec {
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub name: &'static str,
}

fn entry_bytes(part: &PartitionSpec) -> [u8; ENTRY_SIZE] {
    let mut e = [0u8; ENTRY_SIZE];
    e[0..16].copy_from_slice(&part.type_guid);
    e[16..32].copy_from_slice(&part.unique_guid);
    e[32..40].copy_from_slice(&part.first_lba.to_le_bytes());
    e[40..48].copy_from_slice(&part.last_lba.to_le_bytes());
    // 48..56 attributes stay zero.
    for (i, unit) in part.name.encode_utf16().take(35).enumerate() {
        let at = 56 + i * 2;
        e[at..at + 2].copy_from_slice(&unit.to_le_bytes());
    }
    e
}

fn header_bytes(
    my_lba: u64,
    alternate_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entry_lba: u64,
    disk_guid: &[u8; 16],
    entries_crc: u32,
) -> [u8; SECTOR as usize] {
    let mut h = [0u8; SECTOR as usize];
    h[0..8].copy_from_slice(b"EFI PART");
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    h[12..16].copy_from_slice(&92u32.to_le_bytes()); // header size
    // 16..20 is the header CRC, filled in below.
    h[24..32].copy_from_slice(&my_lba.to_le_bytes());
    h[32..40].copy_from_slice(&alternate_lba.to_le_bytes());
    h[40..48].copy_from_slice(&first_usable.to_le_bytes());
    h[48..56].copy_from_slice(&last_usable.to_le_bytes());
    h[56..72].copy_from_slice(disk_guid);
    h[72..80].copy_from_slice(&entry_lba.to_le_bytes());
    h[80..84].copy_from_slice(&(ENTRY_COUNT as u32).to_le_bytes());
    h[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
    h[88..92].copy_from_slice(&entries_crc.to_le_bytes());

    // The header CRC covers exactly header_size bytes with the field zeroed.
    let crc = crc32(&h[0..92]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}

/// Protective MBR: one 0xEE partition covering the disk, so tools that only
/// understand MBR see the disk as fully allocated rather than empty.
fn protective_mbr(disk_sectors: u64) -> [u8; SECTOR as usize] {
    let mut mbr = [0u8; SECTOR as usize];
    let covered = (disk_sectors - 1).min(u32::MAX as u64) as u32;
    let e = &mut mbr[446..462];
    e[0] = 0x00; // not bootable
    e[1] = 0x00; // CHS start: head 0
    e[2] = 0x02; // sector 2
    e[3] = 0x00; // cylinder 0
    e[4] = 0xEE; // GPT protective
    e[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]); // CHS end: maxed out
    e[8..12].copy_from_slice(&1u32.to_le_bytes());
    e[12..16].copy_from_slice(&covered.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    mbr
}

fn write_at<W: Write + Seek>(dev: &mut W, lba: u64, data: &[u8]) -> io::Result<()> {
    dev.seek(SeekFrom::Start(lba * SECTOR))?;
    dev.write_all(data)
}

/// Write a complete GPT describing `parts` onto a `disk_sectors`-sector device.
pub fn write<W: Write + Seek>(
    dev: &mut W,
    disk_sectors: u64,
    parts: &[PartitionSpec],
) -> io::Result<()> {
    assert!(parts.len() <= ENTRY_COUNT);

    let mut array = vec![0u8; ENTRY_SIZE * ENTRY_COUNT];
    for (i, part) in parts.iter().enumerate() {
        array[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE].copy_from_slice(&entry_bytes(part));
    }
    let entries_crc = crc32(&array);

    let last_lba = disk_sectors - 1;
    let backup_entries_lba = last_lba - ENTRY_SECTORS;
    let last_usable = backup_entries_lba - 1;
    let disk_guid = guid::random();

    write_at(dev, 0, &protective_mbr(disk_sectors))?;
    write_at(dev, 2, &array)?;
    write_at(dev, backup_entries_lba, &array)?;
    write_at(
        dev,
        1,
        &header_bytes(
            1,
            last_lba,
            FIRST_USABLE_LBA,
            last_usable,
            2,
            &disk_guid,
            entries_crc,
        ),
    )?;
    write_at(
        dev,
        last_lba,
        &header_bytes(
            last_lba,
            1,
            FIRST_USABLE_LBA,
            last_usable,
            backup_entries_lba,
            &disk_guid,
            entries_crc,
        ),
    )?;
    dev.flush()
}

/// Last usable LBA for a device of `disk_sectors` sectors.
pub fn last_usable_lba(disk_sectors: u64) -> u64 {
    disk_sectors - 1 - ENTRY_SECTORS - 1
}
