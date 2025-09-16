use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use bytemuck::{NoUninit, Pod, Zeroable};

use crate::fs::{File, FileAttrs, FileKind, FileTime};

/// FAT filesystem variant
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatVariant {
    Fat12,
    Fat16,
    Fat32,
}

/// FAT32 Boot Sector (exactly 512 bytes).
/// This is the on-disk BIOS Parameter Block (BPB) + Extended BPB + boot code.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Fat32BootSector {
    /// Jump instruction to boot code (e.g., EB xx 90).
    pub jmp_boot: [u8; 3],

    /// OEM name (ASCII, often "MSDOS5.0").
    pub oem_name: [u8; 8],

    /// Bytes per sector. Legal values: 512, 1024, 2048, 4096.
    pub bytes_per_sector: u16,

    /// Sectors per cluster. Must be a power of 2 (1,2,4,...128).
    pub sectors_per_cluster: u8,

    /// Reserved sector count. Includes the boot sector itself. Usually 32.
    pub reserved_sector_count: u16,

    /// Number of FAT copies. Usually 2.
    pub num_fats: u8,

    /// Number of root directory entries (FAT12/16 only). Always 0 on FAT32.
    pub root_entry_count: u16,

    /// Total sectors if < 65536. Zero for FAT32, use `total_sectors_32` instead.
    pub total_sectors_16: u16,

    /// Media descriptor. 0xF8 for fixed disk.
    pub media: u8,

    /// FAT size in sectors (FAT12/16 only). Zero for FAT32, use `fat_size_32`.
    pub fat_size_16: u16,

    /// Sectors per track (for BIOS CHS geometry).
    pub sectors_per_track: u16,

    /// Number of heads (for BIOS CHS geometry).
    pub num_heads: u16,

    /// Hidden sectors before this partition (for MBR). 0 if none.
    pub hidden_sectors: u32,

    /// Total sectors if >= 65536. Used for FAT32.
    pub total_sectors_32: u32,

    /// Size of each FAT in sectors (FAT32 only).
    pub fat_size_32: u32,

    /// Extended flags: bit7..0 = active FAT, bit8 = FAT mirroring disabled.
    pub ext_flags: u16,

    /// Filesystem version. Should be 0.
    pub fs_version: u16,

    /// First cluster number of the root directory (usually 2).
    pub root_cluster: u32,

    /// Sector number of FSInfo structure (relative to boot sector, usually 1).
    pub fs_info: u16,

    /// Sector number of backup boot sector (usually 6).
    pub backup_boot_sector: u16,

    /// Reserved for future expansion. Must be zero.
    pub _reserved: [u8; 12],

    /// BIOS drive number (0x80 = first hard disk).
    pub drive_number: u8,

    /// Reserved, not used. Must be zero.
    pub _reserved1: u8,

    /// Extended boot signature (0x29 if `volume_id`, `volume_label`, `file_system_type` valid).
    pub boot_signature: u8,

    /// Volume serial number, set at format time.
    pub volume_id: u32,

    /// Volume label (ASCII, 11 bytes, space padded).
    pub volume_label: [u8; 11],

    /// Filesystem type string (ASCII, "FAT32   ").
    pub file_system_type: [u8; 8],

    /// Bootstrap code area. May contain bootloader program.
    pub boot_code: [u8; 420],

    /// Boot sector signature. Must be 0xAA55.
    pub boot_sector_signature: u16,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct FsInfo {
    pub lead_signature: u32,   // 0x41615252 ("RRaA")
    pub reserved1: [u8; 480],  // Reserved (must be zero)
    pub struct_signature: u32, // 0x61417272 ("rrAa")
    pub free_count: u32,       // Free cluster count (0xFFFFFFFF if unknown)
    pub next_free: u32,        // Next free cluster hint (0xFFFFFFFF if unknown)
    pub reserved2: [u8; 12],   // Reserved (must be zero)
    pub trail_signature: u32,  // 0xAA550000
}

/// Directory Entry (32 bytes)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct DirectoryEntry {
    pub name: [u8; 11],          // 8.3 filename
    pub attributes: u8,          // File attributes
    pub nt_reserved: u8,         // Reserved for NT
    pub creation_time_tenth: u8, // Creation time (tenths of second)
    pub creation_time: u16,      // Creation time
    pub creation_date: u16,      // Creation date
    pub last_access_date: u16,   // Last access date
    pub first_cluster_high: u16, // High 16 bits of first cluster
    pub write_time: u16,         // Write time
    pub write_date: u16,         // Write date
    pub first_cluster_low: u16,  // Low 16 bits of first cluster
    pub file_size: u32,          // File size in bytes
}

