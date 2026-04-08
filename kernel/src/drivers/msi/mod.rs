#![expect(unused)]

use crate::{
    apic::get_lapic,
    drivers::pci::{
        config::{
            find_capability, pci_read_u8, pci_read_u16, pci_read_u32, pci_write_u16, pci_write_u32,
            read_bar_phys,
        },
        structures::{PciAddress, PciDevice},
    },
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
};
use x86_64::structures::paging::PageTableFlags;

#[derive(Debug, Clone, Copy)]
pub enum MsiError {
    NoCapabilities,
    NoMsiCapability,
    NoMsixCapability,
    MmioMapFailed,
    InvalidEntry,
}

// PCI capability IDs
const CAP_ID_MSI: u8 = 0x05;
const CAP_ID_MSIX: u8 = 0x11;

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

    // Program message address and data.
    // NOTE: The interrupt is targeted at the current CPU (the one running this init code).
    // If the xHCI driver thread later runs on a different CPU, the interrupt still fires on
    // this CPU and wake_thread_irq posts to the scheduler — that is safe across CPUs.
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

pub fn enable_msix_for_device(
    dev: &PciDevice,
    vector: u8,
    table_entry: u16,
) -> Result<(), MsiError> {
    let addr = dev.address;

    // 1. Find MSI-X capability
    let msix_offset = find_capability(addr, CAP_ID_MSIX).ok_or(MsiError::NoMsixCapability)?;

    // 2. Read control register, extract table size
    let ctrl = pci_read_u16(addr, msix_offset + 2);
    let table_size = (ctrl & 0x7FF) + 1;

    if table_entry >= table_size {
        return Err(MsiError::InvalidEntry);
    }

    // 3. Read table BIR and offset
    let table_dword = pci_read_u32(addr, msix_offset + 4);
    let bir = (table_dword & 0x7) as u8;
    let table_offset = (table_dword & !0x7) as u64;

    // 4. Read BAR and compute table physical address
    let bar_phys = read_bar_phys(addr, bir);
    let table_phys = bar_phys + table_offset;

    // 5. Map the MSI-X table MMIO region
    let table_virt = get_virt_addr_from_phys_offset(table_phys);
    let table_size_bytes = (table_size as u64) * 16; // 16 bytes per entry
    {
        let mut mapper = memory_mapper();
        let result = mapper.map_address_range(
            table_virt,
            table_phys,
            table_size_bytes as usize,
            PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE | PageTableFlags::GLOBAL,
        );
        match result {
            Ok(_) => {}
            Err(x86_64::structures::paging::mapper::MapToError::PageAlreadyMapped(_)) => {}
            Err(_) => return Err(MsiError::MmioMapFailed),
        }
    }

    // 6. Write the MSI-X table entry.
    // NOTE: The interrupt is targeted at the current CPU (the one running this init code).
    // If the xHCI driver thread later runs on a different CPU, the interrupt still fires on
    // this CPU and wake_thread_irq posts to the scheduler — that is safe across CPUs.
    let entry_ptr = (table_virt.as_u64() + (table_entry as u64) * 16) as *mut u32;
    let lapic_id = unsafe { get_lapic().id() };
    unsafe {
        // msg_addr_lo
        core::ptr::write_volatile(entry_ptr, 0xFEE0_0000 | ((lapic_id & 0xFF) << 12));
        // msg_addr_hi
        core::ptr::write_volatile(entry_ptr.add(1), 0);
        // msg_data = vector
        core::ptr::write_volatile(entry_ptr.add(2), vector as u32);
        // vector_control = 0 (unmask)
        core::ptr::write_volatile(entry_ptr.add(3), 0);
    }

    // 7. Enable MSI-X, clear function mask
    let ctrl = pci_read_u16(addr, msix_offset + 2);
    pci_write_u16(addr, msix_offset + 2, (ctrl | (1 << 15)) & !(1 << 14));

    // 8. Disable legacy INTx
    let cmd = pci_read_u16(addr, 0x04);
    pci_write_u16(addr, 0x04, cmd | (1 << 10));

    Ok(())
}
