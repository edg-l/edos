#![expect(unused)]

use bytemuck::{Pod, Zeroable};

// Host to Device Register FIS
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct FisRegH2D {
    pub fis_type: u8, // FIS_TYPE_REG_H2D
    pub pmport: u8,   // Port multiplier and command/control
    pub command: u8,  // ATA command
    pub featurel: u8, // Feature register low
    pub lba0: u8,     // LBA bits 0-7
    pub lba1: u8,     // LBA bits 8-15
    pub lba2: u8,     // LBA bits 16-23
    pub device: u8,   // Device register
    pub lba3: u8,     // LBA bits 24-31
    pub lba4: u8,     // LBA bits 32-39
    pub lba5: u8,     // LBA bits 40-47
    pub featureh: u8, // Feature register high
    pub countl: u8,   // Count register low
    pub counth: u8,   // Count register high
    pub icc: u8,      // Isochronous command completion
    pub control: u8,  // Control register
    pub reserved: [u8; 4],
}

// FIS Types
pub const FIS_TYPE_REG_H2D: u8 = 0x27; // Register host to device
pub const FIS_TYPE_REG_D2H: u8 = 0x34; // Register device to host
pub const FIS_TYPE_DMA_ACT: u8 = 0x39; // DMA activate
pub const FIS_TYPE_DMA_SETUP: u8 = 0x41; // DMA setup
pub const FIS_TYPE_DATA: u8 = 0x46; // Data
pub const FIS_TYPE_BIST: u8 = 0x58; // BIST activate
pub const FIS_TYPE_PIO_SETUP: u8 = 0x5F; // PIO setup
pub const FIS_TYPE_DEV_BITS: u8 = 0xA1; // Set device bits

// ATA Commands
pub const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
pub const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
pub const ATA_CMD_IDENTIFY: u8 = 0xEC;
pub const ATA_CMD_PACKET: u8 = 0xA0;
pub const ATA_CMD_FLUSH_CACHE: u8 = 0xE7;
pub const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xEA;
pub const ATA_CMD_STANDBY_IMMEDIATE: u8 = 0xE0;
pub const ATA_CMD_IDLE_IMMEDIATE: u8 = 0xE1;
pub const ATA_CMD_SLEEP: u8 = 0xE6;
pub const ATA_CMD_CHECK_POWER_MODE: u8 = 0xE5;
pub const ATA_CMD_SET_FEATURES: u8 = 0xEF;
pub const ATA_CMD_SMART: u8 = 0xB0;
pub const ATA_CMD_VERIFY_SECTORS_EXT: u8 = 0x42;
pub const ATA_CMD_READ_NATIVE_MAX_EXT: u8 = 0x27;
pub const ATA_CMD_SET_MAX_EXT: u8 = 0x37;
pub const ATA_CMD_TRIM: u8 = 0x06; // DATA SET MANAGEMENT
pub const ATA_CMD_SECURITY_ERASE_UNIT: u8 = 0xF4;
pub const ATA_CMD_SECURITY_SET_PASSWORD: u8 = 0xF1;
pub const ATA_CMD_SECURITY_UNLOCK: u8 = 0xF2;
pub const ATA_CMD_SECURITY_FREEZE_LOCK: u8 = 0xF5;

// SMART sub-commands (used with ATA_CMD_SMART)
pub const SMART_READ_DATA: u8 = 0xD0;
pub const SMART_READ_THRESHOLD: u8 = 0xD1;
pub const SMART_ENABLE_OPERATIONS: u8 = 0xD8;
pub const SMART_DISABLE_OPERATIONS: u8 = 0xD9;
pub const SMART_RETURN_STATUS: u8 = 0xDA;

