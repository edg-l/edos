use core::ptr;

use alloc::{sync::Arc, vec::Vec};
use spin::mutex::Mutex;
use x86_64::{
    PhysAddr,
    structures::paging::{PageTableFlags, mapper::TranslateResult},
};

use crate::{
    apic::init::configure_device_interrupt,
    drivers::{
        ahci::{
            AhciError,
            port::AhciPort,
            structures::{
                GHC_AE, GHC_IE, HbaMemory, HbaPort, PORT_CMD_CR, PORT_CMD_FR, PORT_CMD_FRE,
                PORT_CMD_POD, PORT_CMD_ST, PORT_CMD_SUD, SATA_SIG_ATA, SATA_SIG_ATAPI,
            },
        },
        pci::structures::PciDevice,
    },
    interrupts::InterruptIndex,
    memory::{get_virt_addr, mapper::memory_mapper},
    println,
    thread::scheduler::sched,
    timer::Instant,
};

pub struct AhciController {
    pub hba: *mut HbaMemory,
    pub ports: Vec<Option<Arc<Mutex<AhciPort>>>>,
    pub pci_device: PciDevice,
}

impl AhciController {
    pub fn new(pci_device: PciDevice) -> Result<Self, AhciError> {
        // Check if this is actually an AHCI controller
        if pci_device.header.class_code != 0x01 || pci_device.header.subclass != 0x06 {
            return Err(AhciError::InvalidDevice);
        }

        println!("=== AHCI Controller Discovery ===");
        println!(
            "PCI Address: {:02x}:{:02x}.{}",
            pci_device.address.bus, pci_device.address.device, pci_device.address.function
        );
        println!("Vendor ID: {:#x}", pci_device.header.vendor_id);
        println!("Device ID: {:#x}", pci_device.header.device_id);
        println!("BAR5: {:#x}", pci_device.header.bar5);
        println!(
            "IRQ Line: {}, IRQ Pin: {}",
            pci_device.header.interrupt_line, pci_device.header.interrupt_pin
        );

        // Check if BAR5 is valid
        if pci_device.header.bar5 == 0 || pci_device.header.bar5 == 0xFFFFFFFF {
            println!("Invalid BAR5, skipping controller");
            return Err(AhciError::InvalidDevice);
        }

        println!(
            "Initializing AHCI controller at {:02x}:{:02x}.{}",
            pci_device.address.bus, pci_device.address.device, pci_device.address.function
        );

        // Map the AHCI HBA memory
        let hba_base = PhysAddr::new((pci_device.header.bar5 & !0xF) as u64);

        let hba_virt = get_virt_addr(hba_base);

        // Map the HBA memory region (need 0x1100 bytes for full AHCI HBA, round up to 8KB to be safe)
        let hba_size = 0x1100;
        {
            let mut mapper = memory_mapper();
            let result = mapper.translate(hba_virt);
            if let TranslateResult::Mapped { .. } = result {
            } else if let Err(e) = mapper.map_address_range(
                hba_virt,
                hba_base,
                hba_size as usize,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::NO_CACHE
                    | PageTableFlags::GLOBAL,
            ) {
                match e {
                    x86_64::structures::paging::mapper::MapToError::PageAlreadyMapped(
                        _phys_frame,
                    ) => {}
                    _ => Err(AhciError::InvalidDevice)?,
                }
            }
        }

        let hba = hba_virt.as_mut_ptr::<HbaMemory>();

        let mut controller = Self {
            hba,
            ports: Vec::new(),
            pci_device,
        };

        controller.reset_controller()?;
        controller.enable_ahci()?;
        controller.discover_ports()?;
        Self::configure_interrupt(&pci_device)?;

        Self::enable_controller_interrupts(hba)?;

        Ok(controller)
    }

