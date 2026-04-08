#![expect(unused)]

pub mod device;
pub mod registers;
pub mod rings;

use alloc::vec::Vec;

use x86_64::structures::paging::{PageTableFlags, mapper::MapToError};

use crate::{
    drivers::{
        dma::DmaBuffer,
        pci::{
            config::{pci_read_u16, pci_write_u16, read_bar_phys},
            pci_manager,
            structures::PciAddress,
        },
    },
    interrupts::InterruptIndex,
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
    println,
};

use self::{
    registers::{XhciRegisters, reg_read, reg_write},
    rings::{CommandRing, EventRing},
};

/// PCI class/subclass/prog-if identifying an xHCI controller.
const PCI_CLASS_SERIAL_BUS: u8 = 0x0C;
const PCI_SUBCLASS_USB: u8 = 0x03;
const PCI_PROGIF_XHCI: u8 = 0x30;

#[derive(Debug)]
pub enum XhciError {
    ControllerNotReady,
    ResetTimeout,
    CommandTimeout,
    SlotsFull,
    TransferError(u8), // completion code
    InvalidDevice,
    UnsupportedSpeed,
}

pub struct XhciController {
    regs: XhciRegisters,
    pci_addr: PciAddress,
    context_size: usize,      // 32 or 64 bytes per context entry (HCCPARAMS1.CSZ)
    dcbaa: Option<DmaBuffer>, // Device Context Base Address Array
    scratch_array: Option<DmaBuffer>,
    scratch_pages: Vec<DmaBuffer>,
    command_ring: Option<CommandRing>,
    event_ring: Option<EventRing>,
}

impl XhciController {
    /// Probe PCI for an xHCI controller and map its MMIO region.
    ///
    /// Returns `None` if no xHCI device is found.
    pub fn find_and_init() -> Option<Self> {
        let devices = pci_manager().read().get_devices().to_vec();

        for dev in &devices {
            if dev.header.class_code != PCI_CLASS_SERIAL_BUS
                || dev.header.subclass != PCI_SUBCLASS_USB
                || dev.header.prog_if != PCI_PROGIF_XHCI
            {
                continue;
            }

            println!("xhci: found controller at {:?}", dev.address);

            // Read BAR0 (64-bit MMIO)
            let bar_phys = read_bar_phys(dev.address, 0);
            if bar_phys.as_u64() == 0 {
                println!("xhci: BAR0 is zero, skipping");
                continue;
            }

            println!("xhci: BAR0 at {:#x}", bar_phys.as_u64());

            // Enable PCI bus mastering (Command register bit 2)
            let cmd = pci_read_u16(dev.address, 0x04);
            pci_write_u16(dev.address, 0x04, cmd | (1 << 2));

            // Map BAR0 MMIO region into virtual address space via the HHDM offset.
            // 64 KB is a conservative upper bound for xHCI register space.
            let bar_virt = get_virt_addr_from_phys_offset(bar_phys);
            {
                let mut mapper = memory_mapper();
                let result = mapper.map_address_range(
                    bar_virt,
                    bar_phys,
                    0x10000, // 64 KB
                    PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE | PageTableFlags::GLOBAL,
                );
                match result {
                    Ok(_) => {}
                    Err(MapToError::PageAlreadyMapped(_)) => {}
                    Err(e) => {
                        println!("xhci: failed to map BAR0: {:?}", e);
                        continue;
                    }
                }
            }

            let regs = unsafe { XhciRegisters::new(bar_virt.as_mut_ptr()) };

            // Log controller version and capabilities.
            let version = unsafe { core::ptr::read_volatile(&(*regs.cap()).hciversion) };
            let hcsparams1 = unsafe { core::ptr::read_volatile(&(*regs.cap()).hcsparams1) };
            let max_slots = hcsparams1 & 0xFF;
            let max_ports = (hcsparams1 >> 24) & 0xFF;

            println!(
                "xhci: version {}.{}, {} slots, {} ports",
                version >> 8,
                version & 0xFF,
                max_slots,
                max_ports
            );

            return Some(Self {
                regs,
                pci_addr: dev.address,
                context_size: 32, // updated during init() from HCCPARAMS1.CSZ
                dcbaa: None,
                scratch_array: None,
                scratch_pages: Vec::new(),
                command_ring: None,
                event_ring: None,
            });
        }

        None
    }

