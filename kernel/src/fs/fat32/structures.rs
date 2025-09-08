use alloc::string::{String, ToString};
use bytemuck::{Pod, Zeroable};

/// FAT32 Boot Sector (512 bytes)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Fat32BootSector {
    pub jmp_boot: [u8; 3],          // Jump instruction
    pub oem_name: [u8; 8],          // OEM name
    pub bytes_per_sector: u16,      // Bytes per sector (usually 512)
    pub sectors_per_cluster: u8,    // Sectors per cluster
    pub reserved_sector_count: u16, // Reserved sectors (usually 32)
    pub num_fats: u8,               // Number of FAT tables (usually 2)
    pub root_entry_count: u16,      // Root entries (0 for FAT32)
    pub total_sectors_16: u16,      // Total sectors if < 65536 (0 for FAT32)
    pub media: u8,                  // Media descriptor
    pub fat_size_16: u16,           // FAT size in sectors (0 for FAT32)
    pub sectors_per_track: u16,     // Sectors per track
    pub num_heads: u16,             // Number of heads
    pub hidden_sectors: u32,        // Hidden sectors
    pub total_sectors_32: u32,      // Total sectors
    pub fat_size_32: u32,           // FAT32 size in sectors
    pub ext_flags: u16,             // Extended flags
    pub fs_version: u16,            // Filesystem version
    pub root_cluster: u32,          // Root directory cluster
    pub fs_info: u16,               // FSInfo sector number
    pub backup_boot_sector: u16,    // Backup boot sector
    pub reserved: [u8; 12],         // Reserved
    pub drive_number: u8,           // Drive number
    pub reserved1: u8,              // Reserved
    pub boot_signature: u8,         // Boot signature (0x29)
    pub volume_id: u32,             // Volume ID
    pub volume_label: [u8; 11],     // Volume label
    pub file_system_type: [u8; 8],  // "FAT32   "
    pub boot_code: [u8; 420],       // Boot code
    pub boot_sector_signature: u16, // 0xAA55
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
    /// Get the full cluster number
    pub fn first_cluster(&self) -> u32 {
        ((self.first_cluster_high as u32) << 16) | (self.first_cluster_low as u32)
    }

    /// Check if this is a directory
    pub fn is_directory(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }

    /// Check if this is a long filename entry
    pub fn is_long_name(&self) -> bool {
        self.attributes == ATTR_LONG_NAME
    }

    /// Check if entry is deleted
    pub fn is_deleted(&self) -> bool {
        self.name[0] == 0xE5
    }

    /// Check if entry is end of directory
    pub fn is_end(&self) -> bool {
        self.name[0] == 0x00
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

    /// Convert FAT32 8.3 name to string
    pub fn fat_name_to_string(&self) -> String {
        // Handle special cases
        if self.name[0] == b'.' && self.name[1] == b' ' {
            return ".".to_string();
        }
        if self.name[0] == b'.' && self.name[1] == b'.' && self.name[2] == b' ' {
            return "..".to_string();
        }

        let mut result = String::new();

        // Extract basename (first 8 bytes)
        let basename_end = self.name[..8]
            .iter()
            .rposition(|&b| b != b' ')
            .map(|pos| pos + 1)
            .unwrap_or(0);

        if basename_end > 0 {
            result.push_str(&String::from_utf8_lossy(&self.name[..basename_end]));
        }

        // Extract extension (last 3 bytes)
        let ext_end = self.name[8..11]
            .iter()
            .rposition(|&b| b != b' ')
            .map(|pos| pos + 1)
            .unwrap_or(0);

        if ext_end > 0 {
            result.push('.');
            result.push_str(&String::from_utf8_lossy(&self.name[8..8 + ext_end]));
        }

        result
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
