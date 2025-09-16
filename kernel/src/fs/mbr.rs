use crate::{
    drivers::ahci,
    fs::fat32::structures::Fat32BootSector,
    fs::gpt::{FilesystemType, Partition, PartitionType},
    log,
    logs::ThreadLogger,
    thread::scheduler::sched,
};
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use bytemuck::{Pod, Zeroable, try_from_bytes};

/// MBR structure (sector 0)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct MbrHeader {
    pub bootloader: [u8; 440],              // Bootstrap code
    pub disk_id: u32,                       // Disk signature
    pub reserved: u16,                      // Reserved, usually 0x0000
    pub partitions: [MbrPartitionEntry; 4], // 4 partition entries
    pub signature: [u8; 2],                 // Boot signature (0x55, 0xAA)
}

/// MBR Partition Entry structure
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct MbrPartitionEntry {
    pub status: u8,         // Boot indicator (0x80 = bootable, 0x00 = non-bootable)
    pub start_chs: [u8; 3], // Starting CHS address
    pub partition_type: u8, // Partition type ID
    pub end_chs: [u8; 3],   // Ending CHS address
    pub start_lba: u32,     // Starting LBA
    pub size_sectors: u32,  // Size in sectors
}

impl MbrHeader {
    /// Check if the MBR signature is valid
    pub fn is_valid(&self) -> bool {
        self.signature == [0x55, 0xAA]
    }
}

impl MbrPartitionEntry {
    /// Check if this partition entry is used (partition type != 0)
    pub fn is_used(&self) -> bool {
        self.partition_type != 0 && self.size_sectors > 0
    }

    /// Check if this is an extended partition
    pub fn is_extended(&self) -> bool {
        matches!(self.partition_type, 0x05 | 0x0F | 0x85)
    }

    /// Get partition size in sectors (already stored directly)
    pub fn size_sectors(&self) -> u64 {
        self.size_sectors as u64
    }

    /// Get starting LBA as u64
    pub fn starting_lba(&self) -> u64 {
        self.start_lba as u64
    }

    /// Get ending LBA
    pub fn ending_lba(&self) -> u64 {
        if self.size_sectors > 0 {
            self.start_lba as u64 + self.size_sectors as u64 - 1
        } else {
            self.start_lba as u64
        }
    }

    /// Determine partition type from MBR type ID
    pub fn partition_type(&self) -> PartitionType {
        match self.partition_type {
            0x01 => PartitionType::Fat12,
            0x04 => PartitionType::Fat16Small,
            0x06 => PartitionType::Fat16,
            0x0B | 0x0C => PartitionType::Fat32,
            0x07 => PartitionType::Ntfs,
            0x82 => PartitionType::LinuxSwap,
            0x83 => PartitionType::LinuxFilesystem,
            0x05 | 0x0F | 0x85 => PartitionType::Extended,
            type_id => PartitionType::MbrUnknown(type_id),
        }
    }

    /// Generate a simple name for MBR partitions
    pub fn name(&self, index: usize) -> String {
        let type_name = match self.partition_type {
            0x0B | 0x0C => "FAT32",
            0x07 => "NTFS",
            0x82 => "Linux Swap",
            0x83 => "Linux",
            0x05 | 0x0F => "Extended",
            _ => "Unknown",
        };
        format!("MBR Partition {} ({})", index + 1, type_name)
    }
}

/// Parse MBR from a device
pub fn parse_mbr(device_id: u64) -> Result<Vec<Partition>, &'static str> {
    // Read MBR from LBA 0
    let mbr_data =
        ahci::api::read_sectors(device_id, 0, 1, Vec::new()).map_err(|_| "Failed to read MBR")?;

    if mbr_data.len() < core::mem::size_of::<MbrHeader>() {
        return Err("MBR data too small");
    }

    let mbr_header = try_from_bytes::<MbrHeader>(&mbr_data[0..core::mem::size_of::<MbrHeader>()])
        .map_err(|_| "Failed to parse MBR header")?;

    if !mbr_header.is_valid() {
        return Err("Invalid MBR signature");
    }

    let mut partitions = Vec::new();

    // Parse primary partitions
    for (i, entry) in mbr_header.partitions.iter().enumerate() {
        if entry.is_used() && !entry.is_extended() {
            let filesystem = detect_filesystem(device_id, entry.starting_lba())?;

            partitions.push(Partition {
                index: i,
                starting_lba: entry.starting_lba(),
                ending_lba: entry.ending_lba(),
                size_sectors: entry.size_sectors(),
                partition_type: entry.partition_type(),
                name: entry.name(i),
                filesystem,
                device_id,
                unique_partition_guid: generate_mbr_guid(device_id, i),
            });
        }
    }

    // TODO: Handle extended partitions and logical drives
    // For now, we only parse primary partitions

    Ok(partitions)
}

/// Detect filesystem type by reading the first sector of a partition
fn detect_filesystem(
    device_id: u64,
    partition_start_lba: u64,
) -> Result<Option<FilesystemType>, &'static str> {
    // Read first sector of partition
    let sector_data = ahci::api::read_sectors(device_id, partition_start_lba, 1, Vec::new())
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

/// Generate a pseudo-GUID for MBR partitions for compatibility
fn generate_mbr_guid(device_id: u64, partition_index: usize) -> [u8; 16] {
    let mut guid = [0u8; 16];

    // Use device_id and partition_index to create a deterministic "GUID"
    guid[0..8].copy_from_slice(&device_id.to_le_bytes());
    guid[8..12].copy_from_slice(&(partition_index as u32).to_le_bytes());
    // Fill the rest with a pattern to indicate this is MBR-derived
    guid[12..16].copy_from_slice(&[0x4D, 0x42, 0x52, 0x00]); // "MBR" marker

    guid
}

/// Pretty print MBR partition information
pub fn print_partitions(partitions: &[Partition], logger: &ThreadLogger) {
    log!(logger, "Found {} MBR partitions:", partitions.len());
    log!(
        logger,
        "{:<3} {:<12} {:<12} {:<12} {:<20} {:<10} {}",
        "ID",
        "Start LBA",
        "End LBA",
        "Size (MB)",
        "Type",
        "FS",
        "Name"
    );
    log!(logger, "{}", "-".repeat(80));

    for partition in partitions {
        let size_mb = (partition.size_sectors * 512) / (1024 * 1024);
        let type_str = match &partition.partition_type {
            PartitionType::Fat32 => "FAT32".to_string(),
            PartitionType::Fat16 => "FAT16".to_string(),
            PartitionType::Fat16Small => "FAT16 Small".to_string(),
            PartitionType::Fat12 => "FAT12".to_string(),
            PartitionType::Ntfs => "NTFS".to_string(),
            PartitionType::LinuxFilesystem => "Linux FS".to_string(),
            PartitionType::LinuxSwap => "Linux Swap".to_string(),
            PartitionType::Extended => "Extended".to_string(),
            PartitionType::MbrUnknown(id) => format!("Unknown (0x{:02X})", id),
            _ => "Other".to_string(),
        };
        let fs_str = match &partition.filesystem {
            Some(FilesystemType::Fat32) => "FAT32",
            Some(FilesystemType::Unknown) => "Unknown",
            None => "None",
        };

        log!(
            logger,
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
