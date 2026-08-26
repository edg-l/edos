use crate::{
    drivers::block_io::{self, BlockError},
    fs::fat32::structures::Fat32BootSector,
    fs::gpt::{FilesystemType, Partition, PartitionType},
    log,
};

fn read_sectors_vec(
    device_id: u64,
    lba: u64,
    sectors: u16,
    _buf: Vec<u8>,
) -> Result<Vec<u8>, BlockError> {
    let byte_count = sectors as usize * 512;
    let mut buf = alloc::vec![0u8; byte_count];
    let dev = block_io::lookup(device_id).ok_or(BlockError::DeviceGone)?;
    block_io::read_blocking(&dev, lba, &mut buf)?;
    Ok(buf)
}
use alloc::{format, string::String, vec::Vec};
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
            0xEF => PartitionType::EfiSystemMbr,
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
            0xEF => "EFI System",
            _ => "Unknown",
        };
        format!("MBR Partition {} ({})", index + 1, type_name)
    }
}

/// Parse MBR from a device
pub fn parse_mbr(device_id: u64) -> Result<Vec<Partition>, &'static str> {
    // Read MBR from LBA 0
    let mbr_data =
        read_sectors_vec(device_id, 0, 1, Vec::new()).map_err(|_| "Failed to read MBR")?;

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

    // Only the four primary entries. An extended partition is reported as a
    // partition of its own and the logical drives chained inside it are not
    // walked.

    Ok(partitions)
}

/// EFS superblock magic: "EFS!" in little-endian (0x45465321)
const EFS_MAGIC: u32 = 0x45465321;

/// Detect filesystem type by reading the first sector of a partition
fn detect_filesystem(
    device_id: u64,
    partition_start_lba: u64,
) -> Result<Option<FilesystemType>, &'static str> {
    // Read first sector of partition
    let sector_data = read_sectors_vec(device_id, partition_start_lba, 1, Vec::new())
        .map_err(|_| "Failed to read partition boot sector")?;

    if sector_data.len() < 512 {
        return Ok(Some(FilesystemType::Unknown));
    }

    log!(
        "Detecting filesystem at LBA {}, got {} bytes",
        partition_start_lba,
        sector_data.len()
    );

    // Check for EFS first: superblock is at block 1 (4 KiB blocks = 8 sectors).
    let efs_lba = partition_start_lba + 8;
    if let Ok(efs_data) = read_sectors_vec(device_id, efs_lba, 8, Vec::new())
        && efs_data.len() >= 4
    {
        let magic = u32::from_le_bytes([efs_data[0], efs_data[1], efs_data[2], efs_data[3]]);
        if magic == EFS_MAGIC {
            log!("Detected EFS filesystem");
            return Ok(Some(FilesystemType::Efs));
        }
    }

    // Try ISO 9660 detection first (most likely for CD/DVD)
    if let Some(fs_type) = detect_iso9660(device_id, partition_start_lba).unwrap_or(None) {
        log!("Detected ISO 9660 filesystem");
        return Ok(Some(fs_type));
    }

    // Try FAT family detection (for EFI boot partitions)
    if let Some(fs_type) = detect_fat_family(&sector_data) {
        log!("Detected FAT filesystem: {:?}", fs_type);
        return Ok(Some(fs_type));
    }

    // Try NTFS detection
    if let Some(fs_type) = detect_ntfs(&sector_data) {
        log!("Detected NTFS filesystem");
        return Ok(Some(fs_type));
    }

    // Check for basic boot signature
    if sector_data.len() >= 512 && sector_data[510] == 0x55 && sector_data[511] == 0xAA {
        log!("Found boot signature (0x55AA) but unknown filesystem");
    } else {
        log!("No boot signature found");
    }

    log!("Could not identify filesystem type");
    Ok(Some(FilesystemType::Unknown))
}

/// Detect NTFS filesystem
fn detect_ntfs(sector_data: &[u8]) -> Option<FilesystemType> {
    if sector_data.len() < 512 {
        return None;
    }

    // Check for NTFS OEM identifier at offset 3-10 ("NTFS    ")
    if &sector_data[3..11] == b"NTFS    " {
        // Additional validation: check bytes per sector (should be 512, 1024, 2048, or 4096)
        let bytes_per_sector = u16::from_le_bytes([sector_data[11], sector_data[12]]);
        if matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
            return Some(FilesystemType::Ntfs);
        }
    }

    None
}

