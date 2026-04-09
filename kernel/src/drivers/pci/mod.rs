use spin::{Once, RwLock};

use alloc::format;

use crate::{drivers::pci::manager::PciManager, log};

pub mod config;
pub mod manager;
pub mod structures;

pub static PCI_MANAGER: Once<RwLock<PciManager>> = Once::new();

pub fn pci_manager() -> &'static RwLock<PciManager> {
    PCI_MANAGER.call_once(|| {
        let mut pci = PciManager::new();
        pci.scan_devices();
        RwLock::new(pci)
    })
}

pub fn init() {
    for device in pci_manager().read().get_devices() {
        let (class, subclass) =
            PciManager::decode_class(device.header.class_code, device.header.subclass);
        let irq = if device.header.interrupt_line == 255 {
            format!("no IRQ")
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
