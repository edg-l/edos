//! Enough of the GUID Partition Table (UEFI 2.10 §5.3) to learn how long a
//! partition is.
//!
//! The formatter is pointed at a byte offset inside an image and has to know
//! where that partition ends. Taking everything from the offset to the end of
//! the file instead runs the filesystem over the backup GPT header and entry
//! array, which §5.3.1 places in the last 33 sectors of the disk: standard
//! tools then report the image as damaged and a lost primary header cannot be
//! recovered from the backup.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use efs_common::crc32::crc32;

/// LBA size assumed for the table. A partition offset is expressed in bytes
/// and the header is read at byte 512, so a 4Kn disk simply has no signature
/// there and the caller falls back.
const SECTOR: u64 = 512;

const SIGNATURE: &[u8; 8] = b"EFI PART";

/// Fields the header carries, at the offsets §5.3.2 gives them.
const OFF_HEADER_SIZE: usize = 12;
const OFF_HEADER_CRC: usize = 16;
const OFF_ENTRY_LBA: usize = 72;
const OFF_ENTRY_COUNT: usize = 80;
const OFF_ENTRY_SIZE: usize = 84;

/// Fields of one entry, at the offsets §5.3.3 gives them.
const OFF_TYPE_GUID: usize = 0;
const OFF_FIRST_LBA: usize = 32;
const OFF_LAST_LBA: usize = 40;

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn u64_at(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn read_at(file: &mut File, offset: u64, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// Length in bytes of the partition that begins at `offset`, read out of the
/// image's primary GPT.
///
/// `None` when the image carries no usable table or no partition starts
/// exactly there, which is the ordinary case for a bare filesystem image with
/// no partitioning at all.
pub fn partition_size_at(file: &mut File, offset: u64) -> Option<u64> {
    if !offset.is_multiple_of(SECTOR) {
        return None;
    }

    let header = read_at(file, SECTOR, SECTOR as usize)?;
    if &header[..8] != SIGNATURE {
        return None;
    }

    // The CRC covers `header_size` bytes with the CRC field itself zeroed.
    // Checking it is what makes the numbers below worth trusting.
    let header_size = u32_at(&header, OFF_HEADER_SIZE) as usize;
    if !(92..=SECTOR as usize).contains(&header_size) {
        return None;
    }
    let mut hashed = header[..header_size].to_vec();
    hashed[OFF_HEADER_CRC..OFF_HEADER_CRC + 4].fill(0);
    if crc32(&hashed) != u32_at(&header, OFF_HEADER_CRC) {
        return None;
    }

    let entry_lba = u64_at(&header, OFF_ENTRY_LBA);
    let entry_count = u32_at(&header, OFF_ENTRY_COUNT) as usize;
    let entry_size = u32_at(&header, OFF_ENTRY_SIZE) as usize;
    if !(128..=4096).contains(&entry_size) || entry_count == 0 || entry_count > 4096 {
        return None;
    }

    let entries = read_at(file, entry_lba * SECTOR, entry_count * entry_size)?;
    for entry in entries.chunks_exact(entry_size) {
        if entry[OFF_TYPE_GUID..OFF_TYPE_GUID + 16] == [0u8; 16] {
            continue;
        }
        let first = u64_at(entry, OFF_FIRST_LBA);
        let last = u64_at(entry, OFF_LAST_LBA);
        if first * SECTOR == offset && last >= first {
            return Some((last - first + 1) * SECTOR);
        }
    }
    None
}
