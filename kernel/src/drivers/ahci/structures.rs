use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use bytemuck::{Pod, Zeroable};

use crate::log;

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
    pub cap: u32,     // Host Capabilities
    pub ghc: u32,     // Global Host Control
    pub is: u32,      // Interrupt Status
    pub pi: u32,      // Ports Implemented
    pub vs: u32,      // Version
    pub ccc_ctl: u32, // Command Completion Coalescing Control
    pub ccc_pts: u32, // Command Completion Coalescing Ports
    pub em_loc: u32,  // Enclosure Management Location
    pub em_ctl: u32,  // Enclosure Management Control
    pub cap2: u32,    // Host Capabilities Extended
    pub bohc: u32,    // BIOS/OS Handoff Control and Status
    pub reserved: [u8; 116],
    pub vendor: [u8; 96],
    pub ports: [HbaPort; 32], // Port control registers
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HbaPort {
    pub clb: u32,  // Command List Base Address (lower 32-bits)
    pub clbu: u32, // Command List Base Address (upper 32-bits)
    pub fb: u32,   // FIS Base Address (lower 32-bits)
    pub fbu: u32,  // FIS Base Address (upper 32-bits)
    pub is: u32,   // Interrupt Status
    pub ie: u32,   // Interrupt Enable
    pub cmd: u32,  // Command and Status
    pub reserved0: u32,
    pub tfd: u32,  // Task File Data
    pub sig: u32,  // Signature
    pub ssts: u32, // Serial ATA Status
    pub sctl: u32, // Serial ATA Control
    pub serr: u32, // Serial ATA Error
    pub sact: u32, // Serial ATA Active
    pub ci: u32,   // Command Issue
    pub sntf: u32, // Serial ATA Notification
    pub fbs: u32,  // FIS-based Switching Control
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
    pub ctba: u32,  // Command Table Base Address (lower 32-bits)
    pub ctbau: u32, // Command Table Base Address (upper 32-bits)
    pub reserved: [u32; 4],
}

pub const MAX_PRDT_ENTRIES: usize = 248;

// Command Table - contains the actual SATA command
// AHCI spec: PRDT entries start at offset 0x80 from command table base.
// With 248 PRDT entries the struct is exactly 4096 bytes (one page).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct CommandTable {
    pub cfis: [u8; 64], // Command FIS
    pub acmd: [u8; 16], // ATAPI Command
    pub reserved: [u8; 48],
    pub prdt: [PrdtEntry; MAX_PRDT_ENTRIES], // PRDT entries (at offset 0x80)
}

const _: () = {
    assert!(core::mem::offset_of!(CommandTable, prdt) == 0x80);
    assert!(core::mem::size_of::<CommandTable>() == 4096);
};

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
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const SATA_SIG_SEMB: u32 = 0xC33C0101;
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const SATA_SIG_PM: u32 = 0x96690101;

// Command header flags
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const CMD_HEADER_CFL_MASK: u16 = 0x1F; // Command FIS Length
pub const CMD_HEADER_ATAPI: u16 = 1 << 5; // ATAPI command
pub const CMD_HEADER_WRITE: u16 = 1 << 6; // Write direction
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const CMD_HEADER_PREFETCHABLE: u16 = 1 << 7;
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const CMD_HEADER_RESET: u16 = 1 << 8;
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const CMD_HEADER_BIST: u16 = 1 << 9;
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const CMD_HEADER_CLEAR_BUSY: u16 = 1 << 10;

// Port command register bits
pub const PORT_CMD_ST: u32 = 1 << 0; // Start
pub const PORT_CMD_SUD: u32 = 1 << 1; // Spin-up Device
pub const PORT_CMD_POD: u32 = 1 << 2; // Power On Device
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_CMD_CLO: u32 = 1 << 3; // Command List Override
pub const PORT_CMD_FRE: u32 = 1 << 4; // FIS Receive Enable
pub const PORT_CMD_FR: u32 = 1 << 14; // FIS Receive Running
pub const PORT_CMD_CR: u32 = 1 << 15; // Command List Running

