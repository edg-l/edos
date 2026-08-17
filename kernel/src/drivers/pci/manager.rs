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

    // Step 4: Scan the buses that exist
    //
    // Every probe is a pair of port writes and a read, and every one of those
    // traps to the hypervisor on a virtualised machine. Walking all 256 buses
    // by all 32 devices by all 8 functions is 65536 probes to find the eight
    // devices on bus 0, and it is the single longest thing this kernel does
    // before mounting a filesystem.
    //
    // Two rules remove almost all of it. A device whose function 0 is absent
    // has no other functions, so an empty slot costs one probe rather than
    // eight. And a bus exists only if something bridges to it, so the buses
    // worth visiting are bus 0 plus whatever the bridges found there point at.
    pub fn scan_devices(&mut self) {
        println!("Scanning PCI devices...");

        let mut visited = [false; 256];
        let mut pending = alloc::vec![0u8];
        while let Some(bus) = pending.pop() {
            if visited[bus as usize] {
                continue;
            }
            visited[bus as usize] = true;
            self.scan_bus(bus, &mut pending);
        }

        println!("Found {} PCI devices", self.devices.len());
    }

    /// Record every function on `bus`, queueing the far side of any bridge.
    fn scan_bus(&mut self, bus: u8, pending: &mut alloc::vec::Vec<u8>) {
        for device in 0..32 {
            let zero = PciAddress {
                bus,
                device,
                function: 0,
            };
            let Some(first) = self.read_device_header(zero) else {
                continue;
            };
            // Bit 7 of the header type is the only thing that says whether
            // functions 1..7 are worth asking about.
            let functions = if first.header_type & 0x80 != 0 { 8 } else { 1 };

            for function in 0..functions {
                let address = PciAddress {
                    bus,
                    device,
                    function,
                };
                let header = if function == 0 {
                    first
                } else {
                    match self.read_device_header(address) {
                        Some(header) => header,
                        None => continue,
                    }
                };

                // A PCI-to-PCI bridge names its far side in the secondary bus
                // number, and that bus is where anything behind it lives.
                if header.header_type & 0x7F == 0x01 {
                    let buses = self.read_config_u32(address, 0x18);
                    pending.push(((buses >> 8) & 0xFF) as u8);
                }

                self.devices.push(PciDevice { address, header });
            }
        }
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
