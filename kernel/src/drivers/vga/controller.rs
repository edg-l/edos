use x86_64::instructions::port::Port;

use crate::{
    drivers::{pci::structures::PciDevice, vga::error::VgaError},
    log,
};

const DISPI_INDEX_PORT: u16 = 0x01CE;
const DISPI_DATA_PORT: u16 = 0x01CF;

pub const DISPI_INDEX_VIRT_HEIGHT: u16 = 0x07;
pub const DISPI_INDEX_Y_OFFSET: u16 = 0x09;
pub const DISPI_INDEX_VIDEO_MEMORY_64K: u16 = 0x0A;

pub fn dispi_write(index: u16, value: u16) {
    unsafe {
        Port::new(DISPI_INDEX_PORT).write(index);
        Port::new(DISPI_DATA_PORT).write(value);
    }
}

pub fn dispi_read(index: u16) -> u16 {
    unsafe {
        Port::new(DISPI_INDEX_PORT).write(index);
        Port::new(DISPI_DATA_PORT).read()
    }
}

pub struct VgaController {
    pub pci_device: PciDevice,
}

impl VgaController {
    pub fn new(pci_device: PciDevice) -> Result<Self, VgaError> {
        log!(
            "PCI Address: {:02x}:{:02x}.{}",
            pci_device.address.bus,
            pci_device.address.device,
            pci_device.address.function
        );
        log!("Vendor ID: {:#x}", pci_device.header.vendor_id);
        log!("Device ID: {:#x}", pci_device.header.device_id);
        log!("BAR0: {:#x}", pci_device.header.bar0);
        log!("BAR1: {:#x}", pci_device.header.bar1);
        log!("BAR2: {:#x}", pci_device.header.bar2);
        log!("BAR3: {:#x}", pci_device.header.bar3);
        log!("BAR4: {:#x}", pci_device.header.bar4);
        log!("BAR5: {:#x}", pci_device.header.bar5);
        log!(
            "IRQ Line: {}, IRQ Pin: {}",
            pci_device.header.interrupt_line,
            pci_device.header.interrupt_pin
        );

        log!(
            "Initializing VGA controller at {:02x}:{:02x}.{}",
            pci_device.address.bus,
            pci_device.address.device,
            pci_device.address.function
        );

        Ok(Self { pci_device })
    }
}
