//! NVMe controller register layout, doorbell addressing and command/completion
//! entry formats (NVMe Base Specification 2.0 3.1).

use bytemuck::{Pod, Zeroable};

// ---------------------------------------------------------------------------
// Controller registers (3.1.1)
// ---------------------------------------------------------------------------

/// The fixed-size portion of a controller's register set, `BAR0` offsets
/// `0x00`-`0x38`. Doorbells start at `0x1000` and are addressed separately
/// via [`sq_tail_doorbell_offset`] / [`cq_head_doorbell_offset`], since their
/// count and spacing depend on `CAP.DSTRD` and the number of queues created.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct NvmeRegs {
    pub cap: u64,
    pub vs: u32,
    pub intms: u32,
    pub intmc: u32,
    pub cc: u32,
    _rsvd18: u32,
    pub csts: u32,
    _rsvd20: u32,
    pub aqa: u32,
    pub asq: u64,
    pub acq: u64,
}

const _: () = {
    assert!(core::mem::size_of::<NvmeRegs>() == 0x38);
    assert!(core::mem::offset_of!(NvmeRegs, asq) == 0x28);
    assert!(core::mem::offset_of!(NvmeRegs, acq) == 0x30);
};

/// Maximum Queue Entries Supported, `CAP` bits 15:0 (0's based).
pub fn cap_mqes(cap: u64) -> u16 {
    (cap & 0xFFFF) as u16
}

/// Timeout, `CAP` bits 31:24, in 500 ms units.
pub fn cap_to(cap: u64) -> u8 {
    ((cap >> 24) & 0xFF) as u8
}

/// Doorbell Stride, `CAP` bits 35:32: a doorbell is `4 << DSTRD` bytes.
pub fn cap_dstrd(cap: u64) -> u8 {
    ((cap >> 32) & 0xF) as u8
}

/// Command Sets Supported, `CAP` bits 44:37. Bit 0 of this field is the NVM
/// command set.
pub fn cap_css(cap: u64) -> u8 {
    ((cap >> 37) & 0xFF) as u8
}

/// Memory Page Size Minimum, `CAP` bits 51:48: the page size is `2 ^ (12 +
/// MPSMIN)` bytes.
pub fn cap_mpsmin(cap: u64) -> u8 {
    ((cap >> 48) & 0xF) as u8
}

/// Memory Page Size Maximum, `CAP` bits 55:52.
#[expect(
    dead_code,
    reason = "read once a controller's MPSMAX needs checking against a non-default MPS"
)]
pub fn cap_mpsmax(cap: u64) -> u8 {
    ((cap >> 52) & 0xF) as u8
}

/// `CC.EN`, bit 0: enable the controller.
pub const CC_EN: u32 = 1 << 0;
/// `CC.CSS`, bits 6:4: I/O Command Set Selected.
pub const CC_CSS_SHIFT: u32 = 4;
/// `CC.MPS`, bits 10:7: host memory page size, `2 ^ (12 + MPS)` bytes.
pub const CC_MPS_SHIFT: u32 = 7;
/// `CC.SHN`, bits 15:14: Shutdown Notification.
pub const CC_SHN_SHIFT: u32 = 14;
pub const CC_SHN_MASK: u32 = 0x3 << CC_SHN_SHIFT;
pub const CC_SHN_NORMAL: u32 = 0b01 << CC_SHN_SHIFT;
/// `CC.IOSQES`, bits 19:16: I/O Submission Queue Entry Size, as a power of two.
pub const CC_IOSQES_SHIFT: u32 = 16;
/// `CC.IOCQES`, bits 23:20: I/O Completion Queue Entry Size, as a power of two.
pub const CC_IOCQES_SHIFT: u32 = 20;

/// `CSTS.RDY`, bit 0: the controller is ready to accept submission queue doorbell writes.
pub const CSTS_RDY: u32 = 1 << 0;
/// `CSTS.CFS`, bit 1: Controller Fatal Status.
pub const CSTS_CFS: u32 = 1 << 1;
/// `CSTS.SHST`, bits 3:2: Shutdown Status. `10b` is "shutdown complete".
pub const CSTS_SHST_SHIFT: u32 = 2;
pub const CSTS_SHST_MASK: u32 = 0x3 << CSTS_SHST_SHIFT;
pub const CSTS_SHST_COMPLETE: u32 = 0b10 << CSTS_SHST_SHIFT;

/// Byte offset from `BAR0` of the SQ tail doorbell for queue `qid` (3.1.14).
pub fn sq_tail_doorbell_offset(qid: u16, dstrd: u8) -> usize {
    0x1000 + (2 * qid as usize) * (4 << dstrd)
}

