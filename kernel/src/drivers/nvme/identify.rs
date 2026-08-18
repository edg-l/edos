//! Identify Controller and Identify Namespace data structures (NVM Command
//! Set 5.17.2.2, 5.17.2.3).

use alloc::string::{String, ToString};
use bytemuck::{Pod, Zeroable};

/// Identify Controller data structure: 4096 bytes. Only the fields this
/// driver reads are named; everything else is reserved padding sized to
/// land the next named field at its spec offset.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct IdentifyController {
    pub vid: u16,
    pub ssvid: u16,
    /// Serial Number, ASCII, space-padded.
    pub sn: [u8; 20],
    /// Model Number, ASCII, space-padded.
    pub mn: [u8; 40],
    /// Firmware Revision, ASCII, space-padded.
    pub fr: [u8; 8],
    pub rab: u8,
    pub ieee: [u8; 3],
    pub cmic: u8,
    /// Maximum Data Transfer Size, as a page-count power of two. 0 means no
    /// driver-imposed limit.
    pub mdts: u8,
    _rsvd78: [u8; 434],
    /// Submission Queue Entry Size: bits 3:0 required, bits 7:4 maximum, both
    /// as powers of two.
    pub sqes: u8,
    /// Completion Queue Entry Size, same shape as `sqes`.
    pub cqes: u8,
    _rsvd514: u16,
    /// Number of Namespaces.
    pub nn: u32,
    _rsvd520: [u8; 5],
    /// Volatile Write Cache Present, bit 0.
    pub vwc: u8,
    _rsvd526: [u8; 4096 - 526],
}

const _: () = {
    assert!(core::mem::size_of::<IdentifyController>() == 4096);
    assert!(core::mem::offset_of!(IdentifyController, sn) == 4);
    assert!(core::mem::offset_of!(IdentifyController, mn) == 24);
    assert!(core::mem::offset_of!(IdentifyController, fr) == 64);
    assert!(core::mem::offset_of!(IdentifyController, mdts) == 77);
    assert!(core::mem::offset_of!(IdentifyController, sqes) == 512);
    assert!(core::mem::offset_of!(IdentifyController, cqes) == 513);
    assert!(core::mem::offset_of!(IdentifyController, nn) == 516);
    assert!(core::mem::offset_of!(IdentifyController, vwc) == 525);
};

impl IdentifyController {
    pub fn model_trimmed(&self) -> String {
        trim_ascii(&self.mn)
    }

    pub fn serial_trimmed(&self) -> String {
        trim_ascii(&self.sn)
    }

    #[expect(
        dead_code,
        reason = "read once the firmware revision is worth logging alongside model/serial"
    )]
    pub fn firmware_trimmed(&self) -> String {
        trim_ascii(&self.fr)
    }

    pub fn write_cache_present(&self) -> bool {
        self.vwc & 1 != 0
    }
}

/// LBA Format Data Structure (NVM Command Set 5.17.2.3, LBAF array):
/// Metadata Size (bits 15:0), LBA Data Size as a power of two (bits 23:16),
/// Relative Performance (bits 25:24).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct LbaFormat {
    pub ms: u16,
    pub lbads: u8,
    pub rp: u8,
}

const _: () = {
    assert!(core::mem::size_of::<LbaFormat>() == 4);
};

/// Number of LBA formats a namespace can report (NVMe 2.0 caps `NLBAF` at
/// 63); only the low 4 bits of `FLBAS` select one, so entries 16 and above
/// are unreachable by this driver, which refuses any controller reporting
/// `NLBAF > 15` rather than mis-indexing into the extended list.
const LBAF_COUNT: usize = 64;

/// Identify Namespace data structure: 4096 bytes, same padding convention as
/// [`IdentifyController`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct IdentifyNamespace {
    /// Namespace Size, in logical blocks.
    pub nsze: u64,
    /// Namespace Capacity, in logical blocks.
    pub ncap: u64,
    _rsvd16: [u8; 9],
    /// Number of LBA Formats (0's based).
    pub nlbaf: u8,
    /// Formatted LBA Size: bits 3:0 index [`Self::lbaf`] (bits 5:4 select an
    /// extended-list entry above index 15, which this driver does not read).
    pub flbas: u8,
    _rsvd27: [u8; 101],
    pub lbaf: [LbaFormat; LBAF_COUNT],
    _rsvd384: [u8; 4096 - 384],
}

const _: () = {
    assert!(core::mem::size_of::<IdentifyNamespace>() == 4096);
    assert!(core::mem::offset_of!(IdentifyNamespace, nlbaf) == 25);
    assert!(core::mem::offset_of!(IdentifyNamespace, flbas) == 26);
    assert!(core::mem::offset_of!(IdentifyNamespace, lbaf) == 128);
};

impl IdentifyNamespace {
    /// The namespace's logical block size in bytes, from the active LBA
    /// format's `LBADS` (a power of two).
    pub fn lba_size(&self) -> u32 {
        1u32 << self.lbaf[(self.flbas & 0x0F) as usize].lbads
    }
}

fn trim_ascii(bytes: &[u8]) -> String {
    core::str::from_utf8(bytes)
        .unwrap_or("<invalid>")
        .trim()
        .to_string()
}

/// Maximum data transfer size in bytes for a controller's `MDTS` (Identify
/// Controller byte 77) and `CAP.MPSMIN`. `MDTS` of 0 means the controller
/// imposes no limit.
pub fn max_transfer_bytes(mdts: u8, mpsmin: u8) -> usize {
    if mdts == 0 {
        usize::MAX
    } else {
        (1usize << mdts) << (12 + mpsmin)
    }
}
