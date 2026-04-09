// e1000e register offsets
pub const CTRL: u32 = 0x0000;
pub const STATUS: u32 = 0x0008;
pub const EERD: u32 = 0x0014;

pub const ICR: u32 = 0x00C0;
pub const ITR: u32 = 0x00C4;
pub const ICS: u32 = 0x00C8;
pub const IMS: u32 = 0x00D0;
pub const IMC: u32 = 0x00D8;

pub const RCTL: u32 = 0x0100;

pub const TCTL: u32 = 0x0400;

pub const RDBAL: u32 = 0x2800;
pub const RDBAH: u32 = 0x2804;
pub const RDLEN: u32 = 0x2808;
pub const RDH: u32 = 0x2810;
pub const RDT: u32 = 0x2818;

pub const TDBAL: u32 = 0x3800;
pub const TDBAH: u32 = 0x3804;
pub const TDLEN: u32 = 0x3808;
pub const TDH: u32 = 0x3810;
pub const TDT: u32 = 0x3818;

pub const MTA: u32 = 0x5200;
pub const RAL0: u32 = 0x5400;
pub const RAH0: u32 = 0x5404;

// CTRL bits
pub const CTRL_ASDE: u32 = 1 << 5;
pub const CTRL_SLU: u32 = 1 << 6;
pub const CTRL_RST: u32 = 1 << 26;
pub const CTRL_PHY_RST: u32 = 1 << 31;

// RCTL bits
pub const RCTL_EN: u32 = 1 << 1;
pub const RCTL_BAM: u32 = 1 << 15;
// BSIZE=11 (bits 17:16) + BSEX=1 (bit 25) => 4096-byte buffers
pub const RCTL_BSIZE_4096: u32 = (3 << 16) | (1 << 25);
pub const RCTL_SECRC: u32 = 1 << 26;

// TCTL bits
pub const TCTL_EN: u32 = 1 << 1;
pub const TCTL_PSP: u32 = 1 << 3;
pub const TCTL_CT_SHIFT: u32 = 4;
pub const TCTL_COLD_SHIFT: u32 = 12;

// STATUS bits
pub const STATUS_LU: u32 = 1 << 1;

// MSI-X interrupt registers
pub const CTRL_EXT: u32 = 0x0018;
pub const CTRL_EXT_IAME: u32 = 0x0800_0000; // Interrupt Acknowledge Auto-Mask Enable
pub const EIAC: u32 = 0x00DC; // Extended Interrupt Auto Clear
pub const IVAR: u32 = 0x00E4; // Interrupt Vector Allocation Register
// Each field is 8 bits: [2:0]=vector, [3]=valid
// RXQ0 = bits [7:0], TXQ0 = bits [15:8], OTHER = bits [23:16]
pub const IVAR_VALID: u32 = 0x08; // bit 3 = entry valid

// IMS/ICR bits (legacy)
pub const IMS_TXDW: u32 = 1 << 0;
pub const IMS_LSC: u32 = 1 << 2;
pub const IMS_RXT0: u32 = 1 << 7;

// IMS/ICR bits for MSI-X queue interrupts
pub const ICR_RXQ0: u32 = 1 << 20;
pub const ICR_TXQ0: u32 = 1 << 22;
pub const ICR_OTHER: u32 = 1 << 24;