    /// Initialize the xHCI controller hardware.
    ///
    /// Must be called after `find_and_init()`.
    pub fn init(&mut self) -> Result<(), &'static str> {
        // 1. Wait for Controller Not Ready (CNR) to clear
        self.wait_for_ready()?;

        // 2. Halt the controller
        self.halt()?;

        // 3. Reset the controller
        self.reset()?;

        // 4. Read capabilities
        let hcsparams1 = unsafe { reg_read(&(*self.regs.cap()).hcsparams1) };
        let hcsparams2 = unsafe { reg_read(&(*self.regs.cap()).hcsparams2) };
        let hccparams1 = unsafe { reg_read(&(*self.regs.cap()).hccparams1) };

        let max_slots = (hcsparams1 & 0xFF) as u8;
        let max_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
        self.context_size = if hccparams1 & (1 << 2) != 0 { 64 } else { 32 };

        println!("xhci: context size = {} bytes", self.context_size);

        // 5. Set MaxSlotsEn in CONFIG register
        unsafe {
            reg_write(&mut (*self.regs.op()).config, max_slots as u32);
        }

        // 6. Allocate DCBAA (Device Context Base Address Array)
        //    Array of (max_slots + 1) 64-bit pointers, 64-byte aligned.
        let dcbaa_size = (max_slots as usize + 1) * 8;
        self.dcbaa = Some(
            DmaBuffer::allocate_sized(dcbaa_size).map_err(|_| "xhci: failed to allocate DCBAA")?,
        );
        let dcbaa_phys = self.dcbaa.as_ref().unwrap().phys_addr().as_u64();

        // 7. Handle scratchpad buffers if required by the controller.
        //    HCSPARAMS2 bits [25:21] = hi, bits [31:27] = lo; count = (hi << 5) | lo
        let scratch_hi = ((hcsparams2 >> 21) & 0x1F) as u32;
        let scratch_lo = ((hcsparams2 >> 27) & 0x1F) as u32;
        let num_scratch = (scratch_hi << 5) | scratch_lo;
        if num_scratch > 0 {
            println!("xhci: allocating {} scratchpad buffers", num_scratch);

            let scratch_array = DmaBuffer::allocate_sized((num_scratch as usize) * 8)
                .map_err(|_| "xhci: failed to allocate scratchpad array")?;

            let pagesize_reg = unsafe { reg_read(&(*self.regs.op()).pagesize) };
            // PAGESIZE register: bit N set means page size is 2^(N+12). Use trailing_zeros to
            // find the lowest set bit.
            let page_size = 1usize << ((pagesize_reg & 0xFFFF).trailing_zeros() + 12);

            for i in 0..num_scratch as usize {
                let page = DmaBuffer::allocate_sized(page_size)
                    .map_err(|_| "xhci: failed to allocate scratchpad page")?;
                let page_phys = page.phys_addr().as_u64();
                unsafe {
                    core::ptr::write_volatile(
                        (scratch_array.as_ptr() as *mut u64).add(i),
                        page_phys,
                    );
                }
                self.scratch_pages.push(page);
            }

            // Write scratchpad array physical address into DCBAA[0]
            unsafe {
                core::ptr::write_volatile(
                    self.dcbaa.as_ref().unwrap().as_ptr() as *mut u64,
                    scratch_array.phys_addr().as_u64(),
                );
            }
            self.scratch_array = Some(scratch_array);
        }

        // Write DCBAAP (split into two 32-bit writes)
        unsafe {
            reg_write(&mut (*self.regs.op()).dcbaap_lo, dcbaa_phys as u32);
            reg_write(&mut (*self.regs.op()).dcbaap_hi, (dcbaa_phys >> 32) as u32);
        }

        // 8. Allocate Command Ring (256 TRBs) and write CRCR.
        //    Bit 0 of CRCR_LO is the initial Consumer Cycle State (cycle=1 matches our ring).
        self.command_ring = Some(CommandRing::new(256));
        let cr_phys = self.command_ring.as_ref().unwrap().phys_addr();
        unsafe {
            reg_write(&mut (*self.regs.op()).crcr_lo, (cr_phys as u32) | 1);
            reg_write(&mut (*self.regs.op()).crcr_hi, (cr_phys >> 32) as u32);
        }