    fn configure_interrupt(pci_device: &PciDevice) -> Result<(), AhciError> {
        if pci_device.header.interrupt_line == 0xFF {
            return Err(AhciError::InvalidDevice);
        }

        println!(
            "Configuring AHCI interrupt: IRQ {}",
            pci_device.header.interrupt_line
        );

        // Route hardware IRQ to our AHCI vector
        configure_device_interrupt(
            pci_device.header.interrupt_line,
            InterruptIndex::Ahci.as_u8(),
        )
        .unwrap();

        Ok(())
    }

    fn enable_controller_interrupts(hba: *mut HbaMemory) -> Result<(), AhciError> {
        unsafe {
            // Enable global interrupts
            let mut ghc = ptr::read_volatile(&(*hba).ghc);
            ghc |= GHC_IE;
            ptr::write_volatile(&raw mut (*hba).ghc, ghc);
        }
        Ok(())
    }

    fn reset_controller(&mut self) -> Result<(), AhciError> {
        // Request HBA reset
        let mut ghc = unsafe { ptr::read_volatile(&raw const (*self.hba).ghc) };
        ghc |= 1; // HBA Reset bit
        unsafe { ptr::write_volatile(&raw mut (*self.hba).ghc, ghc) };

        // Wait for reset to complete (should clear the bit)
        let start = Instant::now();
        while unsafe { ptr::read_volatile(&raw const (*self.hba).ghc) } & 1 != 0 {
            if start.elapsed().as_millis() > 5000 {
                return Err(AhciError::CommandTimeout);
            }
            sched().thread_yield();
        }

        println!("AHCI controller reset complete");
        Ok(())
    }

    fn enable_ahci(&mut self) -> Result<(), AhciError> {
        let mut ghc = unsafe { ptr::read_volatile(&raw const (*self.hba).ghc) };
        ghc |= GHC_AE; // Enable AHCI
        unsafe { ptr::write_volatile(&raw mut (*self.hba).ghc, ghc) };

        // Wait for AHCI to be enabled (AE bit should remain set)
        let start = Instant::now();
        while unsafe { ptr::read_volatile(&raw const (*self.hba).ghc) } & GHC_AE == 0 {
            println!("Waiting for ACHI init..");
            if start.elapsed().as_millis() > 1000 {
                return Err(AhciError::CommandTimeout);
            }
            sched().thread_yield();
        }

        // Give the controller a moment to fully initialize
        let start = Instant::now();
        while start.elapsed().as_millis() < 100 {
            sched().thread_yield();
        }

        println!("AHCI enabled with interrupts");
        Ok(())
    }

