//! Shared PCI configuration space access helpers.
//!
//! All access to PCI config ports 0xCF8/0xCFC is serialized through
//! `PCI_CONFIG_LOCK` to prevent corruption from concurrent CPUs.

use x86_64::instructions::port::Port;

use super::structures::PciAddress;
use crate::{debug::lock_order::RANK_PCI_CONFIG, ranked_lock};

/// Spinlock serializing PCI config space accesses (ports 0xCF8/0xCFC).
/// The address+data sequence is non-atomic, so concurrent CPUs must not interleave.
pub static PCI_CONFIG_LOCK: spin::Mutex<()> = spin::Mutex::new(());

fn pci_config_address(addr: PciAddress, offset: u8) -> u32 {
    0x8000_0000
        | ((addr.bus as u32) << 16)
        | ((addr.device as u32) << 11)
        | ((addr.function as u32) << 8)
        | ((offset & 0xFC) as u32)
}

pub fn pci_read_u32(addr: PciAddress, offset: u8) -> u32 {
    let _guard = ranked_lock!(RANK_PCI_CONFIG, "PCI_CONFIG_LOCK", PCI_CONFIG_LOCK);
    let mut cfg_addr: Port<u32> = Port::new(0xCF8);
    let mut cfg_data: Port<u32> = Port::new(0xCFC);
    unsafe {
        cfg_addr.write(pci_config_address(addr, offset));
        cfg_data.read()
    }
}

pub fn pci_write_u32(addr: PciAddress, offset: u8, value: u32) {
    let _guard = ranked_lock!(RANK_PCI_CONFIG, "PCI_CONFIG_LOCK", PCI_CONFIG_LOCK);
    let mut cfg_addr: Port<u32> = Port::new(0xCF8);
    let mut cfg_data: Port<u32> = Port::new(0xCFC);
    unsafe {
        cfg_addr.write(pci_config_address(addr, offset));
        cfg_data.write(value);
    }
}

pub fn pci_read_u16(addr: PciAddress, offset: u8) -> u16 {
    let shift = ((offset & 2) as u32) * 8;
    (pci_read_u32(addr, offset) >> shift) as u16
}

pub fn pci_write_u16(addr: PciAddress, offset: u8, value: u16) {
    // Single lock for the read-modify-write to avoid TOCTOU with other CPUs.
    let _guard = ranked_lock!(RANK_PCI_CONFIG, "PCI_CONFIG_LOCK", PCI_CONFIG_LOCK);
    let mut cfg_addr: Port<u32> = Port::new(0xCF8);
    let mut cfg_data: Port<u32> = Port::new(0xCFC);
    let config_addr = pci_config_address(addr, offset & !3);
    let shift = ((offset & 2) as u32) * 8;
    let current = unsafe {
        cfg_addr.write(config_addr);
        cfg_data.read()
    };
    let mask = !(0xFFFFu32 << shift);
    let new_val = (current & mask) | ((value as u32) << shift);
    unsafe {
        cfg_addr.write(config_addr);
        cfg_data.write(new_val);
    }
}

pub fn pci_read_u8(addr: PciAddress, offset: u8) -> u8 {
    let shift = ((offset & 3) as u32) * 8;
    (pci_read_u32(addr, offset) >> shift) as u8
}

/// Walk the PCI capability linked list and return the config-space offset of
/// the first capability with the given ID, or `None` if not found.
pub fn find_capability(addr: PciAddress, target_cap_id: u8) -> Option<u8> {
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

/// Read a PCI BAR register and return the physical base address.
/// Handles both 32-bit and 64-bit BARs (type bits [2:1]).
/// `bar_index` must be 0..5.
pub fn read_bar_phys(addr: PciAddress, bar_index: u8) -> x86_64::PhysAddr {
    assert!(bar_index <= 5, "BAR index {bar_index} out of range");
    let bar_offset = 0x10 + bar_index * 4;
    let bar_low = pci_read_u32(addr, bar_offset);
    let bar_type = (bar_low >> 1) & 0x3;
    let base_low = (bar_low & !0xF) as u64;

    if bar_type == 2 {
        // 64-bit BAR: upper 32 bits in next BAR register.
        // A 64-bit BAR at index 5 would read past the BAR space (no BAR6), which is invalid.
        assert!(bar_index <= 4, "64-bit BAR at index 5 is invalid");
        let bar_high = pci_read_u32(addr, bar_offset + 4) as u64;
        x86_64::PhysAddr::new(base_low | (bar_high << 32))
    } else {
        x86_64::PhysAddr::new(base_low)
    }
}

pub fn pci_write_u8(addr: PciAddress, offset: u8, value: u8) {
    let _guard = ranked_lock!(RANK_PCI_CONFIG, "PCI_CONFIG_LOCK", PCI_CONFIG_LOCK);
    let mut cfg_addr: Port<u32> = Port::new(0xCF8);
    let mut cfg_data: Port<u32> = Port::new(0xCFC);
    let config_addr = pci_config_address(addr, offset & !3);
    let shift = ((offset & 3) as u32) * 8;
    let current = unsafe {
        cfg_addr.write(config_addr);
        cfg_data.read()
    };
    let mask = !(0xFFu32 << shift);
    let new_val = (current & mask) | ((value as u32) << shift);
    unsafe {
        cfg_addr.write(config_addr);
        cfg_data.write(new_val);
    }
}
