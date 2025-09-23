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
            AhciError, DeviceType,
            port::AhciPort,
            structures::{
                GHC_AE, GHC_IE, HbaMemory, HbaPort, PORT_CMD_CR, PORT_CMD_FR, PORT_CMD_FRE,
                PORT_CMD_POD, PORT_CMD_ST, PORT_CMD_SUD, SATA_SIG_ATA, SATA_SIG_ATAPI,
            },
        },
        msi,
        pci::structures::PciDevice,
    },
    interrupts::InterruptIndex,
    log,
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
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
        let logger = sched().get_logger();
        // Check if this is actually an AHCI controller
        if pci_device.header.class_code != 0x01 || pci_device.header.subclass != 0x06 {
            return Err(AhciError::InvalidDevice);
        }

        log!(logger, "=== AHCI Controller Discovery ===");
        log!(
            logger,
            "PCI Address: {:02x}:{:02x}.{}",
            pci_device.address.bus,
            pci_device.address.device,
            pci_device.address.function
        );
        log!(logger, "Vendor ID: {:#x}", pci_device.header.vendor_id);
        log!(logger, "Device ID: {:#x}", pci_device.header.device_id);
        log!(logger, "BAR5: {:#x}", pci_device.header.bar5);
        log!(
            logger,
            "IRQ Line: {}, IRQ Pin: {}",
            pci_device.header.interrupt_line,
            pci_device.header.interrupt_pin
        );

        // Check if BAR5 is valid
        if pci_device.header.bar5 == 0 || pci_device.header.bar5 == 0xFFFFFFFF {
            log!(logger, "Invalid BAR5, skipping controller");
            return Err(AhciError::InvalidDevice);
        }

        log!(
            logger,
            "Initializing AHCI controller at {:02x}:{:02x}.{}",
            pci_device.address.bus,
            pci_device.address.device,
            pci_device.address.function
        );

        // Map the AHCI HBA memory
        let hba_base = PhysAddr::new((pci_device.header.bar5 & !0xF) as u64);

        let hba_virt = get_virt_addr_from_phys_offset(hba_base);

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
        let logger = sched().get_logger();
        // Prefer MSI if available
        match msi::enable_msi_for_device(pci_device, InterruptIndex::Ahci.as_u8()) {
            Ok(()) => {
                log!(
                    logger,
                    "AHCI: using MSI on {:02x}:{:02x}.{}",
                    pci_device.address.bus,
                    pci_device.address.device,
                    pci_device.address.function
                );
                return Ok(());
            }
            Err(e) => {
                log!(
                    logger,
                    "AHCI: MSI unavailable ({:?}), falling back to IOAPIC IRQ {}",
                    e,
                    pci_device.header.interrupt_line
                );
            }
        }

        if pci_device.header.interrupt_line == 0xFF {
            return Err(AhciError::InvalidDevice);
        }

        // Route legacy hardware IRQ to our AHCI vector
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
        let logger = sched().get_logger();
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

        log!(logger, "AHCI controller reset complete");
        Ok(())
    }

    fn enable_ahci(&mut self) -> Result<(), AhciError> {
        let logger = sched().get_logger();
        let mut ghc = unsafe { ptr::read_volatile(&raw const (*self.hba).ghc) };
        ghc |= GHC_AE; // Enable AHCI
        unsafe { ptr::write_volatile(&raw mut (*self.hba).ghc, ghc) };

        // Wait for AHCI to be enabled (AE bit should remain set)
        let start = Instant::now();
        while unsafe { ptr::read_volatile(&raw const (*self.hba).ghc) } & GHC_AE == 0 {
            if start.elapsed().as_millis() > 1000 {
                return Err(AhciError::CommandTimeout);
            }
            sched().thread_yield();
        }

        log!(logger, "AHCI enabled with interrupts");
        Ok(())
    }

    fn discover_ports(&mut self) -> Result<(), AhciError> {
        let logger = sched().get_logger();
        let pi = unsafe { ptr::read_volatile(&(*self.hba).pi) };
        self.ports.resize_with(32, || None);

        for i in 0..32 {
            if pi & (1 << i) != 0 {
                log!(logger, "=== Checking Port {} ===", i);
                let port_ptr = unsafe { &raw mut (*self.hba).ports[i] };

                let ssts = unsafe { ptr::read_volatile(&raw const (*port_ptr).ssts) };
                let device_detection = ssts & 0xF; // DET field
                let interface_power = (ssts >> 8) & 0xF; // IPM field

                if device_detection != 3 || interface_power != 1 {
                    log!(
                        logger,
                        "Port {}: No device present (DET={}, IPM={})",
                        i,
                        device_detection,
                        interface_power
                    );
                    continue;
                }

                log!(logger, "Port {}: Device detected (DET=3, IPM=1)", i);

                // Basic init only (power/spin-up); AhciPort::new will start FRE/ST once
                self.initialize_port(port_ptr, i)?;

                // Initialize AHCI port (program CLB/FB, enable FRE/ST)
                match AhciPort::new(i, port_ptr, DeviceType::Ata) {
                    Ok(port) => {
                        // Read signature with a short bounded wait after start
                        let mut signature =
                            unsafe { ptr::read_volatile(&raw const (*port_ptr).sig) };
                        if signature == 0xffffffff {
                            let mut attempts = 0;
                            while signature == 0xffffffff && attempts < 5 {
                                sched().thread_sleep(core::time::Duration::from_millis(5));
                                signature =
                                    unsafe { ptr::read_volatile(&raw const (*port_ptr).sig) };
                                attempts += 1;
                            }
                        }

                        match signature {
                            SATA_SIG_ATA => {
                                log!(logger, "Found SATA drive on port {}", i);
                                // Port was initialized with DeviceType::Ata, no change needed
                                let mut port = port;
                                port.set_device_type(DeviceType::Ata);
                                self.ports[i] = Some(Arc::new(Mutex::new(port)));
                            }
                            SATA_SIG_ATAPI => {
                                log!(logger, "Found ATAPI device on port {}", i);
                                // Update device type to ATAPI
                                let mut port = port;
                                port.set_device_type(DeviceType::Atapi);
                                self.ports[i] = Some(Arc::new(Mutex::new(port)));
                            }
                            sig => {
                                log!(
                                    logger,
                                    "Port {} has unsupported/invalid signature: {:#x}",
                                    i,
                                    sig
                                );
                                // Unsupported signature; do not keep the port
                            }
                        }
                    }
                    Err(e) => {
                        log!(logger, "Failed to initialize port {}: {:?}", i, e);
                    }
                }
            }
        }

        log!(logger, "Port discovery complete");
        Ok(())
    }

    fn initialize_port(
        &mut self,
        port_ptr: *mut HbaPort,
        port_idx: usize,
    ) -> Result<(), AhciError> {
        let logger = sched().get_logger();
        log!(logger, "Initializing port {} registers", port_idx);

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
                    log!(logger, "Port {}: Timeout waiting for CR to clear", port_idx);
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

            log!(logger, "Port {} powered on, waiting for spin-up", port_idx);

            // Wait a bit for device to spin up
            let start = Instant::now();

            loop {
                // Check SSTS again after spin-up
                let ssts = ptr::read_volatile(&raw const (*port_ptr).ssts);
                let device_detection = ssts & 0xF;
                let interface_power = (ssts >> 8) & 0xF;
                if device_detection != 3 || interface_power != 1 {
                    if start.elapsed().as_millis() > 400 {
                        log!(logger, "Port {}: Device not ready after spin-up", port_idx);
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
            log!(
                logger,
                "Port {} SSTS after spin-up: {:#x} (DET={}, IPM={})",
                port_idx,
                ssts,
                device_detection,
                interface_power
            );

            if device_detection != 3 || interface_power != 1 {
                log!(logger, "Port {}: Device not ready after spin-up", port_idx);
                return Err(AhciError::InvalidDevice);
            }

            // Do not enable FRE/ST here; AhciPort::new will program CLB/FB and start the port
        }

        log!(logger, "Port {} initialization complete", port_idx);
        Ok(())
    }
}