/// Long Filename Entry (32 bytes)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct LongFilenameEntry {
    pub order: u8,              // Order of this entry
    pub name1: [u16; 5],        // First 5 UTF-16 characters
    pub attributes: u8,         // Always 0x0F for LFN
    pub entry_type: u8,         // Always 0 for LFN
    pub checksum: u8,           // Checksum of short name
    pub name2: [u16; 6],        // Next 6 UTF-16 characters
    pub first_cluster_low: u16, // Always 0 for LFN
    pub name3: [u16; 2],        // Last 2 UTF-16 characters
}

// File attributes
pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LONG_NAME: u8 = 0x0F; // LFN entry

// FAT32 cluster values
pub const CLUSTER_FREE: u32 = 0x00000000;
pub const CLUSTER_BAD: u32 = 0x0FFFFFF7;
pub const CLUSTER_EOF: u32 = 0x0FFFFFF8; // End of chain (0x0FFFFFF8 - 0x0FFFFFFF)
pub const FAT32_MASK: u32 = 0x0FFFFFFF;
pub const FAT16_MASK: u32 = 0x0000FFFF;
pub const FAT12_MASK: u32 = 0x00000FFF;

// FAT12/16 specific constants
pub const FAT12_CLUSTER_COUNT_MAX: u32 = 4085;
pub const FAT16_CLUSTER_COUNT_MAX: u32 = 65525;

// FSInfo signatures
pub const FSINFO_LEAD_SIG: u32 = 0x41615252; // "RRaA"
pub const FSINFO_STRUCT_SIG: u32 = 0x61417272; // "rrAa"
pub const FSINFO_TRAIL_SIG: u32 = 0xAA550000;
pub const FSINFO_UNKNOWN: u32 = 0xFFFFFFFF;

impl Fat32BootSector {
    /// Get the first sector of the FAT table
    pub fn fat_start_sector(&self) -> u32 {
        self.reserved_sector_count as u32
    }

    /// Get the first sector of the data area
    pub fn data_start_sector(&self) -> u32 {
        self.fat_start_sector() + (self.num_fats as u32 * self.fat_size_32)
    }

    /// Convert cluster number to LBA
    pub fn cluster_to_lba(&self, cluster: u32) -> u32 {
        self.data_start_sector() + ((cluster - 2) * self.sectors_per_cluster as u32)
    }

    /// Get bytes per cluster
    pub fn bytes_per_cluster(&self) -> u32 {
        self.bytes_per_sector as u32 * self.sectors_per_cluster as u32
    }

    /// Get FSInfo sector LBA
    pub fn fsinfo_lba(&self) -> u32 {
        self.fs_info as u32
    }

    /// Check if this appears to be a valid FAT32 boot sector
    pub fn is_fat32(&self) -> bool {
        // Check basic FAT32 characteristics
        self.bytes_per_sector == 512 &&
        self.sectors_per_cluster > 0 &&
        self.sectors_per_cluster.is_power_of_two() &&
        self.num_fats > 0 &&
        self.root_entry_count == 0 &&  // FAT32 has no root directory entries in boot sector
        self.total_sectors_16 == 0 &&  // FAT32 uses 32-bit sector count
        self.fat_size_16 == 0 &&       // FAT32 uses 32-bit FAT size
        self.fat_size_32 > 0 &&
        self.boot_signature == 0x29 &&
        &self.file_system_type[0..5] == b"FAT32"
    }

    /// Determine FAT variant based on cluster count
    pub fn determine_fat_variant(&self) -> Option<FatVariant> {
        // Basic validation first
        if self.bytes_per_sector != 512
            || self.sectors_per_cluster == 0
            || !self.sectors_per_cluster.is_power_of_two()
            || self.num_fats == 0
        {
            return None;
        }

        let cluster_count = self.calculate_cluster_count();

        if cluster_count < FAT12_CLUSTER_COUNT_MAX {
            Some(FatVariant::Fat12)
        } else if cluster_count < FAT16_CLUSTER_COUNT_MAX {
            Some(FatVariant::Fat16)
        } else {
            Some(FatVariant::Fat32)
        }
    }

