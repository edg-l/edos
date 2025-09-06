use spin::{Once, RwLock};

use crate::{drivers::pci::manager::PciManager, println};

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
        println!(
            "PCI Device: {} - {}, irline = {}, irpin = {}",
            class, subclass, device.header.interrupt_line, device.header.interrupt_pin
        );
    }
}