// Global HBA control bits
pub const GHC_AE: u32 = 1 << 31; // AHCI Enable
pub const GHC_IE: u32 = 1 << 1; // Interrupt Enable

// SSTS register fields
pub const SSTS_DET_MASK: u32 = 0xF; // Device Detection
pub const SSTS_DET_PRESENT: u32 = 3; // Device present and communication established
pub const SSTS_IPM_MASK: u32 = 0xF00; // Interface Power Management
pub const SSTS_IPM_ACTIVE: u32 = 0x100; // Interface in active state

/// Device Detection field of a port's SATA status register (AHCI 1.3.1 §3.3.10).
pub fn ssts_det(ssts: u32) -> u32 {
    ssts & SSTS_DET_MASK
}

/// Interface Power Management field of the same register.
pub fn ssts_ipm(ssts: u32) -> u32 {
    (ssts & SSTS_IPM_MASK) >> 8
}

/// A device is present with an established link and an active interface, the
/// only state a port is brought up in.
pub fn ssts_device_ready(ssts: u32) -> bool {
    ssts & SSTS_DET_MASK == SSTS_DET_PRESENT && ssts & SSTS_IPM_MASK == SSTS_IPM_ACTIVE
}

#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_DHRS: u32 = 1 << 0; // Device to Host Register FIS
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_PSS: u32 = 1 << 1; // PIO Setup FIS
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_DSS: u32 = 1 << 2; // DMA Setup FIS
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_SDBS: u32 = 1 << 3; // Set Device Bits FIS
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_UFS: u32 = 1 << 4; // Unknown FIS
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_DPS: u32 = 1 << 5; // Descriptor Processed
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_PCS: u32 = 1 << 6; // Port Connect Change
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_DMPS: u32 = 1 << 7; // Device Mechanical Presence
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_PRCS: u32 = 1 << 22; // PhyRdy Change
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_IPMS: u32 = 1 << 23; // Incorrect Port Multiplier
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_OFS: u32 = 1 << 24; // Overflow
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_INFS: u32 = 1 << 26; // Interface Non-fatal Error
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_IFS: u32 = 1 << 27; // Interface Fatal Error
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_HBDS: u32 = 1 << 28; // Host Bus Data Error
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_HBFS: u32 = 1 << 29; // Host Bus Fatal Error
pub const PORT_IS_TFES: u32 = 1 << 30; // Task File Error
#[expect(dead_code, reason = "AHCI 1.3.1 register map, kept whole")]
pub const PORT_IS_CPDS: u32 = 1 << 31; // Cold Port Detect

#[derive(Debug, Clone)]
pub struct DeviceIdentifyInfo {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub sectors: u64,
    pub capacity_mb: u64,
    pub capacity_gb: u64,
    pub supports_lba48: bool,
    pub supports_ncq: bool,
    pub ncq_queue_depth: u8, // 1-32 if NCQ supported, 0 otherwise
    pub supports_power_mgmt: bool,
    pub supports_security: bool,
    pub supports_fua: bool,
}

// SCSI Command Descriptor Block structures for ATAPI

/// Allocation length asked of INQUIRY. Covers the standard data plus the
/// vendor-specific tail devices commonly append.
pub const INQUIRY_ALLOCATION_LEN: usize = 96;

/// Mandatory portion of the INQUIRY standard data (SPC-4 §6.4.2): everything
/// up to and including the product revision level.
pub const MIN_INQUIRY_BYTES: usize = 36;

