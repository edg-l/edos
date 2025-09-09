use crate::{drivers::ahci, fs::fat32::structures::Fat32BootSector};
use alloc::{string::String, vec::Vec};
use bytemuck::{Pod, Zeroable, try_from_bytes};

/// GPT Header structure (LBA 1)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GptHeader {
    pub signature: [u8; 8],         // "EFI PART"
    pub revision: u32,              // GPT revision
    pub header_size: u32,           // Size of this header
    pub header_crc32: u32,          // CRC32 of header
    pub reserved: u32,              // Must be zero
    pub my_lba: u64,                // LBA of this header
    pub alternate_lba: u64,         // LBA of alternate header
    pub first_usable_lba: u64,      // First usable LBA for partitions
    pub last_usable_lba: u64,       // Last usable LBA for partitions
    pub disk_guid: [u8; 16],        // Disk GUID
    pub partition_entry_lba: u64,   // LBA of partition entry array
    pub num_partition_entries: u32, // Number of partition entries
    pub partition_entry_size: u32,  // Size of each partition entry
    pub partition_array_crc32: u32, // CRC32 of partition array
}

/// GPT Partition Entry structure
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GptPartitionEntry {
    pub partition_type_guid: [u8; 16],   // Partition type GUID
    pub unique_partition_guid: [u8; 16], // Unique partition GUID
    pub starting_lba: u64,               // Starting LBA
    pub ending_lba: u64,                 // Ending LBA
    pub attributes: u64,                 // Partition attributes
    pub partition_name: [u16; 36],       // Partition name (UTF-16)
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub index: u32,
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub size_sectors: u64,
    pub partition_type: PartitionType,
    pub name: String,
    pub filesystem: Option<FilesystemType>,
    pub device_id: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PartitionType {
    EfiSystem,
    MicrosoftBasicData,
    LinuxFilesystem,
    LinuxSwap,
    Unknown([u8; 16]),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilesystemType {
    Fat32,
    Unknown,
}

impl GptHeader {
    /// Check if the GPT signature is valid
    pub fn is_valid(&self) -> bool {
        &self.signature == b"EFI PART"
    }
}

impl GptPartitionEntry {
    /// Check if this partition entry is used (not all zeros)
    pub fn is_used(&self) -> bool {
        self.partition_type_guid != [0u8; 16]
    }

    /// Get partition size in sectors
    pub fn size_sectors(&self) -> u64 {
        if self.ending_lba >= self.starting_lba {
            self.ending_lba - self.starting_lba + 1
        } else {
            0
        }
    }

    /// Parse partition name from UTF-16
    pub fn name(&self) -> String {
        let mut name = String::new();
        for &utf16_char in &self.partition_name {
            if utf16_char == 0 {
                break;
            }
            if let Some(ch) = char::from_u32(utf16_char as u32) {
                name.push(ch);
            }
        }
        name
    }

    /// Determine partition type from GUID
    pub fn partition_type(&self) -> PartitionType {
        match &self.partition_type_guid {
            // EFI System Partition
            &[
                0x28,
                0x73,
                0x2A,
                0xC1,
                0x1F,
                0xF8,
                0xD2,
                0x11,
                0xBA,
                0x4B,
                0x00,
                0xA0,
                0xC9,
                0x3E,
                0xC9,
                0x3B,
            ] => PartitionType::EfiSystem,
            // Microsoft Basic Data Partition
            &[
                0xA2,
                0xA0,
                0xD0,
                0xEB,
                0xE5,
                0xB9,
                0x33,
                0x44,
                0x87,
                0xC0,
                0x68,
                0xB6,
                0xB7,
                0x26,
                0x99,
                0xC7,
            ] => PartitionType::MicrosoftBasicData,
            // Linux Filesystem
            &[
                0xAF,
                0x3D,
                0xC6,
                0x0F,
                0x83,
                0x84,
                0x72,
                0x47,
                0x8E,
                0x79,
                0x3D,
                0x69,
                0xD8,
                0x47,
                0x7D,
                0xE4,
            ] => PartitionType::LinuxFilesystem,
            // Linux Swap
            &[
                0x6D,
                0xFD,
                0x57,
                0x06,
                0xAB,
                0xA4,
                0xC4,
                0x44,
                0x84,
                0xE5,
                0x09,
                0x33,
                0xC8,
                0x4B,
                0x4F,
                0x4F,
            ] => PartitionType::LinuxSwap,
            guid => PartitionType::Unknown(*guid),
        }
    }
}

/// Parse GPT from a device
pub fn parse_gpt(device_id: u64) -> Result<Vec<Partition>, &'static str> {
    // Read GPT header from LBA 1
    let gpt_data =
        ahci::api::read_sectors(device_id, 1, 1).map_err(|_| "Failed to read GPT header")?;

    if gpt_data.len() < core::mem::size_of::<GptHeader>() {
        return Err("GPT data too small");
    }

    let gpt_header = try_from_bytes::<GptHeader>(&gpt_data[0..core::mem::size_of::<GptHeader>()])
        .map_err(|_| "Failed to parse GPT header")?;

    if !gpt_header.is_valid() {
        return Err("Invalid GPT signature");
    }

    // Calculate how many sectors we need to read for partition entries
    let entries_per_sector = 512 / gpt_header.partition_entry_size as usize;
    let sectors_needed = (gpt_header.num_partition_entries as usize).div_ceil(entries_per_sector);

    // Read partition entries
    let partition_data = ahci::api::read_sectors(
        device_id,
        gpt_header.partition_entry_lba,
        sectors_needed as u16,
    )
    .map_err(|_| "Failed to read partition entries")?;

    let mut partitions = Vec::new();

    // Parse each partition entry
    for i in 0..gpt_header.num_partition_entries {
        let entry_offset = (i as usize) * (gpt_header.partition_entry_size as usize);

        if entry_offset + core::mem::size_of::<GptPartitionEntry>() > partition_data.len() {
            break;
        }

        let entry = try_from_bytes::<GptPartitionEntry>(
            &partition_data[entry_offset..entry_offset + core::mem::size_of::<GptPartitionEntry>()],
        )
        .map_err(|_| "Failed to parse partition entry")?;

        if entry.is_used() {
            let filesystem = detect_filesystem(device_id, entry.starting_lba)?;

            partitions.push(Partition {
                index: i,
                starting_lba: entry.starting_lba,
                ending_lba: entry.ending_lba,
                size_sectors: entry.size_sectors(),
                partition_type: entry.partition_type(),
                name: entry.name(),
                filesystem,
                device_id,
            });
        }
    }

    Ok(partitions)
}

/// Detect filesystem type by reading the first sector of a partition
fn detect_filesystem(
    device_id: u64,
    partition_start_lba: u64,
) -> Result<Option<FilesystemType>, &'static str> {
    // Read first sector of partition
    let sector_data = ahci::api::read_sectors(device_id, partition_start_lba, 1)
        .map_err(|_| "Failed to read partition boot sector")?;

    if sector_data.len() < core::mem::size_of::<Fat32BootSector>() {
        return Ok(Some(FilesystemType::Unknown));
    }

    // Try to parse as FAT32 boot sector
    let boot_sector =
        try_from_bytes::<Fat32BootSector>(&sector_data[0..core::mem::size_of::<Fat32BootSector>()])
            .map_err(|_| "Failed to parse boot sector")?;

    if boot_sector.is_fat32() {
        Ok(Some(FilesystemType::Fat32))
    } else {
        Ok(Some(FilesystemType::Unknown))
    }
}

