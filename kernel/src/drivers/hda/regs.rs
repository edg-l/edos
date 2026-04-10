//! Intel High Definition Audio controller register definitions.
//! Reference: Intel HD Audio Specification Rev 1.0a
#![expect(unused)]

// === Global Registers ===
pub const GCAP: u32 = 0x00; // Global Capabilities (16-bit)
pub const VMIN: u32 = 0x02; // Minor Version (8-bit)
pub const VMAJ: u32 = 0x03; // Major Version (8-bit)
pub const GCTL: u32 = 0x08; // Global Control (32-bit)
pub const WAKEEN: u32 = 0x0C; // Wake Enable (16-bit)
pub const STATESTS: u32 = 0x0E; // State Change Status (16-bit)
pub const INTCTL: u32 = 0x20; // Interrupt Control (32-bit)
pub const INTSTS: u32 = 0x24; // Interrupt Status (32-bit)

// === CORB Registers ===
pub const CORBLBASE: u32 = 0x40;
pub const CORBUBASE: u32 = 0x44;
pub const CORBWP: u32 = 0x48; // Write Pointer (16-bit)
pub const CORBRP: u32 = 0x4A; // Read Pointer (16-bit)
pub const CORBCTL: u32 = 0x4C; // Control (8-bit)
pub const CORBSTS: u32 = 0x4D; // Status (8-bit)
pub const CORBSIZE: u32 = 0x4E; // Size (8-bit)

// === RIRB Registers ===
pub const RIRBLBASE: u32 = 0x50;
pub const RIRBUBASE: u32 = 0x54;
pub const RIRBWP: u32 = 0x58; // Write Pointer (16-bit)
pub const RINTCNT: u32 = 0x5A; // Response Interrupt Count (16-bit)
pub const RIRBCTL: u32 = 0x5C; // Control (8-bit)
pub const RIRBSTS: u32 = 0x5D; // Status (8-bit)
pub const RIRBSIZE: u32 = 0x5E; // Size (8-bit)

// === Stream Descriptor offsets (from stream base) ===
pub const SD_CTL: u32 = 0x00; // Control (24-bit, access as u32)
pub const SD_STS: u32 = 0x03; // Status (8-bit)
pub const SD_LPIB: u32 = 0x04; // Link Position in Buffer (32-bit)
pub const SD_CBL: u32 = 0x08; // Cyclic Buffer Length (32-bit)
pub const SD_LVI: u32 = 0x0C; // Last Valid Index (16-bit)
pub const SD_FIFOS: u32 = 0x10; // FIFO Size (16-bit)
pub const SD_FMT: u32 = 0x12; // Format (16-bit)
pub const SD_BDLPL: u32 = 0x18; // BDL Pointer Lower (32-bit)
pub const SD_BDLPU: u32 = 0x1C; // BDL Pointer Upper (32-bit)

// === GCTL bits ===
pub const GCTL_CRST: u32 = 1 << 0; // Controller Reset

// === INTCTL bits ===
pub const INTCTL_GIE: u32 = 1 << 31; // Global Interrupt Enable
pub const INTCTL_CIE: u32 = 1 << 30; // Controller Interrupt Enable

// === CORBCTL bits ===
pub const CORBCTL_RUN: u8 = 1 << 1; // Enable CORB DMA

// === RIRBCTL bits ===
pub const RIRBCTL_DMA_EN: u8 = 1 << 1; // RIRB DMA Enable
pub const RIRBCTL_IRQ_EN: u8 = 1 << 0; // RIRB IRQ Enable

// === RIRBSTS bits ===
pub const RIRBSTS_IRQ: u8 = 1 << 0; // Response Interrupt

// === SD_CTL bits ===
pub const SD_CTL_SRST: u32 = 1 << 0; // Stream Reset
pub const SD_CTL_RUN: u32 = 1 << 1; // Stream DMA Run
pub const SD_CTL_IOCE: u32 = 1 << 2; // Interrupt On Completion Enable
pub const SD_CTL_FEIE: u32 = 1 << 3; // FIFO Error Interrupt Enable
pub const SD_CTL_DESCE: u32 = 1 << 4; // Descriptor Error Interrupt Enable
// Stream tag is in bits [23:20] of the 24-bit SD_CTL register
pub const SD_CTL_STREAM_TAG_SHIFT: u32 = 20;

// === SD_STS bits ===
pub const SD_STS_BCIS: u8 = 1 << 2; // Buffer Completion Interrupt Status
pub const SD_STS_FIFOE: u8 = 1 << 3; // FIFO Error
pub const SD_STS_DESE: u8 = 1 << 4; // Descriptor Error

// === Stream Format (SD_FMT) encoding ===
pub const FMT_BASE_48K: u16 = 0 << 14;
pub const FMT_BASE_44K: u16 = 1 << 14;
pub const FMT_BITS_16: u16 = 1 << 4; // bits [6:4] = 001 = 16-bit
pub const FMT_BITS_8: u16 = 0 << 4; // bits [6:4] = 000 = 8-bit
// Channels: bits [3:0] = num_channels - 1
// Multiplier: bits [13:11], Divisor: bits [10:8]

/// BDL entry (Buffer Descriptor List). Each entry is 16 bytes.
/// The BDL must be 128-byte aligned.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BdlEntry {
    pub address: u64,
    pub length: u32,
    /// Bit 0 = IOC (Interrupt On Completion). Bits 31:1 reserved (must be 0).
    pub flags: u32,
}

/// Number of BDL entries per stream.
pub const BDL_ENTRIES: usize = 32;

/// Size of each audio DMA buffer (one page).
pub const AUDIO_BUF_SIZE: usize = 4096;