/// Detect ISO 9660 filesystem
fn detect_iso9660(
    device_id: u64,
    partition_start_lba: u64,
) -> Result<Option<FilesystemType>, &'static str> {
    // ISO 9660 Primary Volume Descriptor is at sector 16 (relative to partition start)
    // But for hybrid ISOs, we need to check if this partition actually contains ISO data
    let iso_check_lba = partition_start_lba + 16;

    log!(
        "Checking for ISO 9660 at LBA {} (partition {} + 16)",
        iso_check_lba,
        partition_start_lba
    );

    // Read sector 16 relative to partition start
    let iso_sector = read_sectors_vec(device_id, iso_check_lba, 1, Vec::new())
        .map_err(|_| "Failed to read ISO 9660 volume descriptor sector")?;

    if iso_sector.len() < 512 {
        return Ok(None);
    }

    // Check for ISO 9660 Primary Volume Descriptor signature
    if iso_sector.len() >= 6 {
        let signature = &iso_sector[1..6]; // "CD001" at bytes 1-5
        if signature == b"CD001" {
            // Additional validation: check volume descriptor type (should be 1 for Primary VD)
            if iso_sector[0] == 0x01 {
                log!("Found ISO 9660 Primary Volume Descriptor signature");
                return Ok(Some(FilesystemType::Iso9660));
            }
        }
    }

    // For hybrid ISOs, also check if the partition itself looks like ISO data
    // Many hybrid ISOs embed the ISO filesystem starting from the partition
    if partition_start_lba > 0 {
        // Try reading ISO signature directly from partition start + common ISO offsets
        for offset in [0, 16, 32] {
            if let Ok(test_sector) =
                read_sectors_vec(device_id, partition_start_lba + offset, 1, Vec::new())
                && test_sector.len() >= 6
                && &test_sector[1..6] == b"CD001"
                && test_sector[0] == 0x01
            {
                log!("Found ISO 9660 signature at partition offset {}", offset);
                return Ok(Some(FilesystemType::Iso9660));
            }
        }
    }

    Ok(None)
}

/// Detect FAT family filesystems (FAT12, FAT16, FAT32)
fn detect_fat_family(sector_data: &[u8]) -> Option<FilesystemType> {
    if sector_data.len() < core::mem::size_of::<Fat32BootSector>() {
        log!("Sector too small for FAT detection");
        return None;
    }

    // Check for boot signature first (0x55AA at end)
    if sector_data[510] != 0x55 || sector_data[511] != 0xAA {
        log!("No boot signature for FAT detection");
        return None;
    }

    // Try to parse as FAT boot sector
    let boot_sector =
        try_from_bytes::<Fat32BootSector>(&sector_data[0..core::mem::size_of::<Fat32BootSector>()])
            .ok()?;

    let bytes_per_sector = boot_sector.bytes_per_sector;
    let sectors_per_cluster = boot_sector.sectors_per_cluster;
    log!(
        "FAT boot sector parsed - bytes_per_sector: {}, sectors_per_cluster: {}",
        bytes_per_sector,
        sectors_per_cluster
    );

    // Check if it's FAT32 first (strict check)
    if boot_sector.is_fat32() {
        log!("Strict FAT32 validation passed");
        return Some(FilesystemType::Fat32);
    }

    // Check for FAT12/FAT16 characteristics (more lenient for EFI partitions)
    if is_fat16_or_fat12_lenient(boot_sector) {
        // Determine if it's FAT12 or FAT16 based on cluster count
        let cluster_count = calculate_cluster_count(boot_sector);
        log!("FAT12/16 candidate with {} clusters", cluster_count);

        if cluster_count == 0 {
            // If cluster calculation fails, use heuristics
            detect_fat_by_heuristics(boot_sector)
        } else if cluster_count < 4085 {
            Some(FilesystemType::Fat12)
        } else if cluster_count < 65525 {
            Some(FilesystemType::Fat16)
        } else {
            // Shouldn't happen if not FAT32, but handle gracefully
            None
        }
    } else {
        // Try heuristic detection for unusual EFI partitions
        log!("Strict FAT12/16 validation failed, trying heuristics");
        detect_fat_by_heuristics(boot_sector)
    }
}