/// Pretty print partition information
pub fn print_partitions(partitions: &[Partition]) {
    use crate::println;

    println!("Found {} partitions:", partitions.len());
    println!(
        "{:<3} {:<12} {:<12} {:<12} {:<20} {:<10} {}",
        "ID", "Start LBA", "End LBA", "Size (MB)", "Type", "FS", "Name"
    );
    println!("{}", "-".repeat(80));

    for partition in partitions {
        let size_mb = (partition.size_sectors * 512) / (1024 * 1024);
        let type_str = match &partition.partition_type {
            PartitionType::EfiSystem => "EFI System",
            PartitionType::MicrosoftBasicData => "MS Basic Data",
            PartitionType::LinuxFilesystem => "Linux FS",
            PartitionType::LinuxSwap => "Linux Swap",
            PartitionType::Unknown(_) => "Unknown",
        };
        let fs_str = match &partition.filesystem {
            Some(FilesystemType::Fat32) => "FAT32",
            Some(FilesystemType::Unknown) => "Unknown",
            None => "None",
        };

        println!(
            "{:<3} {:<12} {:<12} {:<12} {:<20} {:<10} {}",
            partition.index,
            partition.starting_lba,
            partition.ending_lba,
            size_mb,
            type_str,
            fs_str,
            partition.name
        );
    }
}
