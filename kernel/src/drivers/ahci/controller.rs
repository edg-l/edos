use core::ptr;

use alloc::vec::Vec;
use x86_64::{PhysAddr, structures::paging::PageTableFlags};

use crate::{
    drivers::{
        ahci::{
            AhciError,
            port::AhciPort,
            structures::{
                GHC_AE, GHC_IE, HbaMemory, PORT_CMD_POD, PORT_CMD_SUD,
                SATA_SIG_ATA, SATA_SIG_ATAPI,
            },
        },
        pci::structures::PciDevice,
    },
    memory::{get_virt_addr, mapper::memory_mapper},
    println,
    thread::scheduler::sched,
    timer::Instant,
};

pub struct AhciController {
    pub hba: *mut HbaMemory,
    pub ports: Vec<Option<AhciPort>>,
    pub pci_device: PciDevice,
}

impl AhciController {
    pub fn new(pci_device: PciDevice) -> Result<Self, AhciError> {
        // Check if this is actually an AHCI controller
        if pci_device.header.class_code != 0x01 || pci_device.header.subclass != 0x06 {
            return Err(AhciError::InvalidDevice);
        }

        println!(
            "Initializing AHCI controller at {:02x}:{:02x}.{}",
            pci_device.address.bus, pci_device.address.device, pci_device.address.function
        );

        // Map the AHCI HBA memory
        let hba_base = PhysAddr::new((pci_device.header.bar5 & !0xF) as u64);
        let hba_virt = get_virt_addr(hba_base);

        // Map the HBA memory region (typically 1KB, but map a full page)
        let mut mapper = memory_mapper();
        mapper
            .map_address(
                hba_virt,
                hba_base,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::NO_CACHE
                    | PageTableFlags::GLOBAL,
            )
            .map_err(|_| AhciError::InvalidDevice)?;

        let hba = hba_virt.as_mut_ptr::<HbaMemory>();

        println!("AHCI Version: {:#x}", unsafe { ptr::read_volatile(&(*hba).vs) });
        println!("AHCI Capabilities: {:#x}", unsafe { ptr::read_volatile(&(*hba).cap) });
        println!("Ports implemented: {:#x}", unsafe { ptr::read_volatile(&(*hba).pi) });

        let mut controller = Self {
            hba,
            ports: Vec::new(),
            pci_device,
        };

        controller.reset_controller()?;
        controller.enable_ahci()?;
        controller.discover_ports()?;

        Ok(controller)
    }

    fn reset_controller(&mut self) -> Result<(), AhciError> {
        // Request HBA reset
        let mut ghc = unsafe { ptr::read_volatile(&(*self.hba).ghc) };
        ghc |= 1; // HBA Reset bit
        unsafe { ptr::write_volatile(&mut (*self.hba).ghc, ghc) };

        // Wait for reset to complete (should clear the bit)
        let start = Instant::now();
        while unsafe { ptr::read_volatile(&(*self.hba).ghc) } & 1 != 0 {
            if start.elapsed().as_millis() > 1000 {
                return Err(AhciError::CommandTimeout);
            }
            sched().thread_yield();
        }

        println!("AHCI controller reset complete");
        Ok(())
    }

    fn enable_ahci(&mut self) -> Result<(), AhciError> {
        let mut ghc = unsafe { ptr::read_volatile(&(*self.hba).ghc) };
        ghc |= GHC_AE; // Enable AHCI
        ghc |= GHC_IE; // Enable interrupts
        unsafe { ptr::write_volatile(&mut (*self.hba).ghc, ghc) };

        // Wait for AHCI to be enabled (AE bit should remain set)
        let start = Instant::now();
        while unsafe { ptr::read_volatile(&(*self.hba).ghc) } & GHC_AE == 0 {
            println!("Waiting for ACHI init..");
            if start.elapsed().as_millis() > 1000 {
                return Err(AhciError::CommandTimeout);
            }
            sched().thread_yield();
        }

        // Give the controller a moment to fully initialize
        let start = Instant::now();
        while start.elapsed().as_millis() < 10 {
            sched().thread_yield();
        }

        println!("AHCI enabled with interrupts");
        Ok(())
    }

