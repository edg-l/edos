//! Minimal FAT32 formatter for the EFI System Partition.
//!
//! Follows Microsoft's FAT32 File System Specification 1.03: BPB and extended
//! BPB in sector 0, FSInfo in sector 1, backup boot region at sector 6, two
//! FATs, and an empty root directory in cluster 2.

use std::io::{self, Seek, SeekFrom, Write};

const SECTOR: u64 = 512;
const RESERVED_SECTORS: u32 = 32;
const NUM_FATS: u32 = 2;
const BACKUP_BOOT_SECTOR: u16 = 6;
const ROOT_CLUSTER: u32 = 2;
/// 4 KiB clusters: enough clusters to stay FAT32 for any ESP we create, and a
/// whole number of them per page.
const SECTORS_PER_CLUSTER: u32 = 8;
/// FAT32 is only FAT32 above this count (spec §3.5); below it the driver is
/// entitled to read the volume as FAT16.
const MIN_FAT32_CLUSTERS: u32 = 65525;

/// Sectors occupied by one FAT, per the spec's sizing procedure.
fn fat_sectors(total_sectors: u32) -> u32 {
    let usable = total_sectors - RESERVED_SECTORS;
    let per_fat_sector = ((256 * SECTORS_PER_CLUSTER) + NUM_FATS) / 2;
    usable.div_ceil(per_fat_sector)
}

fn boot_sector(total_sectors: u32, hidden_sectors: u32, volume_id: u32) -> [u8; SECTOR as usize] {
    let mut s = [0u8; SECTOR as usize];
    let fat_size = fat_sectors(total_sectors);

    s[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]); // jmp short +0x58; nop
    s[3..11].copy_from_slice(b"MSWIN4.1"); // the string every implementation tests against
    s[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    s[13] = SECTORS_PER_CLUSTER as u8;
    s[14..16].copy_from_slice(&(RESERVED_SECTORS as u16).to_le_bytes());
    s[16] = NUM_FATS as u8;
    // 17..19 root entry count and 19..21 total sectors 16 are zero on FAT32.
    s[21] = 0xF8; // fixed disk
    // 22..24 FATSz16 is zero on FAT32.
    s[24..26].copy_from_slice(&32u16.to_le_bytes()); // sectors per track
    s[26..28].copy_from_slice(&8u16.to_le_bytes()); // heads
    s[28..32].copy_from_slice(&hidden_sectors.to_le_bytes());
    s[32..36].copy_from_slice(&total_sectors.to_le_bytes());
    s[36..40].copy_from_slice(&fat_size.to_le_bytes());
    // 40..42 ext flags: FAT mirroring enabled.
    // 42..44 filesystem version 0.
    s[44..48].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
    s[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
    s[50..52].copy_from_slice(&BACKUP_BOOT_SECTOR.to_le_bytes());
    s[64] = 0x80; // drive number
    s[66] = 0x29; // extended boot signature
    s[67..71].copy_from_slice(&volume_id.to_le_bytes());
    s[71..82].copy_from_slice(b"EDOS ESP   ");
    s[82..90].copy_from_slice(b"FAT32   ");
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

fn fs_info() -> [u8; SECTOR as usize] {
    let mut s = [0u8; SECTOR as usize];
    s[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes()); // "RRaA"
    s[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes()); // "rrAa"
    s[488..492].copy_from_slice(&u32::MAX.to_le_bytes()); // free count unknown
    s[492..496].copy_from_slice(&u32::MAX.to_le_bytes()); // next free unknown
    s[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    s
}

fn write_at<W: Write + Seek>(dev: &mut W, base: u64, sector: u64, data: &[u8]) -> io::Result<()> {
    dev.seek(SeekFrom::Start(base + sector * SECTOR))?;
    dev.write_all(data)
}

/// Format the `total_sectors` sectors starting at `start_lba` as FAT32.
///
/// `volume_id` is the serial number reported to the firmware; any value works,
/// it is only used to tell volumes apart.
pub fn format<W: Write + Seek>(
    dev: &mut W,
    start_lba: u64,
    total_sectors: u32,
    volume_id: u32,
) -> io::Result<()> {
    let fat_size = fat_sectors(total_sectors);
    let data_sectors = total_sectors - RESERVED_SECTORS - fat_size * NUM_FATS;
    let clusters = data_sectors / SECTORS_PER_CLUSTER;
    if clusters < MIN_FAT32_CLUSTERS {
        return Err(io::Error::other(format!(
            "partition too small for FAT32: {clusters} clusters, need {MIN_FAT32_CLUSTERS}"
        )));
    }

    let base = start_lba * SECTOR;
    let boot = boot_sector(total_sectors, start_lba as u32, volume_id);

    write_at(dev, base, 0, &boot)?;
    write_at(dev, base, 1, &fs_info())?;
    write_at(dev, base, BACKUP_BOOT_SECTOR as u64, &boot)?;
    write_at(dev, base, BACKUP_BOOT_SECTOR as u64 + 1, &fs_info())?;

    // Both FATs: cluster 0 holds the media byte, cluster 1 the end marker, and
    // cluster 2 is the root directory's single, final cluster.
    let mut first_fat_sector = vec![0u8; SECTOR as usize];
    first_fat_sector[0..4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
    first_fat_sector[4..8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
    first_fat_sector[8..12].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());

    let zero_sector = vec![0u8; SECTOR as usize];
    for fat in 0..NUM_FATS as u64 {
        let fat_start = RESERVED_SECTORS as u64 + fat * fat_size as u64;
        write_at(dev, base, fat_start, &first_fat_sector)?;
        for s in 1..fat_size as u64 {
            write_at(dev, base, fat_start + s, &zero_sector)?;
        }
    }

    // An empty root directory: the whole cluster must read as free entries.
    let data_start = RESERVED_SECTORS as u64 + fat_size as u64 * NUM_FATS as u64;
    for s in 0..SECTORS_PER_CLUSTER as u64 {
        write_at(dev, base, data_start + s, &zero_sector)?;
    }

    dev.flush()
}
