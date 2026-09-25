use spin::Once;

use alloc::{format, string::String};

use crate::{drivers::pci::manager::PciManager, log};

pub mod config;
pub mod manager;
pub mod structures;

/// The device list, scanned once on first use and read-only afterwards, so it
/// needs no lock: `Once` is the only synchronisation it has ever needed.
pub static PCI_MANAGER: Once<PciManager> = Once::new();

pub fn pci_manager() -> &'static PciManager {
    PCI_MANAGER.call_once(|| {
        let mut pci = PciManager::new();
        pci.scan_devices();
        pci
    })
}

pub fn init() {
    for device in pci_manager().get_devices() {
        let (class, subclass) =
            PciManager::decode_class(device.header.class_code, device.header.subclass);
        let irq = if device.header.interrupt_line == 255 {
            String::from("no IRQ")
        } else {
            format!("IRQ {}", device.header.interrupt_line)
        };
        log!(
            "pci: {:02x}:{:02x}.{} {} - {} ({})",
            device.address.bus,
            device.address.device,
            device.address.function,
            class,
            subclass,
            irq
        );
    }
}
