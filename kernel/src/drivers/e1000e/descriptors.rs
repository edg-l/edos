use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct RxDescriptor {
    pub buffer_addr: u64,
    pub length: u16,
    pub checksum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct TxDescriptor {
    pub buffer_addr: u64,
    pub length: u16,
    pub cso: u8,
    pub cmd: u8,
    pub status: u8,
    pub css: u8,
    pub special: u16,
}

// RX descriptor status bits
pub const RX_STATUS_DD: u8 = 1 << 0;
pub const RX_STATUS_EOP: u8 = 1 << 1;

// TX descriptor command bits
pub const TX_CMD_EOP: u8 = 1 << 0;
pub const TX_CMD_IFCS: u8 = 1 << 1;
pub const TX_CMD_RS: u8 = 1 << 3;

// TX descriptor status bits
pub const TX_STATUS_DD: u8 = 1 << 0;
