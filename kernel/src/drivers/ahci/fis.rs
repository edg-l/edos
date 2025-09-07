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

impl FisRegH2D {
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
}