/// Length of the READ CAPACITY(10) parameter data: last LBA and block length,
/// both big-endian 32-bit.
pub const READ_CAPACITY_10_LEN: usize = 8;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ScsiInquiry {
    pub operation_code: u8,    // 0x12
    pub evpd: u8,              // 0
    pub page_code: u8,         // 0
    pub reserved: u8,          // 0
    pub allocation_length: u8, // 96
    pub control: u8,           // 0
    pub padding: [u8; 6],      // Pad to 12 bytes
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ScsiRead10 {
    pub operation_code: u8,       // 0x28
    pub flags: u8,                // 0
    pub lba: [u8; 4],             // Big-endian LBA
    pub reserved: u8,             // 0
    pub transfer_length: [u8; 2], // Big-endian sector count
    pub control: u8,              // 0
    pub padding: [u8; 2],         // Pad to 12 bytes
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ScsiReadCapacity10 {
    pub operation_code: u8, // 0x25
    pub reserved1: u8,      // 0
    pub lba: [u8; 4],       // 0 for capacity request
    pub reserved2: [u8; 2], // 0
    pub pmi: u8,            // 0
    pub control: u8,        // 0
    pub padding: [u8; 3],   // Pad to 12 bytes
}

// SCSI command operation codes
pub const SCSI_CMD_INQUIRY: u8 = 0x12;
pub const SCSI_CMD_READ_10: u8 = 0x28;
pub const SCSI_CMD_READ_CAPACITY_10: u8 = 0x25;

impl ScsiInquiry {
    pub fn new() -> Self {
        let mut cmd = Self::zeroed();
        cmd.operation_code = SCSI_CMD_INQUIRY;
        cmd.allocation_length = INQUIRY_ALLOCATION_LEN as u8;
        cmd
    }
}

impl ScsiRead10 {
    pub fn new(lba: u32, sectors: u16) -> Self {
        let mut cmd = Self::zeroed();
        cmd.operation_code = SCSI_CMD_READ_10;
        cmd.lba = lba.to_be_bytes();
        cmd.transfer_length = sectors.to_be_bytes();
        cmd
    }
}

impl ScsiReadCapacity10 {
    pub fn new() -> Self {
        let mut cmd = Self::zeroed();
        cmd.operation_code = SCSI_CMD_READ_CAPACITY_10;
        cmd
    }
}

impl DeviceIdentifyInfo {
    /// Parse SCSI INQUIRY data into DeviceIdentifyInfo format (for ATAPI devices)
    pub fn from_scsi_inquiry(inquiry_data: &[u8]) -> Self {
        if inquiry_data.len() < MIN_INQUIRY_BYTES {
            return Self::default_atapi();
        }

        // SCSI INQUIRY format:
        // Bytes 8-15: Vendor identification (8 bytes)
        // Bytes 16-31: Product identification (16 bytes)
        // Bytes 32-35: Product revision level (4 bytes)

        let vendor = core::str::from_utf8(&inquiry_data[8..16])
            .unwrap_or("<invalid>")
            .trim()
            .to_string();

        let product = core::str::from_utf8(&inquiry_data[16..32])
            .unwrap_or("<invalid>")
            .trim()
            .to_string();

        let revision = core::str::from_utf8(&inquiry_data[32..36])
            .unwrap_or("<invalid>")
            .trim()
            .to_string();

        // Combine vendor and product for model name
        let model = if vendor.is_empty() {
            product
        } else {
            format!("{} {}", vendor, product)
        };

        Self {
            model,
            serial: "<ATAPI>".to_string(), // ATAPI devices don't always have serial in INQUIRY
            firmware: revision,
            sectors: 0, // Will be filled by READ_CAPACITY
            capacity_mb: 0,
            capacity_gb: 0,
            supports_lba48: false, // Not applicable for ATAPI
            supports_ncq: false,
            ncq_queue_depth: 0,
            supports_power_mgmt: false,
            supports_security: false,
            supports_fua: false,
        }
    }

    fn default_atapi() -> Self {
        Self {
            model: "<Unknown ATAPI>".to_string(),
            serial: "<ATAPI>".to_string(),
            firmware: "<Unknown>".to_string(),
            sectors: 0,
            capacity_mb: 0,
            capacity_gb: 0,
            supports_lba48: false,
            supports_ncq: false,
            ncq_queue_depth: 0,
            supports_power_mgmt: false,
            supports_security: false,
            supports_fua: false,
        }
    }

    /// Parse IDENTIFY data into a structured format
    pub fn from_identify_data(identify_data: &[u8; 512]) -> Self {
        // Model number (words 27-46, byte-swapped)
        let model_bytes: Vec<u8> = identify_data[54..94]
            .chunks_exact(2)
            .flat_map(|chunk| [chunk[1], chunk[0]]) // Byte swap
            .collect();
        let model = core::str::from_utf8(&model_bytes)
            .unwrap_or("<invalid>")
            .trim()
            .to_string();

        // Serial number (words 10-19, byte-swapped)
        let serial_bytes: Vec<u8> = identify_data[20..40]
            .chunks_exact(2)
            .flat_map(|chunk| [chunk[1], chunk[0]]) // Byte swap
            .collect();
        let serial = core::str::from_utf8(&serial_bytes)
            .unwrap_or("<invalid>")
            .trim()
            .to_string();

        // Firmware revision (words 23-26, byte-swapped)
        let firmware_bytes: Vec<u8> = identify_data[46..54]
            .chunks_exact(2)
            .flat_map(|chunk| [chunk[1], chunk[0]]) // Byte swap
            .collect();
        let firmware = core::str::from_utf8(&firmware_bytes)
            .unwrap_or("<invalid>")
            .trim()
            .to_string();

        // Capacity (words 60-61 for 28-bit LBA, words 100-103 for 48-bit LBA)
        let lba28_sectors = u32::from_le_bytes([
            identify_data[120],
            identify_data[121],
            identify_data[122],
            identify_data[123],
        ]);

        let lba48_sectors = u64::from_le_bytes([
            identify_data[200],
            identify_data[201],
            identify_data[202],
            identify_data[203],
            identify_data[204],
            identify_data[205],
            identify_data[206],
            identify_data[207],
        ]);

        // Check if 48-bit LBA is supported (bit 10 of word 83)
        let supports_lba48 = identify_data[167] & 0x04 != 0;

        // NCQ support: word 76 (SATA capabilities) bit 8
        let sata_caps = u16::from_le_bytes([identify_data[152], identify_data[153]]);
        let supports_ncq = sata_caps & (1 << 8) != 0;
        // Queue depth: word 75 bits 4:0 = max queue depth - 1
        let ncq_queue_depth = if supports_ncq {
            let qdw = u16::from_le_bytes([identify_data[150], identify_data[151]]);
            ((qdw & 0x1F) as u8) + 1
        } else {
            0
        };

        let sectors = if supports_lba48 && lba48_sectors > 0 {
            lba48_sectors
        } else {
            lba28_sectors as u64
        };

        let capacity_mb = (sectors * 512) / (1024 * 1024);
        let capacity_gb = capacity_mb / 1024;

        // Additional capabilities
        let features = u16::from_le_bytes([identify_data[166], identify_data[167]]);
        let supports_power_mgmt = features & 0x0020 != 0;
        let supports_security = features & 0x0002 != 0;

        // FUA support: word 84 bit 6 (commands/features supported)
        let word84 = u16::from_le_bytes([identify_data[168], identify_data[169]]);
        let supports_fua = word84 & (1 << 6) != 0;

        Self {
            model,
            serial,
            firmware,
            sectors,
            capacity_mb,
            capacity_gb,
            supports_lba48,
            supports_ncq,
            ncq_queue_depth,
            supports_power_mgmt,
            supports_security,
            supports_fua,
        }
    }

    /// Print device information in a formatted way
    pub fn print_info(&self, port_idx: usize) {
        log!("=== Device Info for Port {} ===", port_idx);
        log!("Model: {}", self.model);
        log!("Serial: {}", self.serial);
        log!("Firmware: {}", self.firmware);
        log!(
            "Capacity: {} sectors ({} MB / {} GB)",
            self.sectors,
            self.capacity_mb,
            self.capacity_gb
        );
        log!("LBA48 Support: {}", self.supports_lba48);
        if self.supports_ncq {
            log!("  - NCQ supported, queue depth {}", self.ncq_queue_depth);
        }
        if self.supports_power_mgmt {
            log!("  - Power Management supported");
        }
        if self.supports_security {
            log!("  - Security feature set supported");
        }
        if self.supports_fua {
            log!("  - FUA (Force Unit Access) supported");
        }
    }
}