    fn discover_ports(&mut self) -> Result<(), AhciError> {
        let pi = unsafe { ptr::read_volatile(&(*self.hba).pi) };
        self.ports.resize_with(32, || None);

        for i in 0..32 {
            if pi & (1 << i) != 0 {
                println!("=== Checking Port {} ===", i);
                let port_ptr = unsafe { &raw mut (*self.hba).ports[i] };

                let ssts = unsafe { ptr::read_volatile(&raw const (*port_ptr).ssts) };
                let device_detection = ssts & 0xF; // DET field
                let interface_power = (ssts >> 8) & 0xF; // IPM field

                if device_detection != 3 || interface_power != 1 {
                    println!(
                        "Port {}: No device present (DET={}, IPM={})",
                        i, device_detection, interface_power
                    );
                    continue;
                }

                println!("Port {}: Device detected (DET=3, IPM=1)", i);

                // Initialize the port properly before reading signature
                self.initialize_port(port_ptr, i)?;

                // Wait for signature to stabilize after power-on and track all reads
                let mut signature_attempts = 0;
                let mut signature = 0xffffffff;

                while signature == 0xffffffff && signature_attempts < 10 {
                    let start = Instant::now();
                    while start.elapsed().as_millis() < 100 {
                        sched().thread_yield();
                    }

                    signature = unsafe { ptr::read_volatile(&raw const (*port_ptr).sig) };
                    signature_attempts += 1;
                }

                match signature {
                    SATA_SIG_ATA => {
                        println!("Found SATA drive on port {}", i);
                        // Get volatile pointer to the specific port
                        match AhciPort::new(i, port_ptr) {
                            Ok(port) => {
                                self.ports[i] = Some(Arc::new(Mutex::new(port)));
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
                        println!("Port {} has unsupported/invalid signature: {:#x}", i, sig);
                    }
                }
            }
        }

        println!("Port discovery complete");
        Ok(())
    }

    fn initialize_port(
        &mut self,
        port_ptr: *mut HbaPort,
        port_idx: usize,
    ) -> Result<(), AhciError> {
        println!("Initializing port {} registers", port_idx);

        unsafe {
            // First ensure the port is stopped
            let mut cmd = ptr::read_volatile(&raw const (*port_ptr).cmd);

            // Stop command list processing (clear ST bit)
            cmd &= !PORT_CMD_ST;
            ptr::write_volatile(&raw mut (*port_ptr).cmd, cmd);

            // Wait for command list to stop running (CR bit to clear)
            let start = Instant::now();
            while ptr::read_volatile(&raw const (*port_ptr).cmd) & PORT_CMD_CR != 0 {
                if start.elapsed().as_millis() > 200 {
                    println!("Port {}: Timeout waiting for CR to clear", port_idx);
                    return Err(AhciError::CommandTimeout);
                }
                sched().thread_yield();
            }

            // Clear any error conditions
            ptr::write_volatile(&raw mut (*port_ptr).serr, 0xFFFFFFFF);
            ptr::write_volatile(&raw mut (*port_ptr).is, 0xFFFFFFFF);

            // Set up power management: Power On Device + Spin-Up Device
            cmd = ptr::read_volatile(&(*port_ptr).cmd);
            cmd |= PORT_CMD_POD | PORT_CMD_SUD;
            ptr::write_volatile(&raw mut (*port_ptr).cmd, cmd);

            println!("Port {} powered on, waiting for spin-up", port_idx);

            // Wait a bit for device to spin up
            let start = Instant::now();

            loop {
                // Check SSTS again after spin-up
                let ssts = ptr::read_volatile(&raw const (*port_ptr).ssts);
                let device_detection = ssts & 0xF;
                let interface_power = (ssts >> 8) & 0xF;
                if device_detection != 3 || interface_power != 1 {
                    if start.elapsed().as_millis() > 400 {
                        println!("Port {}: Device not ready after spin-up", port_idx);
                        return Err(AhciError::InvalidDevice);
                    } else {
                        sched().thread_yield();
                    }
                } else {
                    break;
                }
            }

            // Check SSTS again after spin-up
            let ssts = ptr::read_volatile(&raw const (*port_ptr).ssts);
            let device_detection = ssts & 0xF;
            let interface_power = (ssts >> 8) & 0xF;
            println!(
                "Port {} SSTS after spin-up: {:#x} (DET={}, IPM={})",
                port_idx, ssts, device_detection, interface_power
            );

            if device_detection != 3 || interface_power != 1 {
                println!("Port {}: Device not ready after spin-up", port_idx);
                return Err(AhciError::InvalidDevice);
            }

            // Enable FIS receive
            cmd = ptr::read_volatile(&raw const (*port_ptr).cmd);
            cmd |= PORT_CMD_FRE;
            ptr::write_volatile(&mut (*port_ptr).cmd, cmd);

            // Wait for FIS receive to start
            let start = Instant::now();
            while ptr::read_volatile(&raw const (*port_ptr).cmd) & PORT_CMD_FR == 0 {
                if start.elapsed().as_millis() > 200 {
                    println!("Port {}: Timeout waiting for FIS receive", port_idx);
                    return Err(AhciError::CommandTimeout);
                }
                sched().thread_yield();
            }

            // Start command processing
            cmd = ptr::read_volatile(&raw const (*port_ptr).cmd);
            cmd |= PORT_CMD_ST;
            ptr::write_volatile(&raw mut (*port_ptr).cmd, cmd);

            // Wait a bit more for device to fully initialize and register FIS
            let start = Instant::now();
            while start.elapsed().as_millis() < 20 {
                sched().thread_yield();
            }
        }

        println!("Port {} initialization complete", port_idx);
        Ok(())
    }
}