    /// Calculate the total number of clusters
    pub fn calculate_cluster_count(&self) -> u32 {
        let total_sectors = if self.total_sectors_16 != 0 {
            self.total_sectors_16 as u32
        } else {
            self.total_sectors_32
        };

        let fat_sectors = if self.fat_size_16 != 0 {
            self.fat_size_16 as u32
        } else {
            self.fat_size_32
        };

        let root_dir_sectors = ((self.root_entry_count as u32 * 32) + 511) / 512;

        let data_sectors = total_sectors
            .saturating_sub(self.reserved_sector_count as u32)
            .saturating_sub(self.num_fats as u32 * fat_sectors)
            .saturating_sub(root_dir_sectors);

        data_sectors / self.sectors_per_cluster as u32
    }
}

impl FsInfo {
    /// Check if FSInfo sector has valid signatures
    pub fn is_valid(&self) -> bool {
        self.lead_signature == FSINFO_LEAD_SIG
            && self.struct_signature == FSINFO_STRUCT_SIG
            && self.trail_signature == FSINFO_TRAIL_SIG
    }

    /// Check if free count is known
    pub fn has_free_count(&self) -> bool {
        self.free_count != FSINFO_UNKNOWN
    }

    /// Check if next free hint is known
    pub fn has_next_free_hint(&self) -> bool {
        self.next_free != FSINFO_UNKNOWN
    }

    /// Update free cluster count (decreases when allocating)
    pub fn update_free_count(&mut self, new_count: u32) {
        self.free_count = new_count;
    }

    /// Update next free cluster hint
    pub fn update_next_free(&mut self, next_cluster: u32) {
        self.next_free = next_cluster;
    }

    /// Create a new FSInfo sector
    pub fn new() -> Self {
        Self {
            lead_signature: FSINFO_LEAD_SIG,
            reserved1: [0; 480],
            struct_signature: FSINFO_STRUCT_SIG,
            free_count: FSINFO_UNKNOWN, // Will be calculated
            next_free: FSINFO_UNKNOWN,  // Will be found during scan
            reserved2: [0; 12],
            trail_signature: FSINFO_TRAIL_SIG,
        }
    }
}

impl DirectoryEntry {
    const NT_LOWER_BASE: u8 = 0x08;
    const NT_LOWER_EXT: u8 = 0x10;

    /// Get the full cluster number
    pub fn first_cluster(&self) -> u32 {
        ((self.first_cluster_high as u32) << 16) | (self.first_cluster_low as u32)
    }

    #[inline(always)]
    pub fn is_directory(&self) -> bool {
        self.attributes & 0x10 != 0
    }
    #[inline(always)]
    pub fn is_hidden(&self) -> bool {
        self.attributes & 0x02 != 0
    }
    #[inline(always)]
    pub fn is_system(&self) -> bool {
        self.attributes & 0x04 != 0
    }
    pub fn is_volume_label(&self) -> bool {
        self.attributes & 0x08 != 0
    }
    #[inline(always)]
    pub fn is_readonly(&self) -> bool {
        self.attributes & 0x01 != 0
    }
    #[inline(always)]
    pub fn is_archive(&self) -> bool {
        self.attributes & 0x20 != 0
    }

    /// Convert string to FAT32 8.3 name format
    pub fn string_to_fat_name(filename: &str) -> [u8; 11] {
        let mut name = [b' '; 11]; // Initialize with spaces

        // Handle special cases
        if filename == "." {
            name[0] = b'.';
            return name;
        }
        if filename == ".." {
            name[0] = b'.';
            name[1] = b'.';
            return name;
        }

        let filename_upper = filename.to_uppercase();
        let bytes = filename_upper.as_bytes();

        // Find dot position
        let dot_pos = bytes.iter().position(|&b| b == b'.');

        match dot_pos {
            Some(dot) => {
                // Has extension
                let basename = &bytes[..dot];
                let extension = &bytes[dot + 1..];

                // Copy basename (max 8 chars)
                let basename_len = basename.len().min(8);
                name[..basename_len].copy_from_slice(&basename[..basename_len]);

                // Copy extension (max 3 chars)
                let ext_len = extension.len().min(3);
                name[8..8 + ext_len].copy_from_slice(&extension[..ext_len]);
            }
            None => {
                // No extension
                let basename_len = bytes.len().min(8);
                name[..basename_len].copy_from_slice(&bytes[..basename_len]);
            }
        }

        name
    }

