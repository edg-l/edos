#![expect(unused)]

use bytemuck::{Pod, Zeroable};
use volatile::{VolatileFieldAccess, access::ReadWrite};

// AHCI HBA Memory Registers (volatile access required)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, VolatileFieldAccess)]
pub struct HbaMemory {
    // Generic Host Control
    #[access(ReadWrite)]
    pub cap: u32, // Host Capabilities
    #[access(ReadWrite)]
    pub ghc: u32, // Global Host Control
    #[access(ReadWrite)]
    pub is: u32, // Interrupt Status
    #[access(ReadWrite)]
    pub pi: u32, // Ports Implemented
    #[access(ReadWrite)]
    pub vs: u32, // Version
    #[access(ReadWrite)]
    pub ccc_ctl: u32, // Command Completion Coalescing Control
    #[access(ReadWrite)]
    pub ccc_pts: u32, // Command Completion Coalescing Ports
    #[access(ReadWrite)]
    pub em_loc: u32, // Enclosure Management Location
    #[access(ReadWrite)]
    pub em_ctl: u32, // Enclosure Management Control
    #[access(ReadWrite)]
    pub cap2: u32, // Host Capabilities Extended
    #[access(ReadWrite)]
    pub bohc: u32, // BIOS/OS Handoff Control and Status
    pub reserved: [u8; 116],
    #[access(ReadWrite)]
    pub vendor: [u8; 96],
    #[access(ReadWrite)]
    pub ports: [HbaPort; 32], // Port control registers
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, VolatileFieldAccess)]
pub struct HbaPort {
    #[access(ReadWrite)]
    pub clb: u32, // Command List Base Address (lower 32-bits)
    #[access(ReadWrite)]
    pub clbu: u32, // Command List Base Address (upper 32-bits)
    #[access(ReadWrite)]
    pub fb: u32, // FIS Base Address (lower 32-bits)
    #[access(ReadWrite)]
    pub fbu: u32, // FIS Base Address (upper 32-bits)
    #[access(ReadWrite)]
    pub is: u32, // Interrupt Status
    #[access(ReadWrite)]
    pub ie: u32, // Interrupt Enable
    #[access(ReadWrite)]
    pub cmd: u32, // Command and Status
    pub reserved0: u32,
    #[access(ReadWrite)]
    pub tfd: u32, // Task File Data
    #[access(ReadWrite)]
    pub sig: u32, // Signature
    #[access(ReadWrite)]
    pub ssts: u32, // Serial ATA Status
    #[access(ReadWrite)]
    pub sctl: u32, // Serial ATA Control
    #[access(ReadWrite)]
    pub serr: u32, // Serial ATA Error
    #[access(ReadWrite)]
    pub sact: u32, // Serial ATA Active
    #[access(ReadWrite)]
    pub ci: u32, // Command Issue
    #[access(ReadWrite)]
    pub sntf: u32, // Serial ATA Notification
    #[access(ReadWrite)]
    pub fbs: u32, // FIS-based Switching Control
    pub reserved1: [u32; 11],
    #[access(ReadWrite)]
    pub vendor: [u32; 4],
}

// Command List Entry - points to command table
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, VolatileFieldAccess)]
pub struct CommandHeader {
    #[access(ReadWrite)]
    pub flags: u16, // Command flags (CFL, W, P, R, B, C, A)
    #[access(ReadWrite)]
    pub prdtl: u16, // Physical Region Descriptor Table Length
    #[access(ReadWrite)]
    pub prdbc: u32, // Physical Region Descriptor Byte Count
    #[access(ReadWrite)]
    pub ctba: u32, // Command Table Base Address (lower 32-bits)
    #[access(ReadWrite)]
    pub ctbau: u32, // Command Table Base Address (upper 32-bits)
    pub reserved: [u32; 4],
}

// Command Table - contains the actual SATA command
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, VolatileFieldAccess)]
pub struct CommandTable {
    #[access(ReadWrite)]
    pub cfis: [u8; 64], // Command FIS
    #[access(ReadWrite)]
    pub acmd: [u8; 16], // ATAPI Command
    pub reserved: [u8; 48],
    // PRDT entries follow (variable length)
}

// Physical Region Descriptor Table Entry - describes DMA scatter-gather
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct PrdtEntry {
    pub dba: u32,  // Data Base Address (lower 32-bits)
    pub dbau: u32, // Data Base Address (upper 32-bits)
    pub reserved: u32,
    pub dbc: u32, // Data Byte Count (bit 31 = interrupt on completion)
}

// Received FIS Structure
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HbaFis {
    pub dsfis: [u8; 32], // DMA Setup FIS
    pub psfis: [u8; 32], // PIO Setup FIS
    pub rfis: [u8; 24],  // D2H Register FIS
    pub sdbfis: [u8; 8], // Set Device Bits FIS
    pub ufis: [u8; 64],  // Unknown FIS
    pub reserved: [u8; 96],
}

// Port signature constants
pub const SATA_SIG_ATA: u32 = 0x00000101;
pub const SATA_SIG_ATAPI: u32 = 0xEB140101;
pub const SATA_SIG_SEMB: u32 = 0xC33C0101;
pub const SATA_SIG_PM: u32 = 0x96690101;

// Command header flags
pub const CMD_HEADER_CFL_MASK: u16 = 0x1F; // Command FIS Length
pub const CMD_HEADER_WRITE: u16 = 1 << 6; // Write direction
pub const CMD_HEADER_PREFETCHABLE: u16 = 1 << 7;
pub const CMD_HEADER_RESET: u16 = 1 << 8;
pub const CMD_HEADER_BIST: u16 = 1 << 9;
pub const CMD_HEADER_CLEAR_BUSY: u16 = 1 << 10;

// Port command register bits
pub const PORT_CMD_ST: u32 = 1 << 0; // Start
pub const PORT_CMD_SUD: u32 = 1 << 1; // Spin-up Device
pub const PORT_CMD_POD: u32 = 1 << 2; // Power On Device
pub const PORT_CMD_CLO: u32 = 1 << 3; // Command List Override
pub const PORT_CMD_FRE: u32 = 1 << 4; // FIS Receive Enable
pub const PORT_CMD_FR: u32 = 1 << 14; // FIS Receive Running
pub const PORT_CMD_CR: u32 = 1 << 15; // Command List Running

// Global HBA control bits
pub const GHC_AE: u32 = 1 << 31; // AHCI Enable
pub const GHC_IE: u32 = 1 << 1; // Interrupt Enable

// SSTS register fields
pub const SSTS_DET_MASK: u32 = 0xF;          // Device Detection
pub const SSTS_DET_PRESENT: u32 = 3;         // Device present and communication established
pub const SSTS_IPM_MASK: u32 = 0xF00;        // Interface Power Management
pub const SSTS_IPM_ACTIVE: u32 = 0x100;      // Interface in active state
