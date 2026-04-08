//! Shared PCI configuration space access helpers.
//!
//! All access to PCI config ports 0xCF8/0xCFC is serialized through
//! `PCI_CONFIG_LOCK` to prevent corruption from concurrent CPUs.

use x86_64::instructions::port::Port;

use super::structures::PciAddress;

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
    let _guard = PCI_CONFIG_LOCK.lock();
    let mut cfg_addr: Port<u32> = Port::new(0xCF8);
    let mut cfg_data: Port<u32> = Port::new(0xCFC);
    unsafe {
        cfg_addr.write(pci_config_address(addr, offset));
        cfg_data.read()
    }
}

pub fn pci_write_u32(addr: PciAddress, offset: u8, value: u32) {
    let _guard = PCI_CONFIG_LOCK.lock();
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
    let _guard = PCI_CONFIG_LOCK.lock();
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

#[expect(unused)]
pub fn pci_write_u8(addr: PciAddress, offset: u8, value: u8) {
    let _guard = PCI_CONFIG_LOCK.lock();
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