    /// Convert FAT32 8.3 name to String, honoring NT case flags.
    pub fn fat_name_to_string(&self) -> String {
        // Special entries
        if self.name[0] == b'.' && self.name[1] == b' ' {
            return ".".into();
        }
        if self.name[0] == b'.' && self.name[1] == b'.' && self.name[2] == b' ' {
            return "..".into();
        }

        // Lead-byte 0x05 means literal 0xE5 in first char of name
        let mut name = self.name;
        if name[0] == 0x05 {
            name[0] = 0xE5;
        }

        // Trim spaces in base and ext
        let base_end = name[..8]
            .iter()
            .rposition(|&b| b != b' ')
            .map(|p| p + 1)
            .unwrap_or(0);
        let ext_end = name[8..11]
            .iter()
            .rposition(|&b| b != b' ')
            .map(|p| p + 1)
            .unwrap_or(0);

        let base = &name[..base_end];
        let ext = &name[8..8 + ext_end];

        // Apply case flags
        let base_bytes: Vec<u8> = if self.nt_reserved & Self::NT_LOWER_BASE != 0 {
            base.iter().map(|b| b.to_ascii_lowercase()).collect()
        } else {
            base.to_vec()
        };
        let ext_bytes: Vec<u8> = if self.nt_reserved & Self::NT_LOWER_EXT != 0 {
            ext.iter().map(|b| b.to_ascii_lowercase()).collect()
        } else {
            ext.to_vec()
        };

        let mut out = String::new();
        out.push_str(&String::from_utf8_lossy(&base_bytes));
        if !ext_bytes.is_empty() {
            out.push('.');
            out.push_str(&String::from_utf8_lossy(&ext_bytes));
        }
        out
    }

    /// Set the name field from a string
    pub fn set_name_from_string(&mut self, filename: &str) {
        self.name = Self::string_to_fat_name(filename);
    }

    /// Check if filename matches this entry's name
    pub fn name_matches(&self, filename: &str) -> bool {
        let fat_name = Self::string_to_fat_name(filename);
        self.name == fat_name
    }

    /// Check if name is valid for 8.3 format
    pub fn is_valid_short_name(filename: &str) -> bool {
        // Check length
        if filename.is_empty() || filename.len() > 12 {
            return false;
        }

        // Special cases
        if filename == "." || filename == ".." {
            return true;
        }

        // Find dot
        let dot_pos = filename.find('.');

        match dot_pos {
            Some(dot) => {
                let basename = &filename[..dot];
                let extension = &filename[dot + 1..];

                // Check basename and extension lengths
                if basename.is_empty() || basename.len() > 8 || extension.len() > 3 {
                    return false;
                }

                // Check for invalid characters
                Self::has_valid_fat_chars(basename) && Self::has_valid_fat_chars(extension)
            }
            None => {
                // No extension
                filename.len() <= 8 && Self::has_valid_fat_chars(filename)
            }
        }
    }

    /// Check if string contains only valid FAT characters
    fn has_valid_fat_chars(s: &str) -> bool {
        s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '%' | '\''
                        | '-'
                        | '_'
                        | '@'
                        | '~'
                        | '`'
                        | '!'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '^'
                        | '#'
                        | '&'
                )
        })
    }
}

impl From<DirectoryEntry> for File {
    fn from(de: DirectoryEntry) -> Self {
        let name = de.fat_name_to_string();

        let kind = if de.is_directory() {
            FileKind::Directory
        } else {
            FileKind::File
        };

        let attrs = FileAttrs {
            readonly: de.is_readonly(),
            hidden: de.is_hidden(),
            system: de.is_system(),
            archive: de.is_archive(),
        };

        File {
            name,
            kind,
            size: de.file_size as u64,
            attrs,
            created: Some(FileTime {
                date: de.creation_date,
                time: de.creation_time,
                tenth: de.creation_time_tenth,
            }),
            accessed: Some(FileTime {
                date: de.last_access_date,
                time: 0,
                tenth: 0,
            }),
            modified: Some(FileTime {
                date: de.write_date,
                time: de.write_time,
                tenth: 0,
            }),
        }
    }
}