        // 9. Allocate Event Ring (256 TRBs) and program Interrupter 0.
        //    Write order matters: ERSTSZ, ERDP, then ERSTBA (writing ERSTBA triggers hardware).
        self.event_ring = Some(EventRing::new(256));
        let er = self.event_ring.as_ref().unwrap();
        let intr = self.regs.interrupter(0);
        unsafe {
            // ERSTSZ = 1 (one segment)
            reg_write(&mut (*intr).erstsz, 1);
            // ERDP = initial dequeue pointer
            let erdp = er.dequeue_phys();
            reg_write(&mut (*intr).erdp_lo, erdp as u32);
            reg_write(&mut (*intr).erdp_hi, (erdp >> 32) as u32);
            // ERSTBA = segment table base address (write last)
            let erstba = er.erst_phys();
            reg_write(&mut (*intr).erstba_lo, erstba as u32);
            reg_write(&mut (*intr).erstba_hi, (erstba >> 32) as u32);
            // Enable interrupter: IMAN bit 1 = Interrupt Enable
            reg_write(&mut (*intr).iman, reg_read(&(*intr).iman) | (1 << 1));
        }

        // 10. Enable MSI-X (with MSI fallback) so the controller can signal interrupts.
        let devices = pci_manager().read().get_devices().to_vec();
        if let Some(dev) = devices.iter().find(|d| d.address == self.pci_addr) {
            if let Err(e) =
                crate::drivers::msi::enable_msix_for_device(dev, InterruptIndex::Xhci.as_u8(), 0)
            {
                println!("xhci: MSI-X setup failed: {:?}, trying MSI", e);
                if let Err(e2) =
                    crate::drivers::msi::enable_msi_for_device(dev, InterruptIndex::Xhci.as_u8())
                {
                    println!("xhci: MSI setup also failed: {:?}", e2);
                }
            }
        }

        // 11. Start the controller: set Run/Stop (bit 0) and Interrupter Enable (bit 2).
        unsafe {
            let cmd = reg_read(&(*self.regs.op()).usbcmd);
            reg_write(&mut (*self.regs.op()).usbcmd, cmd | (1 << 0) | (1 << 2));
        }

        // Wait for HCHalted (bit 0 of USBSTS) to clear, confirming the controller is running.
        for _ in 0..1_000_000u32 {
            let sts = unsafe { reg_read(&(*self.regs.op()).usbsts) };
            if sts & (1 << 0) == 0 {
                println!("xhci: controller started");
                return Ok(());
            }
            core::hint::spin_loop();
        }

        Err("xhci: controller failed to start")
    }

    /// Wait until Controller Not Ready (CNR, bit 11 of USBSTS) clears.
    fn wait_for_ready(&self) -> Result<(), &'static str> {
        for _ in 0..1_000_000u32 {
            let sts = unsafe { reg_read(&(*self.regs.op()).usbsts) };
            if sts & (1 << 11) == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("xhci: controller not ready timeout")
    }

    /// Halt the controller by clearing the Run/Stop bit and waiting for HCHalted.
    fn halt(&self) -> Result<(), &'static str> {
        unsafe {
            let cmd = reg_read(&(*self.regs.op()).usbcmd);
            reg_write(&mut (*self.regs.op()).usbcmd, cmd & !(1 << 0));
        }
        for _ in 0..1_000_000u32 {
            let sts = unsafe { reg_read(&(*self.regs.op()).usbsts) };
            if sts & (1 << 0) != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("xhci: halt timeout")
    }

    /// Reset the controller by setting HCRST and waiting for it to clear.
    fn reset(&self) -> Result<(), &'static str> {
        unsafe {
            reg_write(&mut (*self.regs.op()).usbcmd, 1 << 1); // HCRST
        }
        for _ in 0..1_000_000u32 {
            let cmd = unsafe { reg_read(&(*self.regs.op()).usbcmd) };
            if cmd & (1 << 1) == 0 {
                // HCRST cleared; also wait for CNR to clear before returning
                return self.wait_for_ready();
            }
            core::hint::spin_loop();
        }
        Err("xhci: reset timeout")
    }
}