impl FisRegH2D {
    /// Flush write cache to disk
    pub fn new_flush_cache() -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7;
        fis.command = ATA_CMD_FLUSH_CACHE_EXT;
        fis.device = 1 << 6; // LBA mode
        fis
    }

    pub fn new_read_dma_ext(lba: u64, sectors: u16) -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7; // Command register
        fis.command = ATA_CMD_READ_DMA_EXT;
        fis.device = 1 << 6; // LBA mode

        // Set LBA
        fis.lba0 = (lba & 0xFF) as u8;
        fis.lba1 = ((lba >> 8) & 0xFF) as u8;
        fis.lba2 = ((lba >> 16) & 0xFF) as u8;
        fis.lba3 = ((lba >> 24) & 0xFF) as u8;
        fis.lba4 = ((lba >> 32) & 0xFF) as u8;
        fis.lba5 = ((lba >> 40) & 0xFF) as u8;

        // Set sector count
        fis.countl = (sectors & 0xFF) as u8;
        fis.counth = ((sectors >> 8) & 0xFF) as u8;

        fis
    }

    pub fn new_write_dma_ext(lba: u64, sectors: u16) -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7; // Command register
        fis.command = ATA_CMD_WRITE_DMA_EXT;
        fis.device = 1 << 6; // LBA mode

        // Set LBA
        fis.lba0 = (lba & 0xFF) as u8;
        fis.lba1 = ((lba >> 8) & 0xFF) as u8;
        fis.lba2 = ((lba >> 16) & 0xFF) as u8;
        fis.lba3 = ((lba >> 24) & 0xFF) as u8;
        fis.lba4 = ((lba >> 32) & 0xFF) as u8;
        fis.lba5 = ((lba >> 40) & 0xFF) as u8;

        // Set sector count
        fis.countl = (sectors & 0xFF) as u8;
        fis.counth = ((sectors >> 8) & 0xFF) as u8;

        fis
    }

    pub fn new_identify() -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7; // Command register
        fis.command = ATA_CMD_IDENTIFY;
        fis.device = 0; // Master device
        fis
    }

    /// TRIM/DISCARD command for SSDs
    pub fn new_trim() -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7;
        fis.command = ATA_CMD_TRIM;
        fis.device = 1 << 6;
        fis.featurel = 0x01; // TRIM bit
        fis
    }

    /// SMART commands for drive health monitoring
    pub fn new_smart(subcommand: u8) -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7;
        fis.command = ATA_CMD_SMART;
        fis.featurel = subcommand;
        fis.lba1 = 0x4F; // SMART signature
        fis.lba2 = 0xC2; // SMART signature
        fis
    }

    /// Verify sectors without reading data
    pub fn new_verify(lba: u64, sectors: u16) -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7;
        fis.command = ATA_CMD_VERIFY_SECTORS_EXT;
        fis.device = 1 << 6; // LBA mode

        // Set LBA
        fis.lba0 = (lba & 0xFF) as u8;
        fis.lba1 = ((lba >> 8) & 0xFF) as u8;
        fis.lba2 = ((lba >> 16) & 0xFF) as u8;
        fis.lba3 = ((lba >> 24) & 0xFF) as u8;
        fis.lba4 = ((lba >> 32) & 0xFF) as u8;
        fis.lba5 = ((lba >> 40) & 0xFF) as u8;

        fis.countl = (sectors & 0xFF) as u8;
        fis.counth = ((sectors >> 8) & 0xFF) as u8;
        fis
    }

    /// Put drive in standby mode
    pub fn new_standby() -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7;
        fis.command = ATA_CMD_STANDBY_IMMEDIATE;
        fis
    }

    /// Create ATAPI PACKET command FIS
    pub fn new_atapi_packet(transfer_length: u16) -> Self {
        let mut fis = Self::zeroed();
        fis.fis_type = FIS_TYPE_REG_H2D;
        fis.pmport = 1 << 7; // Command register
        fis.command = ATA_CMD_PACKET;
        fis.featurel = 0x01; // DMA transfer
        fis.device = 0x40; // LBA mode

        // Set byte count for transfer (in lba1/lba2 fields for PACKET command)
        fis.lba1 = (transfer_length & 0xFF) as u8;
        fis.lba2 = ((transfer_length >> 8) & 0xFF) as u8;

        fis
    }
}