/// More lenient FAT12/16 detection for EFI boot partitions
fn is_fat16_or_fat12_lenient(boot_sector: &Fat32BootSector) -> bool {
    // Basic validation (more lenient than original)
    boot_sector.bytes_per_sector == 512 &&
    boot_sector.sectors_per_cluster > 0 &&
    boot_sector.sectors_per_cluster <= 128 &&  // Relaxed: allow up to 128
    boot_sector.num_fats > 0 &&
    boot_sector.num_fats <= 2 &&  // Usually 1 or 2
    boot_sector.fat_size_16 > 0 &&  // FAT12/16 uses 16-bit FAT size
    boot_sector.total_sectors_32 == 0 // FAT12/16 typically uses 16-bit sector count
}

/// Heuristic FAT detection for unusual boot sectors
fn detect_fat_by_heuristics(boot_sector: &Fat32BootSector) -> Option<FilesystemType> {
    // Check OEM name for common FAT signatures
    let oem_str = core::str::from_utf8(&boot_sector.oem_name).unwrap_or("");
    log!("OEM name: '{}'", oem_str);

    if oem_str.contains("MSDOS") || oem_str.contains("mkfs") || oem_str.contains("FAT") {
        // If we have reasonable sector size and cluster size, assume FAT16
        let bytes_per_sector = boot_sector.bytes_per_sector;
        let sectors_per_cluster = boot_sector.sectors_per_cluster;
        if bytes_per_sector == 512 && sectors_per_cluster > 0 && sectors_per_cluster <= 64 {
            log!("Heuristic detection: assuming FAT16 based on OEM and geometry");
            return Some(FilesystemType::Fat16);
        }
    }

    // Check for jump instruction (common FAT signature)
    let jmp_first = boot_sector.jmp_boot[0];
    let bytes_per_sector = boot_sector.bytes_per_sector;
    if jmp_first == 0xEB || jmp_first == 0xE9 {
        log!("Found jump instruction, might be FAT");
        if bytes_per_sector == 512 {
            return Some(FilesystemType::Fat16); // Default guess
        }
    }

    None
}

/// Check if boot sector has FAT12/FAT16 characteristics
#[expect(unused)]
fn is_fat16_or_fat12(boot_sector: &Fat32BootSector) -> bool {
    // Basic FAT validation
    boot_sector.bytes_per_sector == 512 &&
    boot_sector.sectors_per_cluster > 0 &&
    boot_sector.sectors_per_cluster.is_power_of_two() &&
    boot_sector.num_fats > 0 &&
    boot_sector.root_entry_count > 0 &&  // FAT12/16 has root directory entries
    boot_sector.fat_size_16 > 0 &&       // FAT12/16 uses 16-bit FAT size
    // Check for FAT12/16 filesystem type strings
    (boot_sector.file_system_type.starts_with(b"FAT12") ||
     boot_sector.file_system_type.starts_with(b"FAT16") ||
     boot_sector.file_system_type.starts_with(b"FAT  "))
}

/// Calculate the cluster count for FAT filesystem determination
fn calculate_cluster_count(boot_sector: &Fat32BootSector) -> u32 {
    let total_sectors = if boot_sector.total_sectors_16 != 0 {
        boot_sector.total_sectors_16 as u32
    } else {
        boot_sector.total_sectors_32
    };

    let fat_sectors = boot_sector.fat_size_16 as u32;
    let root_dir_sectors = (boot_sector.root_entry_count as u32 * 32).div_ceil(512);

    let data_sectors = total_sectors
        .saturating_sub(boot_sector.reserved_sector_count as u32)
        .saturating_sub(boot_sector.num_fats as u32 * fat_sectors)
        .saturating_sub(root_dir_sectors);

    data_sectors / boot_sector.sectors_per_cluster as u32
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
pub fn print_partitions(partitions: &[Partition]) {
    log!("Found {} MBR partitions:", partitions.len());
    log!(
        "{:<3} {:<12} {:<12} {:<12} {:<20} {:<10} {}",
        "ID",
        "Start LBA",
        "End LBA",
        "Size (MB)",
        "Type",
        "FS",
        "Name"
    );
    log!("{}", "-".repeat(80));

    for partition in partitions {
        let size_mb = (partition.size_sectors * 512) / (1024 * 1024);
        let fs_str = partition
            .filesystem
            .as_ref()
            .map_or("None", FilesystemType::name);

        log!(
            "{:<3} {:<12} {:<12} {:<12} {:<20} {:<10} {}",
            partition.index,
            partition.starting_lba,
            partition.ending_lba,
            size_mb,
            partition.partition_type,
            fs_str,
            partition.name
        );
    }
}
