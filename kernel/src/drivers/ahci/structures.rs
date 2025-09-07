#![expect(unused)]

use bytemuck::{Pod, Zeroable};

// Compile-time structure size assertions
const _: () = {
    // HbaMemory should be 0x1100 bytes (4352 bytes) according to AHCI spec
    assert!(core::mem::size_of::<HbaMemory>() == 0x1100);
    // HbaPort should be 0x80 bytes (128 bytes) according to AHCI spec
    assert!(core::mem::size_of::<HbaPort>() == 0x80);
    // Verify proper alignment
    assert!(core::mem::align_of::<HbaMemory>() >= 4);
    assert!(core::mem::align_of::<HbaPort>() >= 4);
};

// AHCI HBA Memory Registers (volatile access required)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HbaMemory {
    // Generic Host Control
    pub cap: u32, // Host Capabilities
    pub ghc: u32, // Global Host Control
    pub is: u32, // Interrupt Status
    pub pi: u32, // Ports Implemented
    pub vs: u32, // Version
    pub ccc_ctl: u32, // Command Completion Coalescing Control
    pub ccc_pts: u32, // Command Completion Coalescing Ports
    pub em_loc: u32, // Enclosure Management Location
    pub em_ctl: u32, // Enclosure Management Control
    pub cap2: u32, // Host Capabilities Extended
    pub bohc: u32, // BIOS/OS Handoff Control and Status
    pub reserved: [u8; 116],
    pub vendor: [u8; 96],
    pub ports: [HbaPort; 32], // Port control registers
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HbaPort {
    pub clb: u32, // Command List Base Address (lower 32-bits)
    pub clbu: u32, // Command List Base Address (upper 32-bits)
    pub fb: u32, // FIS Base Address (lower 32-bits)
    pub fbu: u32, // FIS Base Address (upper 32-bits)
    pub is: u32, // Interrupt Status
    pub ie: u32, // Interrupt Enable
    pub cmd: u32, // Command and Status
    pub reserved0: u32,
    pub tfd: u32, // Task File Data
    pub sig: u32, // Signature
    pub ssts: u32, // Serial ATA Status
    pub sctl: u32, // Serial ATA Control
    pub serr: u32, // Serial ATA Error
    pub sact: u32, // Serial ATA Active
    pub ci: u32, // Command Issue
    pub sntf: u32, // Serial ATA Notification
    pub fbs: u32, // FIS-based Switching Control
    pub reserved1: [u32; 11],
    pub vendor: [u32; 4],
}

// Command List Entry - points to command table
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct CommandHeader {
    pub flags: u16, // Command flags (CFL, W, P, R, B, C, A)
    pub prdtl: u16, // Physical Region Descriptor Table Length
    pub prdbc: u32, // Physical Region Descriptor Byte Count
    pub ctba: u32, // Command Table Base Address (lower 32-bits)
    pub ctbau: u32, // Command Table Base Address (upper 32-bits)
    pub reserved: [u32; 4],
}

// Command Table - contains the actual SATA command
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct CommandTable {
    pub cfis: [u8; 64], // Command FIS
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

impl HbaMemory {
    pub fn print_structure_info() {
        crate::println!("=== HBA Memory Structure Info ===");
        crate::println!("HbaMemory size: {} bytes (expected: {} bytes)",
            core::mem::size_of::<Self>(), 0x1100);
        crate::println!("HbaMemory alignment: {} bytes",
            core::mem::align_of::<Self>());
        crate::println!("HbaPort size: {} bytes (expected: {} bytes)",
            core::mem::size_of::<HbaPort>(), 0x80);
        crate::println!("HbaPort alignment: {} bytes",
            core::mem::align_of::<HbaPort>());

        // Print field offsets for verification
        crate::println!("Field offsets:");
        crate::println!("  cap: {}", core::mem::offset_of!(Self, cap));
        crate::println!("  ghc: {}", core::mem::offset_of!(Self, ghc));
        crate::println!("  vs: {}", core::mem::offset_of!(Self, vs));
        crate::println!("  pi: {}", core::mem::offset_of!(Self, pi));
        crate::println!("  vendor: {}", core::mem::offset_of!(Self, vendor));
        crate::println!("  ports: {}", core::mem::offset_of!(Self, ports));
    }

    pub fn print_vendor_area(&self) {
        use alloc::string::String;

        let mut output = String::new();
        output.push_str("=== HBA Vendor Area (96 bytes at offset 0xA0) ===\n");
        output.push_str("Vendor data: ");

        for (i, &byte) in self.vendor.iter().enumerate() {
            output.push_str(&alloc::format!("{:02x}", byte));
            if (i + 1) % 16 == 0 {
                output.push('\n');
                if i < self.vendor.len() - 1 {
                    output.push_str("             ");
                }
            } else if (i + 1) % 4 == 0 {
                output.push(' ');
            }
        }

        crate::print!("{}", output);
    }

    pub fn print_basic_registers(&self) {
        crate::println!("=== HBA Basic Registers ===");
        crate::println!("CAP: {:#x}", self.cap);
        crate::println!("GHC: {:#x}", self.ghc);
        crate::println!("IS: {:#x}", self.is);
        crate::println!("PI: {:#x}", self.pi);
        crate::println!("VS: {:#x}", self.vs);
        crate::println!("CAP2: {:#x}", self.cap2);
        crate::println!("BOHC: {:#x}", self.bohc);
    }
}

impl HbaPort {
    pub fn print_registers(&self, port_idx: usize) {
        crate::println!("=== Port {} Registers ===", port_idx);
        crate::println!("CLB: {:#x}", self.clb);
        crate::println!("CLBU: {:#x}", self.clbu);
        crate::println!("FB: {:#x}", self.fb);
        crate::println!("FBU: {:#x}", self.fbu);
        crate::println!("IS: {:#x}", self.is);
        crate::println!("IE: {:#x}", self.ie);
        crate::println!("CMD: {:#x}", self.cmd);
        crate::println!("TFD: {:#x}", self.tfd);
        crate::println!("SIG: {:#x}", self.sig);
        crate::println!("SSTS: {:#x}", self.ssts);
        crate::println!("SCTL: {:#x}", self.sctl);
        crate::println!("SERR: {:#x}", self.serr);
        crate::println!("SACT: {:#x}", self.sact);
        crate::println!("CI: {:#x}", self.ci);
        crate::println!("SNTF: {:#x}", self.sntf);
        crate::println!("FBS: {:#x}", self.fbs);
    }

    pub fn print_vendor_area(&self, port_idx: usize) {
        use alloc::string::String;

        let mut output = String::new();
        output.push_str(&alloc::format!("=== Port {} Vendor Area ===\n", port_idx));
        output.push_str("Vendor registers: ");

        for (i, &reg) in self.vendor.iter().enumerate() {
            output.push_str(&alloc::format!("{:#x}", reg));
            if i < self.vendor.len() - 1 {
                output.push(' ');
            }
        }

        crate::println!("{}", output);
    }
}
