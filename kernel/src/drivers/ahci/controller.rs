use core::ptr;

use alloc::vec::Vec;
use x86_64::{PhysAddr, structures::paging::PageTableFlags};

use crate::{
    drivers::{
        ahci::{
            AhciError,
            port::AhciPort,
            structures::{
                GHC_AE, GHC_IE, HbaMemory, HbaPort, PORT_CMD_POD, PORT_CMD_SUD,
                PORT_CMD_ST, PORT_CMD_CR, PORT_CMD_FRE, PORT_CMD_FR,
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

        // Map the HBA memory region (need 0x1100 bytes for full AHCI HBA, round up to 8KB to be safe)
        let hba_size = 0x2000u64; // 8KB to cover the full HBA memory region plus padding
        let mut mapper = memory_mapper();
        mapper
            .map_address_range(
                hba_virt,
                hba_base,
                hba_size as usize,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::NO_CACHE
                    | PageTableFlags::GLOBAL,
            )
            .map_err(|_| AhciError::InvalidDevice)?;

        println!(
            "Mapped HBA memory: virt={:#x}, phys={:#x}, size={:#x}",
            hba_virt.as_u64(),
            hba_base.as_u64(),
            hba_size
        );

        let hba = hba_virt.as_mut_ptr::<HbaMemory>();

        // Print structure validation info first
        crate::drivers::ahci::structures::HbaMemory::print_structure_info();

        println!("=== AHCI Controller Memory Mapping ===");
        println!("HBA Physical Address: {:#x}", hba_base.as_u64());
        println!("HBA Virtual Address: {:#x}", hba_virt.as_u64());
        println!("HBA Pointer: {:p}", hba);

        // Safely read and display the HBA memory contents
        let hba_ref = unsafe { &*hba };
        hba_ref.print_basic_registers();
        hba_ref.print_vendor_area();

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
        ghc |= GHC_IE; // Enable interrupts
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
        while start.elapsed().as_millis() < 10 {
            sched().thread_yield();
        }

        println!("AHCI enabled with interrupts");
        Ok(())
    }

    fn discover_ports(&mut self) -> Result<(), AhciError> {
        let pi = unsafe { ptr::read_volatile(&(*self.hba).pi) };
        self.ports.resize_with(32, || None);

        println!("=== Port Discovery Debug ===");
        println!("Ports Implemented (PI): {:#x}", pi);
        println!("HBA base address: {:p}", self.hba);
        println!(
            "Ports array offset: {:#x}",
            core::mem::offset_of!(HbaMemory, ports)
        );

        for i in 0..32 {
            if pi & (1 << i) != 0 {
                println!("=== Checking Port {} ===", i);

                let port_ptr = unsafe { &raw mut (*self.hba).ports[i] };
                let port_addr = port_ptr as *const HbaPort as usize;
                let expected_offset = self.hba as usize
                    + core::mem::offset_of!(HbaMemory, ports)
                    + (i * core::mem::size_of::<HbaPort>());

                println!(
                    "Port {} address: {:#x} (expected: {:#x})",
                    i, port_addr, expected_offset
                );
                if port_addr != expected_offset {
                    println!("WARNING: Port {} address mismatch!", i);
                }

                // Read and display all port registers for debugging
                let port_ref = unsafe { &*port_ptr };
                port_ref.print_registers(i);

                // Verify memory access is working by reading multiple times
                let ssts1 = unsafe { ptr::read_volatile(&raw const (*port_ptr).ssts) };
                let ssts2 = unsafe { ptr::read_volatile(&raw const (*port_ptr).ssts) };
                let ssts3 = unsafe { ptr::read_volatile(&raw const (*port_ptr).ssts) };

                if ssts1 != ssts2 || ssts2 != ssts3 {
                    println!(
                        "WARNING: SSTS values inconsistent: {:#x}, {:#x}, {:#x}",
                        ssts1, ssts2, ssts3
                    );
                }

                let ssts = ssts1;
                let device_detection = ssts & 0xF; // DET field
                let interface_power = (ssts >> 8) & 0xF; // IPM field

                println!(
                    "SSTS breakdown: DET={}, IPM={}, full={:#x}",
                    device_detection, interface_power, ssts
                );

                if device_detection != 3 || interface_power != 1 {
                    println!(
                        "Port {}: No device present (DET={}, IPM={})",
                        i, device_detection, interface_power
                    );
                    continue;
                }

                println!("Port {}: Device detected (DET=3, IPM=1) ✓", i);

                // Initialize the port properly before reading signature
                println!("Initializing port {}", i);
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

                    println!(
                        "Port {}: Signature read #{}: {:#x}",
                        i, signature_attempts, signature
                    );

                    // Also check all other important registers for changes
                    let cmd = unsafe { ptr::read_volatile(&raw const (*port_ptr).cmd) };
                    let tfd = unsafe { ptr::read_volatile(&raw const (*port_ptr).tfd) };
                    let serr = unsafe { ptr::read_volatile(&raw const (*port_ptr).serr) };
                    println!(
                        "Port {} state: CMD={:#x}, TFD={:#x}, SERR={:#x}",
                        i, cmd, tfd, serr
                    );

                    if signature == 0xffffffff {
                        println!(
                            "Port {}: Signature attempt {} failed, retrying...",
                            i, signature_attempts
                        );
                    }
                }

                println!(
                    "Port {} final signature: {:#x} after {} attempts",
                    i, signature, signature_attempts
                );

                // Display vendor area for this port too
                (unsafe { &*port_ptr }).print_vendor_area(i);

                match signature {
                    SATA_SIG_ATA => {
                        println!("✓ Found SATA drive on port {}", i);
                        // Get volatile pointer to the specific port
                        match AhciPort::new(i, port_ptr) {
                            Ok(port) => {
                                self.ports[i] = Some(port);
                            }
                            Err(e) => {
                                println!("✗ Failed to initialize port {}: {:?}", i, e);
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

                        // Do a memory dump around the signature register for debugging
                        println!("Memory dump around SIG register:");
                        let sig_addr = unsafe { &raw const (*port_ptr).sig };
                        for offset in -2..=2isize {
                            let addr = unsafe { sig_addr.offset(offset) };
                            let value = unsafe { ptr::read_volatile(addr) };
                            println!("  [sig{:+}] {:#x}: {:#x}", offset * 4, addr as usize, value);
                        }
                    }
                }
            }
        }

        println!("Port discovery complete");
        Ok(())
    }

    fn initialize_port(&mut self, port_ptr: *mut HbaPort, port_idx: usize) -> Result<(), AhciError> {
        println!("Initializing port {} registers", port_idx);

        unsafe {
            // First ensure the port is stopped
            let mut cmd = ptr::read_volatile(&raw const (*port_ptr).cmd);
            println!("Port {} CMD before init: {:#x}", port_idx, cmd);

            // Stop command list processing (clear ST bit)
            cmd &= !PORT_CMD_ST;
            ptr::write_volatile(&raw mut (*port_ptr).cmd, cmd);

            // Wait for command list to stop running (CR bit to clear)
            let start = Instant::now();
            while ptr::read_volatile(&raw const (*port_ptr).cmd) & PORT_CMD_CR != 0 {
                if start.elapsed().as_millis() > 500 {
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
            while start.elapsed().as_millis() < 1000 { // 1 second spin-up time
                sched().thread_yield();
            }

            // Check SSTS again after spin-up
            let ssts = ptr::read_volatile(&raw const (*port_ptr).ssts);
            let device_detection = ssts & 0xF;
            let interface_power = (ssts >> 8) & 0xF;
            println!("Port {} SSTS after spin-up: {:#x} (DET={}, IPM={})",
                port_idx, ssts, device_detection, interface_power);

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
                if start.elapsed().as_millis() > 500 {
                    println!("Port {}: Timeout waiting for FIS receive", port_idx);
                    return Err(AhciError::CommandTimeout);
                }
                sched().thread_yield();
            }

            // Start command processing
            cmd = ptr::read_volatile(&raw const (*port_ptr).cmd);
            cmd |= PORT_CMD_ST;
            ptr::write_volatile(&raw mut (*port_ptr).cmd, cmd);

            let final_cmd = ptr::read_volatile(&(*port_ptr).cmd);
            println!("Port {} CMD after init: {:#x}", port_idx, final_cmd);

            // Wait a bit more for device to fully initialize and register FIS
            let start = Instant::now();
            while start.elapsed().as_millis() < 500 { // Additional 500ms wait
                sched().thread_yield();
            }
        }

        println!("Port {} initialization complete", port_idx);
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
