use core::ptr::NonNull;

use alloc::{boxed::Box, vec::Vec};
use volatile::{VolatilePtr, map_field};
use x86_64::{PhysAddr, VirtAddr, structures::paging::PageTableFlags};

use crate::{
    drivers::{
        ahci::{
            AhciError,
            port::AhciPort,
            structures::{
                GHC_AE, GHC_IE, HbaMemory, HbaMemoryVolatileFieldAccess, HbaPort, SATA_SIG_ATA,
            },
        },
        pci::structures::PciDevice,
    },
    memory::{get_virt_addr, mapper::memory_mapper},
    println,
};

pub struct AhciController {
    pub hba: VolatilePtr<'static, HbaMemory>,
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
                PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE | PageTableFlags::GLOBAL,
            )
            .map_err(|_| AhciError::InvalidDevice)?;

        let hba =
            unsafe { VolatilePtr::new(NonNull::new_unchecked(hba_virt.as_mut_ptr::<HbaMemory>())) };

        println!("AHCI Version: {:#x}", map_field!(hba.vs).read());
        println!("AHCI Capabilities: {:#x}", map_field!(hba.cap).read());
        println!("Ports implemented: {:#x}", map_field!(hba.pi).read());

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
        _ = self.hba.ghc().read();
        let mut ghc = self.hba.ghc().read();
        ghc |= 1; // HBA Reset bit
        self.hba.ghc().write(ghc);

        // Wait for reset to complete (should clear the bit)
        let start = crate::timer::Instant::now();
        while self.hba.ghc().read() & 1 != 0 {
            if start.elapsed().as_millis() > 1000 {
                return Err(AhciError::CommandTimeout);
            }
            x86_64::instructions::hlt();
        }

        println!("AHCI controller reset complete");
        Ok(())
    }

    fn enable_ahci(&mut self) -> Result<(), AhciError> {
        let mut ghc = self.hba.ghc().read();
        ghc |= GHC_AE; // Enable AHCI
        ghc |= GHC_IE; // Enable interrupts
        self.hba.ghc().write(ghc);

        println!("AHCI enabled with interrupts");
        Ok(())
    }

    fn discover_ports(&mut self) -> Result<(), AhciError> {
        let pi = self.hba.pi().read();
        self.ports.resize_with(32, || None);

        for i in 0..32 {
            if pi & (1 << i) != 0 {
                println!("Checking port {}", i);

                let signature = self.hba.ports().read()[i].sig;

                match signature {
                    SATA_SIG_ATA => {
                        println!("Found SATA drive on port {}", i);
                        // Get volatile pointer to the specific port
                        let port_ptr = unsafe {
                            self.hba
                                .ports()
                                .map(|x| x.cast::<HbaPort>().offset(i as isize).cast())
                        };
                        match AhciPort::new(i, port_ptr) {
                            Ok(port) => {
                                self.ports[i] = Some(port);
                            }
                            Err(e) => {
                                println!("Failed to initialize port {}: {:?}", i, e);
                            }
                        }
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
        let is = self.hba.is().read();

        // Clear global interrupt status
        self.hba.is().write(is);

        // Handle each port's interrupts
        for (port_idx, port_opt) in self.ports.iter_mut().enumerate() {
            if is & (1 << port_idx) != 0 {
                if let Some(port) = port_opt {
                    port.handle_interrupt();
                }
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