/// Byte offset from `BAR0` of the CQ head doorbell for queue `qid` (3.1.15).
pub fn cq_head_doorbell_offset(qid: u16, dstrd: u8) -> usize {
    0x1000 + (2 * qid as usize + 1) * (4 << dstrd)
}

// ---------------------------------------------------------------------------
// Submission / completion queue entries (NVM Command Set 3.3, 3.4)
// ---------------------------------------------------------------------------

/// Common command format: 64 bytes, identical layout for every admin and NVM
/// command. `cdw10`-`cdw15` are opcode-specific.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct SubmissionQueueEntry {
    /// Opcode (bits 7:0) and Command Identifier (bits 31:16). Built with
    /// [`cdw0`].
    pub cdw0: u32,
    pub nsid: u32,
    /// CDW2-3, reserved for every command this driver issues.
    pub reserved: u64,
    /// Metadata pointer; unused, since no supported namespace carries
    /// metadata.
    pub mptr: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

const _: () = {
    assert!(core::mem::size_of::<SubmissionQueueEntry>() == 64);
};

/// Build CDW0's Opcode (bits 7:0) and Command Identifier (bits 31:16) fields.
pub const fn cdw0(opcode: u8, cid: u16) -> u32 {
    opcode as u32 | ((cid as u32) << 16)
}

/// Completion queue entry: 16 bytes (NVM Command Set 3.4.1).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct CompletionQueueEntry {
    /// Command-specific result. Reserved (0) for every command this driver
    /// issues in polled admin use, since Identify returns its data through
    /// PRP1 rather than DW0.
    pub dw0: u32,
    pub dw1: u32,
    /// SQ Head Pointer (bits 15:0), SQ Identifier (bits 31:16).
    pub dw2: u32,
    /// Command Identifier (bits 15:0), Phase Tag (bit 16), Status Field
    /// (bits 31:17: SC 24:17, SCT 27:25, CRD 29:28, M 30, DNR 31).
    pub dw3: u32,
}

const _: () = {
    assert!(core::mem::size_of::<CompletionQueueEntry>() == 16);
};

/// Command Identifier, DW3 bits 15:0.
pub fn cqe_cid(dw3: u32) -> u16 {
    (dw3 & 0xFFFF) as u16
}

/// Phase Tag, DW3 bit 16.
pub fn cqe_phase(dw3: u32) -> bool {
    dw3 & (1 << 16) != 0
}

/// Status Code, from the upper 16 bits of DW3 as handed to
/// [`super::queue::NvmeQueue::drain`]'s callback (bits 24:17 of the original
/// DW3, i.e. bits 8:1 here).
pub fn status_code(status: u16) -> u8 {
    ((status >> 1) & 0xFF) as u8
}

/// Status Code Type, from the same status half (bits 27:25 of DW3, bits
/// 11:9 here).
pub fn status_code_type(status: u16) -> u8 {
    ((status >> 9) & 0x7) as u8
}

/// Do Not Retry, from the same status half (bit 31 of DW3, bit 15 here).
#[expect(
    dead_code,
    reason = "consulted by the SCT/SC-to-BlockError status table"
)]
pub fn status_dnr(status: u16) -> bool {
    status & (1 << 15) != 0
}

/// More, from the same status half (bit 30 of DW3, bit 14 here): additional
/// status information is available via Get Log Page.
#[expect(
    dead_code,
    reason = "consulted by the SCT/SC-to-BlockError status table"
)]
pub fn status_more(status: u16) -> bool {
    status & (1 << 14) != 0
}

// ---------------------------------------------------------------------------
// Opcodes
// ---------------------------------------------------------------------------

pub const ADMIN_OPC_CREATE_IO_SQ: u8 = 0x01;
pub const ADMIN_OPC_DELETE_IO_CQ: u8 = 0x04;
pub const ADMIN_OPC_CREATE_IO_CQ: u8 = 0x05;
pub const ADMIN_OPC_IDENTIFY: u8 = 0x06;
pub const ADMIN_OPC_SET_FEATURES: u8 = 0x09;

/// Identify CNS values used by this driver (NVM Command Set 5.17.2.1).
pub const IDENTIFY_CNS_NAMESPACE: u32 = 0x00;
pub const IDENTIFY_CNS_CONTROLLER: u32 = 0x01;
pub const IDENTIFY_CNS_ACTIVE_NAMESPACE_LIST: u32 = 0x02;

pub const NVM_OPC_FLUSH: u8 = 0x00;
pub const NVM_OPC_WRITE: u8 = 0x01;
pub const NVM_OPC_READ: u8 = 0x02;
