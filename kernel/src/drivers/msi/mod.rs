#![expect(unused)]

use crate::{
    apic::get_lapic,
    drivers::pci::{
        config::{pci_read_u8, pci_read_u16, pci_read_u32, pci_write_u16, pci_write_u32},
        structures::{PciAddress, PciDevice},
    },
};

#[derive(Debug, Clone, Copy)]
pub enum MsiError {
    NoCapabilities,
    NoMsiCapability,
}

// PCI capability IDs
const CAP_ID_MSI: u8 = 0x05;

// MSI Control bits
const MSI_ENABLE: u16 = 1 << 0;
const MSI_MULTIPLE_MESSAGE_CAPABLE_SHIFT: u16 = 1; // 3 bits
const MSI_MULTIPLE_MESSAGE_ENABLE_SHIFT: u16 = 4; // 3 bits
const MSI_64BIT_CAPABLE: u16 = 1 << 7;
const MSI_PER_VECTOR_MASKING: u16 = 1 << 8; // not used yet

/// Enable MSI for a given PCI device to a specific APIC vector.
/// Falls back to returning an error if MSI capability is missing.
pub fn enable_msi_for_device(dev: &PciDevice, vector: u8) -> Result<(), MsiError> {
    let cap_ptr = dev.header.capabilities; // offset of first capability (type 0 header)
    if cap_ptr == 0 {
        return Err(MsiError::NoCapabilities);
    }

    let addr = dev.address;
    let msi_offset = find_capability(addr, CAP_ID_MSI).ok_or(MsiError::NoMsiCapability)?;

    // Read MSI control
    let mut ctrl = pci_read_u16(addr, msi_offset + 2);

    // Determine 64-bit capable
    let is_64 = (ctrl & MSI_64BIT_CAPABLE) != 0;

    // Program message address and data
    let lapic_id = unsafe { get_lapic().id() } as u32;
    let msg_addr_low: u32 = 0xFEE0_0000 | ((lapic_id & 0xFF) << 12);
    let msg_addr_high: u32 = 0; // xAPIC mode

    // Fixed delivery mode, edge triggered, deassert: data is just the vector in bits 7:0
    let msg_data: u16 = vector as u16;

    if is_64 {
        pci_write_u32(addr, msi_offset + 4, msg_addr_low);
        pci_write_u32(addr, msi_offset + 8, msg_addr_high);
        pci_write_u16(addr, msi_offset + 12, msg_data);
        // Optional: mask bits at offset + 16 if per-vector masking supported
    } else {
        pci_write_u32(addr, msi_offset + 4, msg_addr_low);
        pci_write_u16(addr, msi_offset + 8, msg_data);
    }

    // Enable only a single message for now
    let mmc = (ctrl >> MSI_MULTIPLE_MESSAGE_CAPABLE_SHIFT) & 0x7; // supported count as 2^mmc
    let _supported_irqs = 1u16 << mmc;
    ctrl &= !(0x7 << MSI_MULTIPLE_MESSAGE_ENABLE_SHIFT);
    ctrl |= 0 << MSI_MULTIPLE_MESSAGE_ENABLE_SHIFT; // enable 1 message

    // Enable MSI
    ctrl |= MSI_ENABLE;
    pci_write_u16(addr, msi_offset + 2, ctrl);

    // Also disable legacy INTx in Command register (bit 10)
    let mut command = pci_read_u16(addr, 0x04);
    command |= 1 << 10;
    pci_write_u16(addr, 0x04, command);

    Ok(())
}

fn find_capability(addr: PciAddress, target_cap_id: u8) -> Option<u8> {
    let mut ptr = pci_read_u8(addr, 0x34);
    let mut guard = 0;
    while ptr != 0 && guard < 64 {
        let cap_id = pci_read_u8(addr, ptr);
        if cap_id == target_cap_id {
            return Some(ptr);
        }
        ptr = pci_read_u8(addr, ptr + 1);
        guard += 1;
    }
    None
}
