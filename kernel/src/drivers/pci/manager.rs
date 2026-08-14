use alloc::vec::Vec;

use crate::{
    drivers::pci::{
        config,
        structures::{PciAddress, PciConfigHeader, PciDevice},
    },
    println,
};

pub struct PciManager {
    devices: Vec<PciDevice>,
}

impl PciManager {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    // Delegate to locked helpers so concurrent PCI config access from driver
    // kthreads (via `config::pci_read_*`) and manager code here does not
    // interleave 0xCF8 address writes with 0xCFC data reads. Prior to this
    // refactor, `PciManager` owned its own unlocked port handles, producing
    // garbage BAR reads that aliased heap frames.
    fn read_config_u32(&mut self, address: PciAddress, offset: u8) -> u32 {
        config::pci_read_u32(address, offset)
    }

    // Step 2: Check if device exists
    fn device_exists(&mut self, address: PciAddress) -> bool {
        let vendor_id = self.read_config_u32(address, 0) & 0xFFFF;
        vendor_id != 0xFFFF
    }

    // Step 3: Read full device header
    fn read_device_header(&mut self, address: PciAddress) -> Option<PciConfigHeader> {
        if !self.device_exists(address) {
            return None;
        }

        let mut header_bytes = [0u8; 64];

        // Read header in 32-bit chunks
        for i in 0..16usize {
            let data = self.read_config_u32(address, (i * 4) as u8);
            let bytes = data.to_le_bytes();
            header_bytes[i * 4..(i + 1) * 4].copy_from_slice(&bytes);
        }

        Some(*bytemuck::from_bytes(&header_bytes))
    }

    // Step 4: Scan all possible addresses
    pub fn scan_devices(&mut self) {
        println!("Scanning PCI devices...");

        for bus in 0..256 {
            for device in 0..32 {
                for function in 0..8 {
                    let address = PciAddress {
                        bus: bus as u8,
                        device: device as u8,
                        function: function as u8,
                    };

                    if let Some(header) = self.read_device_header(address) {
                        println!(
                            "Found device {:02x}:{:02x}.{}: {:04x}:{:04x}",
                            bus, device, function, header.vendor_id, header.device_id
                        );

                        self.devices.push(PciDevice { address, header });

                        // If not multi-function device, skip other functions
                        if function == 0 && (header.header_type & 0x80) == 0 {
                            break;
                        }
                    }
                }
            }
        }

        println!("Found {} PCI devices", self.devices.len());
    }

    pub fn get_devices(&self) -> &[PciDevice] {
        &self.devices
    }

    // Helper to decode class information
    pub fn decode_class(class_code: u8, subclass: u8) -> (&'static str, &'static str) {
        match (class_code, subclass) {
            (0x00, 0x01) => ("Unclassified", "VGA Controller"),
            (0x01, 0x01) => ("Storage", "IDE Controller"),
            (0x01, 0x06) => ("Storage", "SATA Controller"),
            (0x02, 0x00) => ("Network", "Ethernet Controller"),
            (0x03, 0x00) => ("Display", "VGA Controller"),
            (0x04, 0x03) => ("Multimedia", "Audio Device"),
            (0x06, 0x00) => ("Bridge", "Host Bridge"),
            (0x06, 0x01) => ("Bridge", "ISA Bridge"),
            (0x06, 0x04) => ("Bridge", "PCI-to-PCI Bridge"),
            (0x0C, 0x03) => ("Serial Bus", "USB Controller"),
            _ => ("Unknown", "Unknown"),
        }
    }
}