    fn discover_ports(&mut self) -> Result<(), AhciError> {
        let pi = unsafe { ptr::read_volatile(&(*self.hba).pi) };
        self.ports.resize_with(32, || None);

        println!("PI: {pi}");
        for i in 0..32 {
            if pi & (1 << i) != 0 {
                println!("Checking port {}", i);

                let port_ptr = unsafe { &raw mut (*self.hba).ports[i] };

                let ssts = unsafe { ptr::read_volatile(&(*port_ptr).ssts) };
                let device_detection = ssts & 0xF; // DET field
                let interface_power = (ssts >> 8) & 0xF; // IPM field

                if device_detection != 3 || interface_power != 1 {
                    println!("Port {}: No device present (SSTS: {:#x})", i, ssts);
                    continue;
                }

                println!("Port {}: Device detected", i);

                let mut need_power_cycle = false;

                // Power up port if needed
                let mut cmd = unsafe { ptr::read_volatile(&(*port_ptr).cmd) };
                if cmd & PORT_CMD_SUD == 0 {
                    println!("Port {}: Spinning up device", i);
                    cmd |= PORT_CMD_SUD; // Spin-up device
                    unsafe { ptr::write_volatile(&mut (*port_ptr).cmd, cmd) };
                    need_power_cycle = true;
                }
                if cmd & PORT_CMD_POD == 0 {
                    println!("Port {}: Powering on device", i);
                    cmd |= PORT_CMD_POD; // Power on device
                    unsafe { ptr::write_volatile(&mut (*port_ptr).cmd, cmd) };
                    need_power_cycle = true;
                }

                // Wait a bit for device to stabilize
                if need_power_cycle {
                    let start = Instant::now();
                    while start.elapsed().as_millis() < 500 {
                        sched().thread_yield();
                    }
                }

                // Wait for signature to stabilize after power-on
                let mut signature_attempts = 0;
                let mut signature = 0xffffffff;

                while signature == 0xffffffff && signature_attempts < 5 {
                    let start = Instant::now();
                    while start.elapsed().as_millis() < 100 {
                        sched().thread_yield();
                    }

                    signature = unsafe { ptr::read_volatile(&(*port_ptr).sig) };
                    signature_attempts += 1;

                    if signature == 0xffffffff {
                        println!("Port {}: Signature attempt {} failed, retrying...", i, signature_attempts);
                    }
                }

                println!("Got port signature: {signature} after {} attempts", signature_attempts);

                match signature {
                    SATA_SIG_ATA => {
                        println!("Found SATA drive on port {}", i);
                        // Get volatile pointer to the specific port
                        match AhciPort::new(i, port_ptr) {
                            Ok(port) => {
                                self.ports[i] = Some(port);
                            }
                            Err(e) => {
                                println!("Failed to initialize port {}: {:?}", i, e);
                            }
                        }
                    }
                    SATA_SIG_ATAPI => {
                        println!(
                            "Found ATAPI device on port {} (sig: {:#x}) - not supported yet",
                            i, signature
                        );
                    }
                    sig => {
                        println!("Port {} has unsupported signature: {:#x}", i, sig);
                    }
                }
            }
        }

        println!("Port discovery complete");
        Ok(())
    }

    pub fn handle_interrupt(&mut self) {
        let is = unsafe { ptr::read_volatile(&(*self.hba).is) };

        // Clear global interrupt status
        unsafe { ptr::write_volatile(&mut (*self.hba).is, is) };

        // Handle each port's interrupts
        for (port_idx, port_opt) in self.ports.iter_mut().enumerate() {
            if is & (1 << port_idx) != 0
                && let Some(port) = port_opt
            {
                port.handle_interrupt();
            }
        }
    }

    pub fn get_port(&mut self, port_idx: usize) -> Option<&mut AhciPort> {
        if port_idx < self.ports.len() {
            self.ports[port_idx].as_mut()
        } else {
            None
        }
    }
}
