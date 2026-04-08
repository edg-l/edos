#![expect(unused)]

pub mod device;
pub mod registers;
pub mod rings;

use core::ptr;

use x86_64::structures::paging::{PageTableFlags, mapper::MapToError};

use crate::{
    drivers::pci::{
        config::{pci_read_u16, pci_write_u16, read_bar_phys},
        pci_manager,
        structures::PciAddress,
    },
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
    println,
};

use self::registers::XhciRegisters;

/// PCI class/subclass/prog-if identifying an xHCI controller.
const PCI_CLASS_SERIAL_BUS: u8 = 0x0C;
const PCI_SUBCLASS_USB: u8 = 0x03;
const PCI_PROGIF_XHCI: u8 = 0x30;

pub struct XhciController {
    regs: XhciRegisters,
    pci_addr: PciAddress,
}

impl XhciController {
    /// Probe PCI for an xHCI controller and map its MMIO region.
    ///
    /// Returns `None` if no xHCI device is found.
    pub fn find_and_init() -> Option<Self> {
        let devices = pci_manager().read().get_devices().to_vec();

        for dev in &devices {
            if dev.header.class_code != PCI_CLASS_SERIAL_BUS
                || dev.header.subclass != PCI_SUBCLASS_USB
                || dev.header.prog_if != PCI_PROGIF_XHCI
            {
                continue;
            }

            println!("xhci: found controller at {:?}", dev.address);

            // Read BAR0 (64-bit MMIO)
            let bar_phys = read_bar_phys(dev.address, 0);
            if bar_phys.as_u64() == 0 {
                println!("xhci: BAR0 is zero, skipping");
                continue;
            }

            println!("xhci: BAR0 at {:#x}", bar_phys.as_u64());

            // Enable PCI bus mastering (Command register bit 2)
            let cmd = pci_read_u16(dev.address, 0x04);
            pci_write_u16(dev.address, 0x04, cmd | (1 << 2));

            // Map BAR0 MMIO region into virtual address space via the HHDM offset.
            // 64 KB is a conservative upper bound for xHCI register space.
            let bar_virt = get_virt_addr_from_phys_offset(bar_phys);
            {
                let mut mapper = memory_mapper();
                let result = mapper.map_address_range(
                    bar_virt,
                    bar_phys,
                    0x10000, // 64 KB
                    PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE | PageTableFlags::GLOBAL,
                );
                match result {
                    Ok(_) => {}
                    Err(MapToError::PageAlreadyMapped(_)) => {}
                    Err(e) => {
                        println!("xhci: failed to map BAR0: {:?}", e);
                        continue;
                    }
                }
            }

            let regs = unsafe { XhciRegisters::new(bar_virt.as_mut_ptr()) };

            // Log controller version and capabilities.
            let version = unsafe { ptr::read_volatile(&(*regs.cap()).hciversion) };
            let hcsparams1 = unsafe { ptr::read_volatile(&(*regs.cap()).hcsparams1) };
            let max_slots = hcsparams1 & 0xFF;
            let max_ports = (hcsparams1 >> 24) & 0xFF;

            println!(
                "xhci: version {}.{}, {} slots, {} ports",
                version >> 8,
                version & 0xFF,
                max_slots,
                max_ports
            );

            return Some(Self {
                regs,
                pci_addr: dev.address,
            });
        }

        None
    }
}
