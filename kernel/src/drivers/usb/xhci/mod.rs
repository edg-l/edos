pub mod device;
pub mod registers;
pub mod rings;

use core::time::Duration;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Once;

use x86_64::structures::paging::{PageTableFlags, mapper::MapToError};

use crate::{
    drivers::{
        dma::{DmaBuffer, dma},
        pci::{
            config::{pci_read_u16, pci_write_u16, read_bar_phys},
            pci_manager,
            structures::PciAddress,
        },
    },
    interrupts::InterruptIndex,
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
    println,
    thread::{
        mailbox::Mailbox,
        util::{kthread_exit, queue_spawn_kthread_named_arg},
    },
};

use crate::drivers::usb::hid;

use self::{
    device::{
        ConfigDescriptor, DESC_CONFIGURATION, DESC_DEVICE, DESC_ENDPOINT, DESC_INTERFACE,
        DeviceDescriptor, EndpointDescriptor, HID_PROTOCOL_KEYBOARD, HID_PROTOCOL_MOUSE,
        InterfaceDescriptor, SetupPacket, USB_CLASS_HID, USB_CLASS_MASS_STORAGE, UsbDevice,
        UsbSpeed,
    },
    registers::{XhciRegisters, reg_read, reg_write},
    rings::{
        COMP_SHORT_PACKET, COMP_SUCCESS, CommandRing, EventRing, TRB_DIR_IN, TRB_IDT, TRB_IOC,
        TRB_TYPE_COMMAND_COMPLETION, TRB_TYPE_DATA_STAGE, TRB_TYPE_NORMAL,
        TRB_TYPE_PORT_STATUS_CHANGE, TRB_TYPE_SETUP_STAGE, TRB_TYPE_STATUS_STAGE,
        TRB_TYPE_TRANSFER, TransferRing, Trb,
    },
};
use crate::thread::scheduler::{thread_park, thread_park_while, thread_sleep};

/// PCI class/subclass/prog-if identifying an xHCI controller.
const PCI_CLASS_SERIAL_BUS: u8 = 0x0C;
const PCI_SUBCLASS_USB: u8 = 0x03;
const PCI_PROGIF_XHCI: u8 = 0x30;

#[derive(Debug)]
pub enum XhciError {
    CommandTimeout,
    SlotsFull,
    /// xHCI completion code, carried for the `Debug` rendering in the driver's logs.
    TransferError(
        #[expect(
            dead_code,
            reason = "the controller's completion code, carried for the log line"
        )]
        u8,
    ),
    InvalidDevice,
    UnsupportedSpeed,
}

/// Request payload sent to the xHCI driver thread for block I/O.
#[derive(Debug)]
pub enum UsbBlockRequest {
    Read {
        lba: u64,
        sectors: u16,
    },
    Write {
        lba: u64,
        sectors: u16,
        data: Vec<u8>,
    },
}

/// Response returned to the caller after block I/O completes.
#[derive(Debug)]
pub enum UsbBlockResponse {
    ReadResult(Result<Vec<u8>, XhciError>),
    WriteResult(Result<Vec<u8>, XhciError>),
}

/// Global mailbox for USB block I/O. Initialized once the first USB storage device
/// has been fully configured. Callers must spin-yield until it is available.
pub static USB_BLOCK_MAILBOX: Once<Arc<Mailbox<UsbBlockRequest, UsbBlockResponse>>> = Once::new();

pub struct XhciController {
    regs: XhciRegisters,
    context_size: usize, // 32 or 64 bytes per context entry (HCCPARAMS1.CSZ)
    dcbaa: DmaBuffer,    // Device Context Base Address Array
    /// The scratchpad array the controller reads through DCBAA[0]. The hardware reaches
    /// it by physical address, so nothing reads the field again; holding it is what keeps
    /// the allocation alive for the controller's life.
    #[expect(
        dead_code,
        reason = "scratchpad pages are owned so the controller keeps them; the driver never reads them"
    )]
    scratch_array: Option<DmaBuffer>,
    /// The pages that array points at, held for the same reason.
    #[expect(
        dead_code,
        reason = "scratchpad pages are owned so the controller keeps them; the driver never reads them"
    )]
    scratch_pages: Vec<DmaBuffer>,
    command_ring: CommandRing,
    event_ring: EventRing,
}

impl XhciController {
    /// Probe PCI for an xHCI controller, map its MMIO region and bring the hardware up.
    ///
    /// Returns `None` if no xHCI device is found, or if none of the ones found could be
    /// started.
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

            // SAFETY: `bar_virt` is the HHDM address of BAR0 and the block above
            // mapped 64 KB of MMIO there, which covers the whole xHCI register
            // space, so it is the valid mapped BAR0 pointer `new` requires.
            let regs = unsafe { XhciRegisters::new(bar_virt.as_mut_ptr()) };

            // Log controller version and capabilities.
            // SAFETY: `regs.cap()` points at the capability registers inside the
            // mapping just established, and both fields are read-only hardware
            // values, so a volatile read of each is well formed.
            let version = unsafe { core::ptr::read_volatile(&(*regs.cap()).hciversion) };
            // SAFETY: as above, the adjacent HCSPARAMS1 register.
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

            match Self::bring_up(regs, dev.address) {
                Ok(controller) => return Some(controller),
                Err(e) => {
                    println!("xhci: init failed: {}", e);
                    continue;
                }
            }
        }

        None
    }

    /// Bring the controller hardware up on an already-mapped register block.
    fn bring_up(regs: XhciRegisters, pci_addr: PciAddress) -> Result<Self, &'static str> {
        // 1. Wait for Controller Not Ready (CNR) to clear
        Self::wait_for_ready(&regs)?;

        // 2. Halt the controller
        Self::halt(&regs)?;

        // 3. Reset the controller
        Self::reset(&regs)?;

        // 4. Read capabilities
        // SAFETY: the caller built `regs` from a mapped BAR0, so `regs.cap()`
        // is inside that mapping and each of these three is a read-only
        // capability register within it.
        let hcsparams1 = unsafe { reg_read(&(*regs.cap()).hcsparams1) };
        // SAFETY: as above, HCSPARAMS2.
        let hcsparams2 = unsafe { reg_read(&(*regs.cap()).hcsparams2) };
        // SAFETY: as above, HCCPARAMS1.
        let hccparams1 = unsafe { reg_read(&(*regs.cap()).hccparams1) };

        let max_slots = (hcsparams1 & 0xFF) as u8;
        let context_size = if hccparams1 & (1 << 2) != 0 { 64 } else { 32 };

        // 5. Set MaxSlotsEn in CONFIG register
        // SAFETY: `regs.op()` is the operational register block inside the
        // mapped BAR0. The controller is halted and reset at this point, so it
        // is not consuming CONFIG concurrently.
        unsafe {
            reg_write(&mut (*regs.op()).config, max_slots as u32);
        }

        // 6. Allocate DCBAA (Device Context Base Address Array)
        //    Array of (max_slots + 1) 64-bit pointers, 64-byte aligned.
        let dcbaa_size = (max_slots as usize + 1) * 8;
        let dcbaa = dma()
            .allocate_sized(dcbaa_size)
            .map_err(|_| "xhci: failed to allocate DCBAA")?;
        let dcbaa_phys = dcbaa.phys_addr().as_u64();

        // 7. Handle scratchpad buffers if required by the controller.
        //    xHCI spec §5.3.3: HCSPARAMS2 bits [31:27] = Hi, bits [25:21] = Lo
        //    count = (Hi << 5) | Lo
        let scratch_lo = (hcsparams2 >> 21) & 0x1F;
        let scratch_hi = (hcsparams2 >> 27) & 0x1F;
        let num_scratch = (scratch_hi << 5) | scratch_lo;
        let mut scratch_pages: Vec<DmaBuffer> = Vec::new();
        let mut scratch_array_buf = None;
        if num_scratch > 0 {
            let scratch_array = dma()
                .allocate_sized((num_scratch as usize) * 8)
                .map_err(|_| "xhci: failed to allocate scratchpad array")?;

            // SAFETY: `regs.op()` is inside the mapped BAR0 and PAGESIZE is a
            // read-only register within it.
            let pagesize_reg = unsafe { reg_read(&(*regs.op()).pagesize) };
            // PAGESIZE register: bit N set means page size is 2^(N+12). Use trailing_zeros to
            // find the lowest set bit.
            let page_size = 1usize << ((pagesize_reg & 0xFFFF).trailing_zeros() + 12);

            for i in 0..num_scratch as usize {
                let page = dma()
                    .allocate_sized(page_size)
                    .map_err(|_| "xhci: failed to allocate scratchpad page")?;
                let page_phys = page.phys_addr().as_u64();
                // SAFETY: `scratch_array` is a DMA allocation of `num_scratch`
                // `u64`s and `i` is bounded by that count, so the write is in
                // bounds. DMA buffers are page aligned, so every entry is
                // `u64`-aligned.
                unsafe {
                    core::ptr::write_volatile(
                        (scratch_array.as_ptr() as *mut u64).add(i),
                        page_phys,
                    );
                }
                scratch_pages.push(page);
            }

            // Write scratchpad array physical address into DCBAA[0]
            // SAFETY: the DCBAA is `max_slots + 1` `u64`s, so entry 0 exists;
            // the controller has not been given DCBAAP yet, so nothing else is
            // reading it.
            unsafe {
                core::ptr::write_volatile(
                    dcbaa.as_ptr() as *mut u64,
                    scratch_array.phys_addr().as_u64(),
                );
            }
            scratch_array_buf = Some(scratch_array);
        }

        // Write DCBAAP (split into two 32-bit writes)
        // SAFETY: `regs.op()` is inside the mapped BAR0. The controller is
        // still halted, so it cannot observe the halves separately.
        unsafe {
            reg_write(&mut (*regs.op()).dcbaap_lo, dcbaa_phys as u32);
            reg_write(&mut (*regs.op()).dcbaap_hi, (dcbaa_phys >> 32) as u32);
        }

        // 8. Allocate Command Ring (256 TRBs) and write CRCR.
        //    Bit 0 of CRCR_LO is the initial Consumer Cycle State (cycle=1 matches our ring).
        let command_ring = CommandRing::new(256);
        let cr_phys = command_ring.phys_addr();
        // SAFETY: `regs.op()` is inside the mapped BAR0, and the controller is
        // still halted, so the two halves of CRCR cannot be read apart.
        unsafe {
            reg_write(&mut (*regs.op()).crcr_lo, (cr_phys as u32) | 1);
            reg_write(&mut (*regs.op()).crcr_hi, (cr_phys >> 32) as u32);
        }

        // 9. Allocate Event Ring (256 TRBs) and program Interrupter 0.
        //    Write order matters: ERSTSZ, ERDP, then ERSTBA (writing ERSTBA triggers hardware).
        let event_ring = EventRing::new(256);
        let er = &event_ring;
        let intr = regs.interrupter(0);
        // SAFETY: `regs.interrupter(0)` is the first interrupter register set
        // inside the mapped BAR0's runtime region, and every xHCI controller
        // implements at least interrupter 0. The order below is the one the
        // spec requires: ERSTBA last, because writing it is what arms the ring.
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
        if let Some(dev) = devices.iter().find(|d| d.address == pci_addr)
            && let Err(e) =
                crate::drivers::msi::enable_msix_for_device(dev, InterruptIndex::Xhci.as_u8(), 0)
        {
            println!("xhci: MSI-X setup failed: {:?}, trying MSI", e);
            if let Err(e2) =
                crate::drivers::msi::enable_msi_for_device(dev, InterruptIndex::Xhci.as_u8())
            {
                println!("xhci: MSI setup also failed: {:?}", e2);
            }
        }

        // 11. Start the controller: set Run/Stop (bit 0) and Interrupter Enable (bit 2).
        // SAFETY: `regs.op()` is inside the mapped BAR0. Every ring and array
        // the controller will touch has been allocated and programmed above,
        // so it is safe for it to start consuming them.
        unsafe {
            let cmd = reg_read(&(*regs.op()).usbcmd);
            reg_write(&mut (*regs.op()).usbcmd, cmd | (1 << 0) | (1 << 2));
        }

        // Wait for HCHalted (bit 0 of USBSTS) to clear, confirming the controller is running.
        for _ in 0..1_000_000u32 {
            // SAFETY: `regs.op()` is inside the mapped BAR0 and USBSTS is a
            // register within it; the read is volatile so the poll re-reads it.
            let sts = unsafe { reg_read(&(*regs.op()).usbsts) };
            if sts & (1 << 0) == 0 {
                println!("xhci: controller started");
                return Ok(Self {
                    regs,
                    context_size,
                    dcbaa,
                    scratch_array: scratch_array_buf,
                    scratch_pages,
                    command_ring,
                    event_ring,
                });
            }
            core::hint::spin_loop();
        }

        Err("xhci: controller failed to start")
    }

    /// Wait until Controller Not Ready (CNR, bit 11 of USBSTS) clears.
    fn wait_for_ready(regs: &XhciRegisters) -> Result<(), &'static str> {
        for _ in 0..1_000_000u32 {
            // SAFETY: `regs.op()` is inside the mapped BAR0 and USBSTS is a
            // register within it; the read is volatile so the poll re-reads it.
            let sts = unsafe { reg_read(&(*regs.op()).usbsts) };
            if sts & (1 << 11) == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("xhci: controller not ready timeout")
    }

    /// Halt the controller by clearing the Run/Stop bit and waiting for HCHalted.
    fn halt(regs: &XhciRegisters) -> Result<(), &'static str> {
        // SAFETY: `regs.op()` is inside the mapped BAR0. Clearing Run/Stop is
        // a read-modify-write of USBCMD, and this driver is the only writer.
        unsafe {
            let cmd = reg_read(&(*regs.op()).usbcmd);
            reg_write(&mut (*regs.op()).usbcmd, cmd & !(1 << 0));
        }
        for _ in 0..1_000_000u32 {
            // SAFETY: as above — a volatile poll of USBSTS inside the BAR.
            let sts = unsafe { reg_read(&(*regs.op()).usbsts) };
            if sts & (1 << 0) != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("xhci: halt timeout")
    }

    /// Reset the controller by setting HCRST and waiting for it to clear.
    fn reset(regs: &XhciRegisters) -> Result<(), &'static str> {
        // SAFETY: `regs.op()` is inside the mapped BAR0. The controller is
        // halted by the caller before HCRST is set, which is what the spec
        // requires of a reset (xHCI 1.2 §4.2).
        unsafe {
            reg_write(&mut (*regs.op()).usbcmd, 1 << 1); // HCRST
        }
        for _ in 0..1_000_000u32 {
            // SAFETY: as above — a volatile poll of USBCMD inside the BAR.
            let cmd = unsafe { reg_read(&(*regs.op()).usbcmd) };
            if cmd & (1 << 1) == 0 {
                // HCRST cleared; also wait for CNR to clear before returning
                return Self::wait_for_ready(regs);
            }
            core::hint::spin_loop();
        }
        Err("xhci: reset timeout")
    }

    /// Submit a command TRB and wait (by polling) for the matching Command Completion Event.
    ///
    /// All commands and event ring polling happen in the driver thread; there are no cross-thread
    /// races to worry about here.
    pub fn submit_command(&mut self, trb: Trb) -> Result<Trb, XhciError> {
        let cmd_phys = self.command_ring.push(trb);

        // Ring doorbell 0 — Host Controller Command doorbell.
        // SAFETY: `self.regs` owns the mapped BAR0, and doorbell 0 is the
        // command doorbell every controller implements. The TRB it announces
        // was written to the command ring above.
        unsafe {
            reg_write(self.regs.doorbell(0), 0);
        }

        // Poll the event ring until we see the Command Completion Event whose parameter
        // field contains the physical address of the command TRB we just submitted.
        for _ in 0..5_000_000u32 {
            if let Some(event) = self.event_ring.poll() {
                // Acknowledge the event by advancing the ERDP and clearing EHB (bit 3).
                let erdp = self.event_ring.dequeue_phys();
                let intr = self.regs.interrupter(0);
                // SAFETY: `self.regs` owns the mapped BAR0 and interrupter 0 is
                // inside its runtime region. `erdp` is the event ring's own
                // dequeue address, so handing it back is what tells the
                // controller the slot is free again.
                unsafe {
                    reg_write(&mut (*intr).erdp_lo, (erdp as u32) | (1 << 3));
                    reg_write(&mut (*intr).erdp_hi, (erdp >> 32) as u32);
                }

                if event.trb_type() == TRB_TYPE_COMMAND_COMPLETION && event.parameter == cmd_phys {
                    let comp_code = ((event.status >> 24) & 0xFF) as u8;
                    if comp_code == COMP_SUCCESS {
                        return Ok(event);
                    } else {
                        return Err(XhciError::TransferError(comp_code));
                    }
                }

                if event.trb_type() == TRB_TYPE_PORT_STATUS_CHANGE {
                    let port_id = ((event.parameter >> 24) & 0xFF) as u8;
                    println!("xhci: port {} status change during command", port_id);
                }
            }
            core::hint::spin_loop();
        }

        Err(XhciError::CommandTimeout)
    }

    /// Perform a USB control transfer on a device's EP0 (default control pipe).
    ///
    /// Builds Setup Stage + (optional) Data Stage + Status Stage TRBs, rings the doorbell,
    /// then polls the event ring for the resulting Transfer Event.
    ///
    /// Returns the number of bytes actually transferred.
    pub fn control_transfer(
        &mut self,
        device: &mut UsbDevice,
        setup: SetupPacket,
        data_buf_phys: Option<u64>,
        data_len: u16,
        direction_in: bool,
    ) -> Result<usize, XhciError> {
        let ring = &mut device.ep0_ring;

        // Transfer Request Type field in the Setup Stage TRB:
        //   0 = No Data stage, 2 = OUT Data stage, 3 = IN Data stage.
        let trt: u32 = if data_len == 0 {
            0
        } else if direction_in {
            3
        } else {
            2
        };

        // 1. Setup Stage TRB — the 8-byte setup packet is placed directly in the parameter
        //    field (Immediate Data flag set), so no separate DMA buffer is needed for it.
        // SAFETY: `SetupPacket` is `#[repr(C, packed)]` and exactly the eight
        // bytes of a USB setup packet (USB 2.0 §9.3), and `setup` is a live
        // local, so the slice covers initialised bytes it owns for this scope.
        let setup_bytes =
            unsafe { core::slice::from_raw_parts(&setup as *const SetupPacket as *const u8, 8) };
        let mut setup_param = [0u8; 8];
        setup_param.copy_from_slice(setup_bytes);
        let setup_trb = Trb {
            parameter: u64::from_le_bytes(setup_param),
            status: 8, // TRB Transfer Length = 8 (setup packet is always 8 bytes)
            control: ((TRB_TYPE_SETUP_STAGE as u32) << 10) | TRB_IDT | (trt << 16),
        };
        ring.push(setup_trb);

        // 2. Data Stage TRB (only present when there is a data phase).
        if data_len > 0
            && let Some(buf_phys) = data_buf_phys
        {
            let dir_bit = if direction_in { TRB_DIR_IN } else { 0 };
            let data_trb = Trb {
                parameter: buf_phys,
                status: data_len as u32,
                control: ((TRB_TYPE_DATA_STAGE as u32) << 10) | dir_bit,
            };
            ring.push(data_trb);
        }

        // 3. Status Stage TRB — direction is the complement of the data stage
        //    (or IN when there is no data stage, per xHCI spec §4.11.2.2).
        let status_dir = if data_len > 0 && direction_in {
            0
        } else {
            TRB_DIR_IN
        };
        let status_trb = Trb {
            parameter: 0,
            status: 0,
            control: ((TRB_TYPE_STATUS_STAGE as u32) << 10) | TRB_IOC | status_dir,
        };
        ring.push(status_trb);

        // Ring doorbell for this slot, endpoint 0 (doorbell target = 1).
        // SAFETY: `self.regs` owns the mapped BAR0, and `slot_id` came from an
        // Enable Slot completion, so it indexes a doorbell the controller
        // allocated. Target 1 is EP0's DCI.
        unsafe {
            reg_write(self.regs.doorbell(device.slot_id), 1);
        }

        // Poll for the Transfer Event that corresponds to our Status Stage TRB.
        for _ in 0..5_000_000u32 {
            if let Some(event) = self.event_ring.poll() {
                let erdp = self.event_ring.dequeue_phys();
                let intr = self.regs.interrupter(0);
                // SAFETY: `self.regs` owns the mapped BAR0 and interrupter 0 is
                // inside its runtime region. `erdp` is the event ring's own
                // dequeue address, so handing it back is what tells the
                // controller the slot is free again.
                unsafe {
                    reg_write(&mut (*intr).erdp_lo, (erdp as u32) | (1 << 3));
                    reg_write(&mut (*intr).erdp_hi, (erdp >> 32) as u32);
                }

                if event.trb_type() == TRB_TYPE_TRANSFER {
                    let event_slot = ((event.control >> 24) & 0xFF) as u8;
                    if event_slot != device.slot_id {
                        continue; // not our event, skip
                    }
                    let comp_code = ((event.status >> 24) & 0xFF) as u8;
                    let residual = event.status & 0x00FF_FFFF;
                    if comp_code == COMP_SUCCESS || comp_code == COMP_SHORT_PACKET {
                        return Ok((data_len as u32).saturating_sub(residual) as usize);
                    } else {
                        return Err(XhciError::TransferError(comp_code));
                    }
                }

                // Stale command completion events can arrive here; just ignore them.
                if event.trb_type() == TRB_TYPE_PORT_STATUS_CHANGE {
                    let port_id = ((event.parameter >> 24) & 0xFF) as u8;
                    println!("xhci: port {} status change during transfer", port_id);
                }
            }
            core::hint::spin_loop();
        }

        Err(XhciError::CommandTimeout)
    }

    /// React to a Port Status Change event from the event ring.
    ///
    /// Clears the status change bits in PORTSC, resets the port if needed (USB 2.0),
    /// and calls `enumerate_device` to produce a `UsbDevice`.
    pub fn handle_port_status_change(&mut self, port_id: u8) -> Result<UsbDevice, XhciError> {
        // port_id in xHCI events is 1-based; regs.port() takes a 0-based index.
        let port = self.regs.port(port_id - 1);

        // SAFETY: `port` came from `regs.port()`, which asserts the index is
        // below MaxPorts and returns a pointer inside the mapped BAR0.
        let portsc = unsafe { reg_read(&(*port).portsc) };

        // Preserve Port Power (bit 9).  Write 1 to the W1C status bits to clear them.
        // Do NOT write 1 to PED (bit 1) — that would disable the port.
        let pp_bit: u32 = 1 << 9;
        let w1c_bits: u32 = (1 << 17) // CSC – Connect Status Change
            | (1 << 18)               // PEC – Port Enabled/Disabled Change
            | (1 << 19)               // WRC – Warm Port Reset Change
            | (1 << 20)               // OCC – Over-current Change
            | (1 << 21)               // PRC – Port Reset Change
            | (1 << 22)               // PLC – Port Link State Change
            | (1 << 23); // CEC – Port Config Error Change
        // SAFETY: `port` is inside the mapped BAR0, as above. Writing the W1C
        // change bits back is how they are cleared; Port Power is carried over
        // so the write does not turn the port off.
        unsafe {
            reg_write(&mut (*port).portsc, (portsc & pp_bit) | w1c_bits);
        }

        let ccs = portsc & (1 << 0); // Current Connect Status
        let ped = portsc & (1 << 1); // Port Enabled/Disabled
        let speed = ((portsc >> 10) & 0xF) as u8;

        if ccs == 0 {
            // Nothing connected on this port — device disconnected event.
            return Err(XhciError::InvalidDevice);
        }

        if ped == 0 {
            // Port is connected but not yet enabled.  For USB 2.0 devices we must issue a
            // port reset, which the controller performs and then sets PRC when done.
            // SAFETY: `port` is inside the mapped BAR0, as above.
            unsafe {
                let sc = reg_read(&(*port).portsc);
                reg_write(&mut (*port).portsc, (sc & pp_bit) | (1 << 4)); // PR – Port Reset
            }

            for _ in 0..1_000_000u32 {
                // SAFETY: as above — a volatile poll of the same PORTSC.
                let sc = unsafe { reg_read(&(*port).portsc) };
                if sc & (1 << 21) != 0 {
                    // Clear PRC (Port Reset Change) by writing 1 to it.
                    // SAFETY: as above; PRC is W1C and Port Power is carried
                    // over so clearing it does not turn the port off.
                    unsafe { reg_write(&mut (*port).portsc, (sc & pp_bit) | (1 << 21)) };
                    break;
                }
                core::hint::spin_loop();
            }

            // Re-read PORTSC after the reset completes to get the updated speed and PED.
            // SAFETY: as above — the same in-BAR register, read volatile.
            let portsc_new = unsafe { reg_read(&(*port).portsc) };
            if portsc_new & (1 << 1) == 0 {
                // Port still not enabled after reset.
                return Err(XhciError::InvalidDevice);
            }
            let speed_new = ((portsc_new >> 10) & 0xF) as u8;
            return self.enumerate_device(port_id, speed_new);
        }

        self.enumerate_device(port_id, speed)
    }

    /// Enumerate a USB device that has just been connected and reset on `port_id`.
    ///
    /// Performs: Enable Slot → allocate contexts → Address Device.
    /// Returns a `UsbDevice` ready for further descriptor fetching.
    fn enumerate_device(&mut self, port_id: u8, port_speed: u8) -> Result<UsbDevice, XhciError> {
        let speed = UsbSpeed::from_port_speed(port_speed).ok_or(XhciError::UnsupportedSpeed)?;

        println!(
            "xhci: enumerating device on port {}, speed {:?}",
            port_id, speed
        );

        // Step 1 — Enable Slot: ask the controller for a slot ID.
        let completion = self.submit_command(Trb::enable_slot())?;
        // Slot ID is in bits [31:24] of the completion event's *control* field
        let slot_id = ((completion.control >> 24) & 0xFF) as u8;

        if slot_id == 0 {
            return Err(XhciError::SlotsFull);
        }

        // Step 2 — Allocate the Input Context.
        // Layout: InputControlContext (1 × context_size) + SlotContext (1 × context_size)
        //         + 31 Endpoint Contexts = 33 × context_size bytes total.
        let ctx_size = self.context_size;
        let input_ctx = dma()
            .allocate_sized(33 * ctx_size)
            .map_err(|_| XhciError::InvalidDevice)?;

        // Step 3 — Allocate EP0 Transfer Ring (64 TRBs is ample for control transfers).
        let ep0_ring = TransferRing::new(64);

        // Step 4 — Fill Input Control Context.
        // Offset 0 = Drop Context Flags, offset 4 = Add Context Flags.
        // We add Slot (bit 0) and EP0 (bit 1) → Add Flags = 0b11 = 0x3.
        let icc_ptr = input_ctx.as_ptr() as *mut u32;
        // SAFETY: `input_ctx` is a `33 * ctx_size` DMA allocation whose first
        // `ctx_size` bytes are the Input Control Context, and `ctx_size` is 32
        // or 64, so both dwords are in bounds. DMA buffers are page aligned.
        unsafe {
            core::ptr::write_volatile(icc_ptr, 0); // Drop Context Flags = 0
            core::ptr::write_volatile(icc_ptr.add(1), 0x3); // Add Context Flags: Slot + EP0
        }

        // Step 5 — Fill Slot Context (at offset 1 × context_size).
        // SAFETY: the Slot Context is the second of the 33 contexts in the
        // `33 * ctx_size` allocation, so this offset is in bounds.
        let slot_ctx_ptr = unsafe { input_ctx.as_ptr().add(ctx_size) as *mut u32 };
        // SAFETY: `slot_ctx_ptr` starts a `ctx_size`-byte context, so dwords 0
        // and 1 are inside it, and the allocation is page aligned.
        unsafe {
            // Dword 0: Speed (bits [23:20]), Context Entries = 1 (bits [31:27]).
            let dword0 = (speed.to_slot_speed() << 20) | (1 << 27);
            core::ptr::write_volatile(slot_ctx_ptr, dword0);
            // Dword 1: Root Hub Port Number in bits [23:16].
            let dword1 = (port_id as u32) << 16;
            core::ptr::write_volatile(slot_ctx_ptr.add(1), dword1);
        }

        // Step 6 — Fill EP0 Context (at offset 2 × context_size).
        // EP Type 4 = Control Bidirectional.
        // SAFETY: the EP0 Context is the third of the 33 contexts in the
        // allocation, so this offset is in bounds.
        let ep0_ctx_ptr = unsafe { input_ctx.as_ptr().add(2 * ctx_size) as *mut u32 };
        let max_packet = speed.default_max_packet_size();
        // SAFETY: `ep0_ctx_ptr` starts a `ctx_size`-byte context and `ctx_size`
        // is at least 32, so dwords 1 through 4 are inside it.
        unsafe {
            // Dword 1: EP Type (bits [5:3] = 4), Max Packet Size (bits [31:16]).
            let dword1 = (4u32 << 3) | ((max_packet as u32) << 16);
            core::ptr::write_volatile(ep0_ctx_ptr.add(1), dword1);
            // Dwords 2-3: TR Dequeue Pointer with DCS=1 (bit 0 indicates initial cycle state).
            let tr_phys = ep0_ring.phys_addr() | 1;
            core::ptr::write_volatile(ep0_ctx_ptr.add(2), tr_phys as u32);
            core::ptr::write_volatile(ep0_ctx_ptr.add(3), (tr_phys >> 32) as u32);
            // Dword 4: Average TRB Length — 8 bytes is a reasonable default for control.
            core::ptr::write_volatile(ep0_ctx_ptr.add(4), 8);
        }

        // Step 7 — Allocate Output Device Context and register it in the DCBAA.
        // Layout: SlotContext + 31 EndpointContexts = 32 × context_size bytes.
        let output_ctx = dma()
            .allocate_sized(32 * ctx_size)
            .map_err(|_| XhciError::InvalidDevice)?;
        let output_ctx_phys = output_ctx.phys_addr().as_u64();

        // SAFETY: the DCBAA holds `max_slots + 1` `u64`s and `slot_id` came
        // from an Enable Slot completion, so the controller allocated it within
        // MaxSlots and the entry is in bounds. The write is volatile because
        // the controller reads the array itself.
        unsafe {
            let entry = (self.dcbaa.as_ptr() as *mut u64).add(slot_id as usize);
            core::ptr::write_volatile(entry, output_ctx_phys);
        }

        // Step 8 — Address Device command: assigns the USB address and transitions the
        // device to the Addressed state.
        let input_ctx_phys = input_ctx.phys_addr().as_u64();
        self.submit_command(Trb::address_device(input_ctx_phys, slot_id, false))
            .map_err(|e| {
                println!("xhci: address device failed: {:?}", e);
                XhciError::InvalidDevice
            })?;

        Ok(UsbDevice {
            slot_id,
            speed,
            ep0_ring,
            input_ctx,
            output_ctx,
            device_descriptor: None,
            config_data: None,
        })
    }

    /// Read the device descriptor from a USB device.
    pub fn get_device_descriptor(
        &mut self,
        device: &mut UsbDevice,
    ) -> Result<DeviceDescriptor, XhciError> {
        let buf = dma()
            .allocate_sized(18)
            .map_err(|_| XhciError::InvalidDevice)?;
        let buf_phys = buf.phys_addr().as_u64();

        let setup = SetupPacket {
            bm_request_type: 0x80,              // Device-to-host, Standard, Device
            b_request: 6,                       // GET_DESCRIPTOR
            w_value: (DESC_DEVICE as u16) << 8, // Descriptor type = Device, index = 0
            w_index: 0,
            w_length: 18,
        };

        let mut transferred = self.control_transfer(device, setup, Some(buf_phys), 18, true)?;

        if transferred < 18 {
            // USB 2.0 §9.4.3: a device whose bMaxPacketSize0 is smaller than the
            // request answers only the first packet. Read that 8-byte prefix,
            // which is what carries bMaxPacketSize0, then ask for the whole
            // descriptor again -- the retry is the only way the remaining ten
            // bytes, idVendor/idProduct/bNumConfigurations among them, arrive.
            let setup8 = SetupPacket {
                w_length: 8,
                ..setup
            };
            if self.control_transfer(device, setup8, Some(buf_phys), 8, true)? >= 8 {
                transferred = self.control_transfer(device, setup, Some(buf_phys), 18, true)?;
            } else {
                transferred = 0;
            }
        }

        // A pooled DMA buffer carries whatever its previous owner left in it, so only the
        // bytes the device actually sent may be read; the rest of the descriptor stays zero.
        let valid = transferred.min(18);
        let mut raw = [0u8; 18];
        // SAFETY: `buf` is an 18-byte DMA allocation, `raw` is an 18-byte
        // array and `valid` is clamped to 18, so both sides are in bounds; the
        // two are distinct allocations.
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), raw.as_mut_ptr(), valid);
        }
        if let Err(e) = dma().dealloc(buf) {
            println!("xhci: dma dealloc failed: {e}");
        }
        if valid < 8 {
            return Err(XhciError::InvalidDevice);
        }
        // SAFETY: `DeviceDescriptor` is the 18-byte wire layout (USB 2.0
        // §9.6.1) and `raw` is 18 initialised bytes, so the read is in bounds.
        // It is unaligned because the type is `packed` and `raw` is a `[u8]`.
        Ok(unsafe { core::ptr::read_unaligned(raw.as_ptr() as *const DeviceDescriptor) })
    }

    /// Read the full configuration descriptor (with all interfaces and endpoints).
    ///
    /// First reads 9 bytes to get wTotalLength, then reads the full blob into a
    /// properly-sized buffer regardless of how large the descriptor is.
    pub fn get_config_descriptor(
        &mut self,
        device: &mut UsbDevice,
        config_index: u8,
    ) -> Result<Vec<u8>, XhciError> {
        // Phase 1: Read just the 9-byte config descriptor header to get wTotalLength.
        let hdr_buf = dma()
            .allocate_sized(9)
            .map_err(|_| XhciError::InvalidDevice)?;
        let hdr_phys = hdr_buf.phys_addr().as_u64();

        let setup = SetupPacket {
            bm_request_type: 0x80,
            b_request: 6, // GET_DESCRIPTOR
            w_value: ((DESC_CONFIGURATION as u16) << 8) | (config_index as u16),
            w_index: 0,
            w_length: 9,
        };

        let hdr_len = self.control_transfer(device, setup, Some(hdr_phys), 9, true)?;

        // A pooled DMA buffer is not zeroed on reuse, so a short transfer would otherwise
        // hand back the previous owner's bytes as a descriptor.
        let mut hdr_raw = [0u8; 9];
        // SAFETY: `hdr_buf` is a 9-byte DMA allocation, `hdr_raw` is a 9-byte
        // array and the count is clamped to 9, so both sides are in bounds.
        unsafe {
            core::ptr::copy_nonoverlapping(hdr_buf.as_ptr(), hdr_raw.as_mut_ptr(), hdr_len.min(9));
        }
        // SAFETY: `ConfigDescriptor` is the 9-byte wire layout (USB 2.0
        // §9.6.3) and `hdr_raw` is 9 initialised bytes. Unaligned because the
        // type is `packed` and the source is a `[u8]`.
        let config_hdr =
            unsafe { core::ptr::read_unaligned(hdr_raw.as_ptr() as *const ConfigDescriptor) };
        let total_len = config_hdr.w_total_length;

        if hdr_len < 9 || total_len < 9 {
            if let Err(e) = dma().dealloc(hdr_buf) {
                println!("xhci: dma dealloc failed: {e}");
            }
            return Ok(alloc::vec![0u8; 0]);
        }

        // Phase 2: Allocate a buffer exactly the size of the full descriptor and read it.
        let full_buf = dma()
            .allocate_sized(total_len as usize)
            .map_err(|_| XhciError::InvalidDevice)?;
        let full_phys = full_buf.phys_addr().as_u64();

        let setup_full = SetupPacket {
            w_length: total_len,
            ..setup
        };
        let full_len =
            self.control_transfer(device, setup_full, Some(full_phys), total_len, true)?;

        // Keep only what the device sent: the descriptor walk stops at the first descriptor
        // that does not fit, so a short transfer truncates the blob rather than parsing
        // whatever the pooled buffer held before.
        let mut data = alloc::vec![0u8; full_len.min(total_len as usize)];
        // SAFETY: `full_buf` is a `total_len`-byte DMA allocation and `data` is
        // at most that long, so `data.len()` bytes are in bounds on both sides.
        unsafe {
            core::ptr::copy_nonoverlapping(full_buf.as_ptr(), data.as_mut_ptr(), data.len());
        }
        if let Err(e) = dma().dealloc(hdr_buf) {
            println!("xhci: dma dealloc failed: {e}");
        }
        if let Err(e) = dma().dealloc(full_buf) {
            println!("xhci: dma dealloc failed: {e}");
        }
        Ok(data)
    }

    /// Send SET_CONFIGURATION to activate a configuration.
    pub fn set_configuration(
        &mut self,
        device: &mut UsbDevice,
        config_value: u8,
    ) -> Result<(), XhciError> {
        let setup = SetupPacket {
            bm_request_type: 0x00, // Host-to-device, Standard, Device
            b_request: 9,          // SET_CONFIGURATION
            w_value: config_value as u16,
            w_index: 0,
            w_length: 0,
        };

        self.control_transfer(device, setup, None, 0, false)?;
        Ok(())
    }

    /// Read an interface's HID report descriptor, which is what says where the
    /// fields of its reports are and what they mean.
    ///
    /// The transfer can come back short; the caller gets what arrived rather
    /// than a fixed length, because a pooled DMA buffer is not zeroed on reuse
    /// and the tail would otherwise be the previous owner's bytes parsed as
    /// descriptor items.
    pub fn get_report_descriptor(
        &mut self,
        device: &mut UsbDevice,
        interface: u8,
        length: u16,
    ) -> Result<Vec<u8>, XhciError> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let buf = dma()
            .allocate_sized(length as usize)
            .map_err(|_| XhciError::InvalidDevice)?;
        let phys = buf.phys_addr().as_u64();

        let setup = SetupPacket {
            bm_request_type: 0x81, // Device-to-host, Standard, Interface
            b_request: 6,          // GET_DESCRIPTOR
            w_value: 0x2200,       // Report descriptor, index 0
            w_index: interface as u16,
            w_length: length,
        };

        let result = self.control_transfer(device, setup, Some(phys), length, true);
        let descriptor = match result {
            Ok(len) => {
                let len = len.min(length as usize);
                let mut out = alloc::vec![0u8; len];
                // SAFETY: `buf` is a `length`-byte DMA allocation and `out` is
                // `len` bytes with `len <= length`, so the copy is in bounds on
                // both sides; the two are distinct allocations.
                unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), out.as_mut_ptr(), len) };
                Ok(out)
            }
            Err(e) => Err(e),
        };

        if let Err(e) = dma().dealloc(buf) {
            println!("xhci: dma dealloc failed: {e}");
        }
        descriptor
    }

    /// Send SET_PROTOCOL to switch a HID device to boot protocol.
    ///
    /// Boot protocol provides fixed 8-byte keyboard reports or 3-byte mouse reports.
    pub fn set_hid_protocol(
        &mut self,
        device: &mut UsbDevice,
        interface: u8,
        protocol: u8,
    ) -> Result<(), XhciError> {
        let setup = SetupPacket {
            bm_request_type: 0x21, // Host-to-device, Class, Interface
            b_request: 0x0B,       // SET_PROTOCOL
            w_value: protocol as u16,
            w_index: interface as u16,
            w_length: 0,
        };
        self.control_transfer(device, setup, None, 0, false)?;
        Ok(())
    }

    /// Set up an interrupt IN endpoint on a device.
    ///
    /// Returns the TransferRing for the endpoint.
    pub fn configure_interrupt_endpoint(
        &mut self,
        device: &mut UsbDevice,
        ep_addr: u8,
        max_packet: u16,
        interval: u8,
    ) -> Result<TransferRing, XhciError> {
        let ep_num = ep_addr & 0x0F;
        let ep_dir_in = ep_addr & 0x80 != 0;
        // xHCI Endpoint Index: EP1 IN = 3, EP1 OUT = 2, EP2 IN = 5, etc.
        // Formula: ep_index = ep_num * 2 + (if IN then 1 else 0)
        let ep_dci = ep_num * 2 + if ep_dir_in { 1 } else { 0 };

        // **Serviced at least this often, whatever the descriptor asks.**
        // `bInterval` is the longest a device is willing to wait between polls,
        // not the shortest it may be asked: an interrupt IN endpoint with
        // nothing to say answers NAK, which costs a transaction and no more.
        //
        // It matters because an interrupt endpoint carries one report per
        // service interval, so the descriptor's period is a hard ceiling on how
        // fast anything queued behind it drains. QEMU's `usb-mouse` asks for
        // 8 ms and then emits a few hundred relative deltas to walk the pointer
        // across the screen, which at 125 a second is seconds of backlog and a
        // pointer that arrives late and in bursts. Polling every millisecond
        // drains the same queue eight times faster.
        //
        // 1 ms is also what a modern pointing device asks for unprompted, and
        // the floor xHCI allows at full and low speed (6.2.3.6).
        const FASTEST_INTERVAL: u8 = 3; // 2^3 * 125us
        let asked = device.speed.interrupt_interval(interval);
        let interval_val = asked.min(FASTEST_INTERVAL);
        println!(
            "xhci: ep {:#04x} {:?} bInterval={} asks {} us, serviced every {} us",
            ep_addr,
            device.speed,
            interval,
            125u32 << asked,
            125u32 << interval_val
        );

        let ctx_size = self.context_size;
        let ring = TransferRing::new(64);

        let input_ctx = &device.input_ctx;

        // Clear and set Input Control Context
        let icc = input_ctx.as_ptr() as *mut u32;
        // SAFETY: `input_ctx` is the device's `33 * ctx_size` DMA allocation
        // and the Input Control Context is its first `ctx_size` bytes, so both
        // dwords are in bounds and `u32`-aligned.
        unsafe {
            // Drop Context Flags = 0
            core::ptr::write_volatile(icc, 0);
            // Add Context Flags: set bit for slot context (0) and the endpoint (ep_dci)
            core::ptr::write_volatile(icc.add(1), (1 << 0) | (1 << ep_dci));
        }

        // Update Slot Context: set Context Entries to the maximum of the current value and ep_dci
        // SAFETY: the Slot Context is the second of the 33 contexts in the
        // allocation, so this offset is in bounds.
        let slot_ctx = unsafe { input_ctx.as_ptr().add(ctx_size) } as *mut u32;
        // SAFETY: `slot_ctx` starts a `ctx_size`-byte context, so dword 0 is
        // inside it. The read-modify-write is not racy: the controller only
        // consumes the input context while a Configure Endpoint command is in
        // flight, and this driver thread issues that command below.
        unsafe {
            let dword0 = core::ptr::read_volatile(slot_ctx);
            // Context Entries is bits [31:27] - must cover all configured endpoints
            let current_entries = (dword0 >> 27) & 0x1F;
            let new_entries = current_entries.max(ep_dci as u32);
            let dword0_new = (dword0 & !(0x1F << 27)) | (new_entries << 27);
            core::ptr::write_volatile(slot_ctx, dword0_new);
        }

        // Set up Endpoint Context for the interrupt IN endpoint
        // SAFETY: `ep_dci` is at most 31, so context index `ep_dci + 1` is at
        // most 32 and stays inside the 33-context allocation.
        let ep_ctx =
            unsafe { input_ctx.as_ptr().add((ep_dci as usize + 1) * ctx_size) } as *mut u32;
        // SAFETY: `ep_ctx` starts a `ctx_size`-byte context and `ctx_size` is
        // at least 32, so dwords 0 through 4 are inside it.
        unsafe {
            // Dword 0: Interval (bits [23:16]). What `bInterval` means depends
            // on the speed; see `UsbSpeed::interrupt_interval`.
            core::ptr::write_volatile(ep_ctx, (interval_val as u32) << 16);

            // Dword 1: EP Type (bits [5:3]) = 7 (Interrupt IN), Max Packet Size (bits [31:16])
            // CErr (bits [2:1]) = 3 (retry up to 3 times)
            let dword1 = (7u32 << 3) | ((max_packet as u32) << 16) | (3 << 1);
            core::ptr::write_volatile(ep_ctx.add(1), dword1);

            // Dwords 2-3: TR Dequeue Pointer with DCS=1
            let tr_phys = ring.phys_addr() | 1;
            core::ptr::write_volatile(ep_ctx.add(2), tr_phys as u32);
            core::ptr::write_volatile(ep_ctx.add(3), (tr_phys >> 32) as u32);

            // Dword 4: Average TRB Length
            core::ptr::write_volatile(ep_ctx.add(4), max_packet as u32);
        }

        // Submit Configure Endpoint command
        let input_ctx_phys = input_ctx.phys_addr().as_u64();
        self.submit_command(Trb::configure_endpoint(input_ctx_phys, device.slot_id))?;

        println!(
            "xhci: configured EP{} IN interrupt, maxpkt={}",
            ep_num, max_packet
        );

        Ok(ring)
    }

    /// Configure both bulk IN and bulk OUT endpoints in one Configure Endpoint command.
    ///
    /// Returns `(in_ring, out_ring)`.
    pub fn configure_bulk_endpoints(
        &mut self,
        device: &mut UsbDevice,
        ep_in_addr: u8,
        ep_in_maxpkt: u16,
        ep_out_addr: u8,
        ep_out_maxpkt: u16,
    ) -> Result<(TransferRing, TransferRing), XhciError> {
        let ep_in_num = ep_in_addr & 0x0F;
        let ep_out_num = ep_out_addr & 0x0F;
        // xHCI DCI: ep_num * 2 + (1 if IN, 0 if OUT)
        let ep_in_dci = ep_in_num * 2 + 1;
        let ep_out_dci = ep_out_num * 2;

        let ctx_size = self.context_size;
        let in_ring = TransferRing::new(64);
        let out_ring = TransferRing::new(64);

        let input_ctx = &device.input_ctx;
        let icc = input_ctx.as_ptr() as *mut u32;
        // SAFETY: `input_ctx` is the device's `33 * ctx_size` DMA allocation
        // and the Input Control Context is its first `ctx_size` bytes, so both
        // dwords are in bounds and `u32`-aligned.
        unsafe {
            // Drop Context Flags = 0
            core::ptr::write_volatile(icc, 0);
            // Add Context Flags: slot (bit 0) + EP IN (bit ep_in_dci) + EP OUT (bit ep_out_dci)
            core::ptr::write_volatile(icc.add(1), (1 << 0) | (1 << ep_in_dci) | (1 << ep_out_dci));
        }

        // Update Slot Context: Context Entries must cover the highest DCI used
        // SAFETY: the Slot Context is the second of the 33 contexts in the
        // allocation, so this offset is in bounds.
        let slot_ctx = unsafe { input_ctx.as_ptr().add(ctx_size) as *mut u32 };
        // SAFETY: `slot_ctx` starts a `ctx_size`-byte context, so dword 0 is
        // inside it, and only this driver thread writes the input context.
        unsafe {
            let dword0 = core::ptr::read_volatile(slot_ctx);
            let current_entries = (dword0 >> 27) & 0x1F;
            let new_entries = current_entries.max(ep_in_dci as u32).max(ep_out_dci as u32);
            core::ptr::write_volatile(slot_ctx, (dword0 & !(0x1F << 27)) | (new_entries << 27));
        }

        // EP Context for bulk IN (type 6)
        // SAFETY: `ep_in_dci` is `(addr & 0x0F) * 2 + 1`, at most 31, so
        // context index `ep_in_dci + 1` stays inside the 33-context allocation.
        let ep_in_ctx =
            unsafe { input_ctx.as_ptr().add((ep_in_dci as usize + 1) * ctx_size) as *mut u32 };
        // SAFETY: `ep_in_ctx` starts a `ctx_size`-byte context and `ctx_size`
        // is at least 32, so dwords 0 through 4 are inside it.
        unsafe {
            core::ptr::write_volatile(ep_in_ctx, 0); // Dword 0
            // Dword 1: EP Type=6 (Bulk IN), Max Packet Size, CErr=3
            let dword1 = (6u32 << 3) | ((ep_in_maxpkt as u32) << 16) | (3 << 1);
            core::ptr::write_volatile(ep_in_ctx.add(1), dword1);
            // Dwords 2-3: TR Dequeue Pointer with DCS=1
            let tr_phys = in_ring.phys_addr() | 1;
            core::ptr::write_volatile(ep_in_ctx.add(2), tr_phys as u32);
            core::ptr::write_volatile(ep_in_ctx.add(3), (tr_phys >> 32) as u32);
            // Dword 4: Average TRB Length
            core::ptr::write_volatile(ep_in_ctx.add(4), ep_in_maxpkt as u32);
        }

        // EP Context for bulk OUT (type 2)
        // SAFETY: `ep_out_dci` is `(addr & 0x0F) * 2`, at most 30, so context
        // index `ep_out_dci + 1` stays inside the 33-context allocation.
        let ep_out_ctx =
            unsafe { input_ctx.as_ptr().add((ep_out_dci as usize + 1) * ctx_size) as *mut u32 };
        // SAFETY: `ep_out_ctx` starts a `ctx_size`-byte context and `ctx_size`
        // is at least 32, so dwords 0 through 4 are inside it.
        unsafe {
            core::ptr::write_volatile(ep_out_ctx, 0); // Dword 0
            // Dword 1: EP Type=2 (Bulk OUT), Max Packet Size, CErr=3
            let dword1 = (2u32 << 3) | ((ep_out_maxpkt as u32) << 16) | (3 << 1);
            core::ptr::write_volatile(ep_out_ctx.add(1), dword1);
            // Dwords 2-3: TR Dequeue Pointer with DCS=1
            let tr_phys = out_ring.phys_addr() | 1;
            core::ptr::write_volatile(ep_out_ctx.add(2), tr_phys as u32);
            core::ptr::write_volatile(ep_out_ctx.add(3), (tr_phys >> 32) as u32);
            // Dword 4: Average TRB Length
            core::ptr::write_volatile(ep_out_ctx.add(4), ep_out_maxpkt as u32);
        }

        let input_ctx_phys = input_ctx.phys_addr().as_u64();
        self.submit_command(Trb::configure_endpoint(input_ctx_phys, device.slot_id))?;

        println!(
            "xhci: configured EP{} IN / EP{} OUT bulk, maxpkt IN={} OUT={}",
            ep_in_num, ep_out_num, ep_in_maxpkt, ep_out_maxpkt
        );

        Ok((in_ring, out_ring))
    }

    /// Perform a bulk transfer (IN or OUT) on a device endpoint.
    ///
    /// For OUT transfers the caller fills the DMA buffer before calling.
    /// For IN transfers the controller fills the DMA buffer on completion.
    /// Returns the number of bytes transferred.
    pub fn bulk_transfer(
        &mut self,
        slot_id: u8,
        ring: &mut TransferRing,
        ep_dci: u32,
        buf_phys: u64,
        length: u32,
        _direction_in: bool,
    ) -> Result<usize, XhciError> {
        let trb = Trb {
            parameter: buf_phys,
            status: length,
            control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
        };
        ring.push(trb);

        // Ring the doorbell for this slot/endpoint
        // SAFETY: `self.regs` owns the mapped BAR0, and `slot_id` came from an
        // Enable Slot completion, so it indexes a doorbell the controller
        // allocated. The TRB it announces was pushed to the ring above.
        unsafe {
            reg_write(self.regs.doorbell(slot_id), ep_dci);
        }

        // Poll for a Transfer Event completion
        for _ in 0..10_000_000u32 {
            if let Some(event) = self.event_ring.poll() {
                let erdp = self.event_ring.dequeue_phys();
                let intr = self.regs.interrupter(0);
                // SAFETY: `self.regs` owns the mapped BAR0 and interrupter 0 is
                // inside its runtime region. `erdp` is the event ring's own
                // dequeue address, so handing it back is what tells the
                // controller the slot is free again.
                unsafe {
                    reg_write(&mut (*intr).erdp_lo, (erdp as u32) | (1 << 3));
                    reg_write(&mut (*intr).erdp_hi, (erdp >> 32) as u32);
                }

                if event.trb_type() == TRB_TYPE_TRANSFER {
                    let event_slot = ((event.control >> 24) & 0xFF) as u8;
                    if event_slot != slot_id {
                        continue;
                    }
                    let comp_code = ((event.status >> 24) & 0xFF) as u8;
                    let residual = event.status & 0x00FF_FFFF;
                    if comp_code == COMP_SUCCESS || comp_code == COMP_SHORT_PACKET {
                        return Ok((length.saturating_sub(residual)) as usize);
                    }
                    println!(
                        "xhci: bulk_transfer error: comp_code={}, slot={}, ep_dci={}",
                        comp_code, slot_id, ep_dci
                    );
                    return Err(XhciError::TransferError(comp_code));
                }
                // Consume non-transfer events (port status changes, etc.)
            } else {
                // No event ready -- clear IMAN.IP and USBSTS.EINT so the controller
                // can deliver new events.
                // SAFETY: `self.regs` owns the mapped BAR0, so interrupter 0
                // and the operational registers are both inside it. IP and
                // EINT are write-1-to-clear, so writing the bit back is what
                // acknowledges the interrupt and lets the next one arrive.
                unsafe {
                    let intr = self.regs.interrupter(0);
                    let iman = reg_read(&(*intr).iman);
                    if iman & 1 != 0 {
                        reg_write(&mut (*intr).iman, iman | 1);
                    }
                    let sts = reg_read(&(*self.regs.op()).usbsts);
                    if sts & (1 << 3) != 0 {
                        reg_write(&mut (*self.regs.op()).usbsts, 1 << 3);
                    }
                }
            }
            core::hint::spin_loop();
        }

        Err(XhciError::CommandTimeout)
    }
}

/// Hand a freshly enumerated USB mass-storage partition to the filesystem.
/// Runs off the xHCI driver thread because registration triggers block reads
/// that only that thread can answer.
extern "C" fn usb_register_partition_thread(arg: *mut u8) -> ! {
    // SAFETY: the only spawner of this kthread passes a `Partition` leaked with
    // `Box::into_raw`, so `arg` is that box's pointer, owned by exactly this
    // thread and reclaimed here.
    let partition: Box<crate::fs::gpt::Partition> = unsafe { Box::from_raw(arg.cast()) };
    if let Err(e) = crate::fs::api::register_partition(*partition) {
        println!("xhci: failed to register USB partition: {:?}", e);
    }
    kthread_exit(0)
}

/// Main xHCI driver entry point, run as a kernel thread.
pub extern "C" fn xhci_driver_main() -> ! {
    let mut controller = match XhciController::find_and_init() {
        Some(c) => c,
        None => {
            println!("xhci: no controller found");
            loop {
                thread_park();
            }
        }
    };

    // keyboard_device holds the active HID keyboard device, its interrupt IN transfer ring,
    // and the endpoint's DCI (used for doorbell writes).
    let mut keyboard_device: Option<(UsbDevice, TransferRing, u32)> = None;
    // mouse_device holds the active pointing device, its interrupt IN transfer
    // ring, and the endpoint's DCI (used for doorbell writes).
    let mut mouse_device: Option<(UsbDevice, TransferRing, u32)> = None;
    // What its reports mean, from its own report descriptor. `None` means the
    // device only offered the boot layout.
    let mut mouse_fields: Option<hid::PointerReport> = None;
    // Its interrupt endpoint's max packet size, which is how long a report can
    // be; the boot layout's four bytes is a floor, not a fact about the device.
    let mut mouse_report_len: usize = 4;
    // mass_storage_device holds the first USB mass storage device and its bulk transfer rings.
    // Block I/O requests arrive via USB_BLOCK_MAILBOX and are executed here.
    let mut mass_storage_device: Option<(
        crate::drivers::usb::mass_storage::UsbMassStorage,
        TransferRing,
        TransferRing,
    )> = None;
    let mut pending_usb_partition: Option<(u64, usize)> = None; // (block_count, index)
    // Counter used to generate unique /dev/usbN names for mass storage devices.
    let mut usb_storage_count: usize = 0;

    // Scan all ports for already-connected devices.
    let max_ports = controller.regs.max_ports();
    for port in 1..=max_ports {
        // SAFETY: `port` runs from 1 to `max_ports`, so `port - 1` is a valid
        // zero-based index and `regs.port()` returns a pointer inside the
        // mapped BAR0.
        let portsc = unsafe { reg_read(&(*controller.regs.port(port - 1)).portsc) };
        let ccs = portsc & 1; // Current Connect Status
        if ccs != 0 {
            match controller.handle_port_status_change(port) {
                Ok(mut device) => {
                    // Read device descriptor
                    match controller.get_device_descriptor(&mut device) {
                        Ok(desc) => {
                            // Copy packed fields to locals before passing to println.
                            let vendor = desc.id_vendor;
                            let product = desc.id_product;
                            println!(
                                "xhci: device {:04x}:{:04x} class={} subclass={} protocol={}",
                                vendor,
                                product,
                                desc.b_device_class,
                                desc.b_device_sub_class,
                                desc.b_device_protocol
                            );
                            device.device_descriptor = Some(desc);
                        }
                        Err(e) => println!("xhci: get device descriptor failed: {:?}", e),
                    }

                    // Read config descriptor
                    match controller.get_config_descriptor(&mut device, 0) {
                        Ok(config_data) => {
                            if config_data.len() >= 9 {
                                let config_value = config_data[5]; // bConfigurationValue
                                if let Err(e) =
                                    controller.set_configuration(&mut device, config_value)
                                {
                                    println!("xhci: set configuration failed: {:?}", e);
                                }
                            }

                            device.config_data = Some(config_data);
                        }
                        Err(e) => println!("xhci: get config descriptor failed: {:?}", e),
                    }

                    // Check if this is a HID keyboard and set it up (only use the first one found).
                    if keyboard_device.is_none() {
                        let kbd_info = device
                            .config_data
                            .as_deref()
                            .and_then(|d| find_hid_interface(d, HID_PROTOCOL_KEYBOARD));

                        if let Some((iface, ep)) = kbd_info {
                            // Switch to boot protocol (0 = boot, 1 = report)
                            if let Err(e) = controller.set_hid_protocol(
                                &mut device,
                                iface.b_interface_number,
                                0,
                            ) {
                                println!("xhci: set boot protocol failed: {:?}", e);
                            }

                            // Copy packed fields before the mutable borrow
                            let ep_addr = ep.b_endpoint_address;
                            let ep_maxpkt = ep.w_max_packet_size;
                            let ep_interval = ep.b_interval;

                            // Compute DCI from the endpoint descriptor address field
                            let ep_num = ep_addr & 0x0F;
                            let ep_dir_in = ep_addr & 0x80 != 0;
                            let ep_dci = (ep_num * 2 + if ep_dir_in { 1 } else { 0 }) as u32;

                            // Configure the interrupt IN endpoint
                            match controller.configure_interrupt_endpoint(
                                &mut device,
                                ep_addr,
                                ep_maxpkt,
                                ep_interval,
                            ) {
                                Ok(ring) => {
                                    keyboard_device = Some((device, ring, ep_dci));
                                }
                                Err(e) => {
                                    println!("xhci: configure endpoint failed: {:?}", e);
                                }
                            }
                            continue;
                        }
                    }

                    // Check if this is a pointing device and set it up (only use
                    // the first one found).
                    if mouse_device.is_none() {
                        // Ask each HID interface what its reports mean, and take
                        // the first that describes a pointer. Binding on the
                        // descriptor rather than on a protocol code is what lets
                        // an absolute device in: a tablet declares no boot
                        // interface, so it has no protocol code to match on.
                        let candidates = device
                            .config_data
                            .as_deref()
                            .map(find_hid_interfaces)
                            .unwrap_or_default();

                        let mut mouse_info = None;
                        for (iface, ep, report_len) in candidates {
                            let parsed = controller
                                .get_report_descriptor(
                                    &mut device,
                                    iface.b_interface_number,
                                    report_len,
                                )
                                .ok()
                                .as_deref()
                                .and_then(hid::parse_pointer);
                            if let Some(fields) = parsed {
                                mouse_info = Some((iface, ep, Some(fields)));
                                break;
                            }
                        }

                        // A device whose descriptor will not parse, but which
                        // declares itself a boot mouse, is still a mouse: keep
                        // the fixed layout as the fallback rather than losing a
                        // device the driver used to handle.
                        let mouse_info = mouse_info.or_else(|| {
                            device
                                .config_data
                                .as_deref()
                                .and_then(|d| find_hid_interface(d, HID_PROTOCOL_MOUSE))
                                .map(|(iface, ep)| (iface, ep, None))
                        });

                        if let Some((iface, ep, fields)) = mouse_info {
                            // Boot protocol replaces the layout the report
                            // descriptor just described, so it is only asked for
                            // when the fixed layout is what will be decoded.
                            // An interface with no boot subclass has no protocol
                            // to set and stalls if asked.
                            let boot_capable = iface.b_interface_sub_class == 1;
                            let use_boot = fields.is_none();
                            if boot_capable {
                                let protocol = if use_boot { 0 } else { 1 };
                                if let Err(e) = controller.set_hid_protocol(
                                    &mut device,
                                    iface.b_interface_number,
                                    protocol,
                                ) {
                                    println!("xhci: set mouse protocol failed: {:?}", e);
                                }
                            }
                            match &fields {
                                Some(f) => println!(
                                    "xhci: pointer on interface {}, {} axes",
                                    iface.b_interface_number,
                                    if f.absolute() { "absolute" } else { "relative" }
                                ),
                                None => println!(
                                    "xhci: pointer on interface {}, boot protocol",
                                    iface.b_interface_number
                                ),
                            }
                            mouse_fields = fields;

                            // Copy packed fields before the mutable borrow
                            let ep_addr = ep.b_endpoint_address;
                            let ep_maxpkt = ep.w_max_packet_size;
                            let ep_interval = ep.b_interval;

                            // Compute DCI from the endpoint descriptor address field
                            let ep_num = ep_addr & 0x0F;
                            let ep_dir_in = ep_addr & 0x80 != 0;
                            let ep_dci = (ep_num * 2 + if ep_dir_in { 1 } else { 0 }) as u32;

                            // Configure the interrupt IN endpoint
                            match controller.configure_interrupt_endpoint(
                                &mut device,
                                ep_addr,
                                ep_maxpkt,
                                ep_interval,
                            ) {
                                Ok(ring) => {
                                    mouse_report_len = (ep_maxpkt as usize).clamp(4, 64);
                                    mouse_device = Some((device, ring, ep_dci));
                                }
                                Err(e) => {
                                    println!("xhci: configure mouse endpoint failed: {:?}", e);
                                }
                            }
                            continue;
                        }
                    }

                    // Check if this is a USB mass storage device (BOT/SCSI).
                    {
                        let msc_info = device.config_data.as_deref().and_then(find_mass_storage);

                        if let Some((_iface, ep_in, ep_out)) = msc_info {
                            println!("xhci: USB mass storage detected on slot {}", device.slot_id);

                            // Copy packed fields before mutable borrows.
                            let ep_in_addr = ep_in.b_endpoint_address;
                            let ep_in_maxpkt = ep_in.w_max_packet_size;
                            let ep_out_addr = ep_out.b_endpoint_address;
                            let ep_out_maxpkt = ep_out.w_max_packet_size;

                            match controller.configure_bulk_endpoints(
                                &mut device,
                                ep_in_addr,
                                ep_in_maxpkt,
                                ep_out_addr,
                                ep_out_maxpkt,
                            ) {
                                Ok((mut in_ring, mut out_ring)) => {
                                    let slot_id = device.slot_id;
                                    let mut msc =
                                        crate::drivers::usb::mass_storage::UsbMassStorage::new(
                                            slot_id,
                                            ep_in_addr,
                                            ep_out_addr,
                                        );

                                    // INQUIRY
                                    match msc.inquiry(&mut controller, &mut in_ring, &mut out_ring)
                                    {
                                        Ok(data) => {
                                            // Vendor: bytes 8-15, Product: bytes 16-31 (ASCII, space-padded)
                                            let vendor = core::str::from_utf8(&data[8..16])
                                                .unwrap_or("????????")
                                                .trim();
                                            let product = core::str::from_utf8(&data[16..32])
                                                .unwrap_or("????????????????")
                                                .trim();
                                            println!(
                                                "xhci: USB storage: vendor='{}' product='{}'",
                                                vendor, product
                                            );

                                            // TEST UNIT READY
                                            match msc.test_unit_ready(
                                                &mut controller,
                                                &mut in_ring,
                                                &mut out_ring,
                                            ) {
                                                Ok(ready) => {
                                                    println!(
                                                        "xhci: USB storage: unit ready={}",
                                                        ready
                                                    );
                                                }
                                                Err(e) => {
                                                    println!(
                                                        "xhci: USB storage: TEST UNIT READY failed: {:?}",
                                                        e
                                                    );
                                                }
                                            }

                                            // READ CAPACITY
                                            match msc.read_capacity(
                                                &mut controller,
                                                &mut in_ring,
                                                &mut out_ring,
                                            ) {
                                                Ok((last_lba, block_size)) => {
                                                    let block_count = last_lba as u64 + 1;
                                                    let size_mb = block_count * block_size as u64
                                                        / (1024 * 1024);
                                                    println!(
                                                        "xhci: USB storage: {} blocks x {} bytes = {} MiB",
                                                        block_count, block_size, size_mb
                                                    );
                                                    msc.block_size = block_size;
                                                    msc.block_count = block_count;

                                                    if mass_storage_device.is_none() {
                                                        USB_BLOCK_MAILBOX.call_once(|| {
                                                            Arc::new(Mailbox::with_capacity(4))
                                                        });
                                                        // Save info for deferred partition registration
                                                        // (done after port scan to avoid blocking during enumeration)
                                                        pending_usb_partition =
                                                            Some((block_count, usb_storage_count));
                                                    }

                                                    mass_storage_device =
                                                        Some((msc, in_ring, out_ring));
                                                    usb_storage_count += 1;
                                                }
                                                Err(e) => {
                                                    println!(
                                                        "xhci: USB storage: READ CAPACITY failed: {:?}",
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            println!("xhci: USB storage: INQUIRY failed: {:?}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    println!("xhci: configure bulk endpoints failed: {:?}", e);
                                }
                            }
                        }
                    }
                }
                Err(e) => println!("xhci: enumeration failed on port {}: {:?}", port, e),
            }
        }
    }

    println!("xhci: initial enumeration complete");

    // Register the USB partition from a separate kthread. Registration makes
    // the fs kthread scan the device for a partition table, and those reads are
    // answered by this thread's mailbox drain below -- doing it inline would
    // block the only thread that can complete them.
    if let Some((block_count, idx)) = pending_usb_partition {
        let device_id = 1000 + idx as u64;
        // Register this USB mass-storage device with the kernel-wide block-io
        // registry so fs/* can submit reads/writes via the AsyncBlockDevice
        // trait without knowing about the underlying mailbox transport.
        crate::drivers::usb::block_dev::register(device_id, block_count);
        let partition = Box::new(crate::fs::gpt::Partition {
            index: 0,
            starting_lba: 0,
            ending_lba: block_count.saturating_sub(1),
            size_sectors: block_count,
            partition_type: crate::fs::gpt::PartitionType::Fat32,
            name: alloc::format!("USB Storage {}", idx),
            filesystem: Some(crate::fs::gpt::FilesystemType::Fat32),
            device_id,
            unique_partition_guid: [0; 16],
        });
        queue_spawn_kthread_named_arg(
            "usb-register",
            usb_register_partition_thread as *const () as u64,
            Box::into_raw(partition) as *mut u8,
        );
    }

    // Allocate DMA buffers for HID reports and pre-fill the interrupt rings.
    // Keyboard: 8-byte boot report; mouse: 4-byte boot report (3 bytes + wheel).
    let kbd_report_buf = dma()
        .allocate_sized(8)
        .expect("xhci: failed to allocate keyboard HID report buf");
    let kbd_report_phys = kbd_report_buf.phys_addr().as_u64();
    let mut prev_kbd_report = [0u8; 8];

    // Software key repeat state for USB HID keyboard.
    // USB keyboards only report state changes, so we must generate repeat events.
    let mut repeat_key: Option<pc_keyboard::KeyCode> = None;
    let mut repeat_next_us: u64 = 0; // next repeat event time (uptime_us)
    const REPEAT_DELAY_US: u64 = 500_000; // 500ms initial delay
    const REPEAT_INTERVAL_US: u64 = 33_333; // ~30 Hz repeat rate

    // How many reports an interrupt endpoint may have in flight.
    //
    // **One is not enough, and a relative mouse is what shows it.** With a
    // single TRB queued the controller has nowhere to put a report until the
    // driver has been woken, has read the last one and has re-armed; anything
    // the device produces in that window waits for the next service interval.
    // A relative device sends a report per delta and QEMU emits hundreds of
    // them to walk a pointer across the screen, so the guest falls behind a
    // queue it drains one interval at a time and the pointer arrives late and
    // in bursts. With a ring of buffers the controller can deliver back to back
    // and the driver drains whatever accumulated in one wake.
    //
    // A physical mouse is a relative device too; `usb-tablet` is a convenience
    // that only exists in a VM. This is the path real hardware takes.
    const HID_QUEUE_DEPTH: usize = 8;

    let mouse_report_buf = dma()
        .allocate_sized(mouse_report_len * HID_QUEUE_DEPTH)
        .expect("xhci: failed to allocate mouse HID report buf");
    let mouse_report_phys = mouse_report_buf.phys_addr().as_u64();
    // Which slot the next completion refers to. An interrupt endpoint has one
    // ring and completes in the order it was filled, so a rotating index is
    // enough to say which buffer the report landed in.
    let mut mouse_slot: usize = 0;
    let mut kbd_slot: usize = 0;

    if let Some((ref mut dev, ref mut ring, ep_dci)) = keyboard_device {
        for i in 0..HID_QUEUE_DEPTH {
            ring.push(Trb {
                parameter: kbd_report_phys + (i * 8) as u64,
                status: 8,
                control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
            });
        }
        // SAFETY: `controller.regs` owns the mapped BAR0, and `dev.slot_id`
        // came from an Enable Slot completion, so it indexes a doorbell the
        // controller allocated. Every TRB it announces is already on the ring.
        unsafe { reg_write(controller.regs.doorbell(dev.slot_id), ep_dci) };
        crate::drivers::keyboard::USB_KEYBOARD_ACTIVE
            .store(true, core::sync::atomic::Ordering::Relaxed);
    }

    // Which report slot each queued TRB will fill. A transfer event carries the
    // address of the TRB it completed, so the buffer to read is the one that
    // TRB pointed at — asked of the event rather than counted, because a count
    // that ever slips reads a slot the controller has not refilled and the last
    // report in it is applied a second time.
    let mut mouse_trb = [0u64; HID_QUEUE_DEPTH];
    if let Some((ref mut dev, ref mut ring, ep_dci)) = mouse_device {
        for (i, trb_phys) in mouse_trb.iter_mut().enumerate() {
            *trb_phys = ring.push(Trb {
                parameter: mouse_report_phys + (i * mouse_report_len) as u64,
                status: mouse_report_len as u32,
                control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
            });
        }
        // SAFETY: `controller.regs` owns the mapped BAR0, and `dev.slot_id`
        // came from an Enable Slot completion, so it indexes a doorbell the
        // controller allocated. Every TRB it announces is already on the ring.
        unsafe { reg_write(controller.regs.doorbell(dev.slot_id), ep_dci) };
        crate::drivers::mouse::USB_MOUSE_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);
    }

    // Reusable DMA buffer for block I/O. Grown as needed, never freed.
    // Avoids allocating and leaking a DMA buffer per request.
    let mut io_buf: Option<DmaBuffer> = None;

    // Main event loop: handle runtime events (hot-plug, transfer completions, etc.)
    //
    // Two wake sources:
    // 1. MSI-X interrupt -> wake_thread_irq from interrupt handler (HID events)
    // 2. USB block I/O -> wake_thread from block_dev after mailbox send
    loop {
        // Use thread_park_while so we only park if there's truly nothing to do.
        // This avoids lost wakes when a mailbox request arrives between the
        // mailbox check and the park call.
        let er = &mut controller.event_ring as *mut EventRing;
        if repeat_key.is_some() {
            // Key held: sleep until next repeat or wake on interrupt/mailbox.
            let now = crate::timer::uptime_us();
            if now < repeat_next_us {
                let wait_us = repeat_next_us - now;
                thread_sleep(Duration::from_micros(wait_us));
            }
        } else {
            // No key held: park indefinitely until interrupt or mailbox.
            thread_park_while(|| {
                // SAFETY: `er` is a raw pointer to `controller.event_ring`,
                // taken so the closure does not hold a borrow of `controller`
                // across the park. `controller` is a live local of this
                // function, and this driver thread is the only one that touches
                // its event ring, so the pointer stays valid and unaliased.
                let has_event = unsafe { (*er).peek() };
                let has_mailbox = USB_BLOCK_MAILBOX.get().is_some_and(|mb| !mb.is_empty());
                !has_event && !has_mailbox
            });
        }

        // Process all pending events.
        while let Some(event) = controller.event_ring.poll() {
            let erdp = controller.event_ring.dequeue_phys();
            let intr = controller.regs.interrupter(0);
            // SAFETY: `controller.regs` owns the mapped BAR0 and interrupter 0
            // is inside its runtime region. `erdp` is the event ring's own
            // dequeue address, so handing it back frees the slot.
            unsafe {
                reg_write(&mut (*intr).erdp_lo, (erdp as u32) | (1 << 3));
                reg_write(&mut (*intr).erdp_hi, (erdp >> 32) as u32);
            }

            match event.trb_type() {
                TRB_TYPE_TRANSFER => {
                    let comp_code = ((event.status >> 24) & 0xFF) as u8;
                    // Slot ID is in bits [31:24] of the event control field.
                    let event_slot_id = ((event.control >> 24) & 0xFF) as u8;

                    if comp_code == COMP_SUCCESS || comp_code == COMP_SHORT_PACKET {
                        // Determine which device this event belongs to by slot ID.
                        let is_keyboard = keyboard_device
                            .as_ref()
                            .is_some_and(|(dev, _, _)| dev.slot_id == event_slot_id);
                        let is_mouse = mouse_device
                            .as_ref()
                            .is_some_and(|(dev, _, _)| dev.slot_id == event_slot_id);

                        if is_keyboard {
                            // Read the keyboard report out of the slot this
                            // completion refers to, as far as the controller
                            // wrote it. The rest of the slot is the last report
                            // that landed there, and keycodes read out of it are
                            // keys nobody pressed.
                            let mut report = [0u8; 8];
                            let residual = (event.status & 0x00FF_FFFF) as usize;
                            let kbd_len = report.len().saturating_sub(residual);
                            // SAFETY: `kbd_report_buf` is a DMA allocation of
                            // `HID_QUEUE_DEPTH` eight-byte slots and `kbd_slot`
                            // is kept below that depth, so the source range is
                            // in bounds; `kbd_len` is at most `report.len()`,
                            // which bounds the destination.
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    kbd_report_buf.as_ptr().add(kbd_slot * 8),
                                    report.as_mut_ptr(),
                                    kbd_len,
                                );
                            }

                            let key_events =
                                hid::process_boot_keyboard_report(&prev_kbd_report, &report);
                            crate::drivers::keyboard::dispatch_key_events(key_events.as_slice());
                            prev_kbd_report = report;

                            // Update key repeat state from the current report.
                            // Find the last non-zero key in the report (most recently pressed).
                            let held_key = report[2..8]
                                .iter()
                                .rev()
                                .find(|&&k| k != 0)
                                .and_then(|&k| hid::usb_hid_to_keycode(k));
                            match held_key {
                                Some(key) => {
                                    if repeat_key != Some(key) {
                                        // New key pressed: start initial delay
                                        repeat_key = Some(key);
                                        repeat_next_us =
                                            crate::timer::uptime_us() + REPEAT_DELAY_US;
                                    }
                                    // Same key still held: keep existing repeat timing
                                }
                                None => {
                                    // All keys released
                                    repeat_key = None;
                                }
                            }

                            // Hand the slot back and take the next one. The ring
                            // stays full, so the controller never has to wait
                            // for this thread between reports.
                            if let Some((ref mut dev, ref mut ring, ep_dci)) = keyboard_device {
                                ring.push(Trb {
                                    parameter: kbd_report_phys + (kbd_slot * 8) as u64,
                                    status: 8,
                                    control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
                                });
                                // SAFETY: the mapped BAR0's doorbell for a slot
                                // the controller allocated, announcing the TRB
                                // just pushed above.
                                unsafe { reg_write(controller.regs.doorbell(dev.slot_id), ep_dci) };
                            }
                            kbd_slot = (kbd_slot + 1) % HID_QUEUE_DEPTH;
                        } else if is_mouse {
                            // Read the report from the DMA buffer. Its length is
                            // the endpoint's, not the boot layout's: an absolute
                            // device reports six bytes and a boot mouse four.
                            //
                            // How much of it the controller actually wrote is
                            // the event's to say. A slot holds the last report
                            // that landed in it, so bytes past this transfer's
                            // are the previous report's, and a displacement read
                            // out of them is motion the device never reported --
                            // applied again on every short completion, which is
                            // a pointer that drifts on its own.
                            // Which slot to read is the event's to say too: it
                            // carries the address of the TRB it completed, and
                            // that TRB pointed at the slot the controller wrote.
                            // Counting events instead assumes they arrive one
                            // per queued TRB in order, and a count that slips
                            // reads a slot nothing refilled — applying the last
                            // report to land there for a second time.
                            if let Some(slot) =
                                mouse_trb.iter().position(|&trb| trb == event.parameter)
                            {
                                if slot != mouse_slot {
                                    hid::MOUSE_SLOT_SLIPS
                                        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                                }
                                mouse_slot = slot;
                            }

                            let mut report = [0u8; 64];
                            let residual = (event.status & 0x00FF_FFFF) as usize;
                            if residual != 0 {
                                hid::MOUSE_SHORT_REPORTS
                                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                            }
                            let len = mouse_report_len.saturating_sub(residual).min(report.len());
                            // SAFETY: `mouse_report_buf` is a DMA allocation of
                            // `HID_QUEUE_DEPTH` slots of `mouse_report_len`
                            // bytes and `mouse_slot` is kept below that depth,
                            // so the source range is in bounds; `len` is
                            // clamped to `report.len()`, which bounds the
                            // destination.
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    mouse_report_buf.as_ptr().add(mouse_slot * mouse_report_len),
                                    report.as_mut_ptr(),
                                    len,
                                );
                            }

                            match mouse_fields {
                                Some(ref fields) => {
                                    hid::process_pointer_report(fields, &report[..len]);
                                }
                                None => {
                                    hid::process_boot_mouse_report(&report[..len], len.min(4));
                                }
                            }

                            // Hand the slot back and take the next one, keeping
                            // the address of the TRB that now carries it.
                            if let Some((ref mut dev, ref mut ring, ep_dci)) = mouse_device {
                                mouse_trb[mouse_slot] = ring.push(Trb {
                                    parameter: mouse_report_phys
                                        + (mouse_slot * mouse_report_len) as u64,
                                    status: mouse_report_len as u32,
                                    control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
                                });
                                // SAFETY: the mapped BAR0's doorbell for a slot
                                // the controller allocated, announcing the TRB
                                // just pushed above.
                                unsafe { reg_write(controller.regs.doorbell(dev.slot_id), ep_dci) };
                            }
                            mouse_slot = (mouse_slot + 1) % HID_QUEUE_DEPTH;
                        } else {
                            println!("xhci: transfer event from unknown slot {}", event_slot_id);
                        }
                    } else {
                        println!("xhci: transfer error, completion code={}", comp_code);
                    }
                }
                TRB_TYPE_PORT_STATUS_CHANGE => {
                    let port_id = ((event.parameter >> 24) & 0xFF) as u8;
                    println!("xhci: port {} status change event", port_id);
                }
                _ => {
                    println!("xhci: unhandled event type {}", event.trb_type());
                }
            }
        }

        // Generate key repeat event if a key is held and the repeat timer has fired.
        if let Some(key) = repeat_key {
            let now = crate::timer::uptime_us();
            if now >= repeat_next_us {
                let event = pc_keyboard::KeyEvent::new(key, pc_keyboard::KeyState::Down);
                crate::drivers::keyboard::dispatch_key_events(&[event]);
                repeat_next_us = now + REPEAT_INTERVAL_US;
            }
        }

        // Clear interrupt bits so the controller can deliver new MSI-X interrupts:
        // - IMAN.IP (bit 0, W1C) on Interrupter 0
        // - USBSTS.EINT (bit 3, W1C) on the controller
        // SAFETY: `controller.regs` owns the mapped BAR0, so interrupter 0 and
        // the operational registers are both inside it. IP and EINT are
        // write-1-to-clear, so writing the bit back acknowledges the interrupt.
        unsafe {
            let intr = controller.regs.interrupter(0);
            let iman = reg_read(&(*intr).iman);
            if iman & 1 != 0 {
                reg_write(&mut (*intr).iman, iman | 1);
            }
            let sts = reg_read(&(*controller.regs.op()).usbsts);
            if sts & (1 << 3) != 0 {
                reg_write(&mut (*controller.regs.op()).usbsts, 1 << 3);
            }
        }

        // Process pending USB block I/O requests from FS threads.
        if let Some(mailbox) = USB_BLOCK_MAILBOX.get() {
            while let Some(mut req) = mailbox.try_recv() {
                match req.payload.take().unwrap() {
                    UsbBlockRequest::Read { lba, sectors } => {
                        let result = if let Some((ref mut msc, ref mut in_ring, ref mut out_ring)) =
                            mass_storage_device
                        {
                            let byte_count = sectors as usize * msc.block_size as usize;
                            // Reallocate the shared I/O buffer only when it's too small.
                            if io_buf.is_none() || io_buf.as_ref().unwrap().size < byte_count {
                                io_buf = dma().allocate_sized(byte_count).ok();
                            }
                            match io_buf {
                                Some(ref buf) => {
                                    let phys = buf.phys_addr().as_u64();
                                    match msc.read_sectors(
                                        &mut controller,
                                        in_ring,
                                        out_ring,
                                        lba as u32,
                                        sectors,
                                        phys,
                                    ) {
                                        Ok(()) => {
                                            let mut out = alloc::vec![0u8; byte_count];
                                            // SAFETY: `buf` is a DMA
                                            // allocation of at least
                                            // `byte_count` bytes (grown just
                                            // above) and `out` is exactly that
                                            // long, so both sides are in
                                            // bounds and distinct.
                                            unsafe {
                                                core::ptr::copy_nonoverlapping(
                                                    buf.as_ptr(),
                                                    out.as_mut_ptr(),
                                                    byte_count,
                                                );
                                            }
                                            Ok(out)
                                        }
                                        Err(e) => Err(e),
                                    }
                                }
                                None => Err(XhciError::InvalidDevice),
                            }
                        } else {
                            Err(XhciError::InvalidDevice)
                        };
                        req.reply(UsbBlockResponse::ReadResult(result));
                    }
                    UsbBlockRequest::Write { lba, sectors, data } => {
                        let result = if let Some((ref mut msc, ref mut in_ring, ref mut out_ring)) =
                            mass_storage_device
                        {
                            let byte_count = sectors as usize * msc.block_size as usize;
                            if io_buf.is_none() || io_buf.as_ref().unwrap().size < byte_count {
                                io_buf = dma().allocate_sized(byte_count).ok();
                            }
                            match io_buf {
                                Some(ref buf) => {
                                    let copy_len = byte_count.min(data.len());
                                    // SAFETY: `copy_len` is bounded by both
                                    // `data.len()` and `byte_count`, and `buf`
                                    // is a DMA allocation of at least
                                    // `byte_count` bytes, so both sides are in
                                    // bounds and distinct.
                                    unsafe {
                                        core::ptr::copy_nonoverlapping(
                                            data.as_ptr(),
                                            buf.as_ptr(),
                                            copy_len,
                                        );
                                    }
                                    let phys = buf.phys_addr().as_u64();
                                    match msc.write_sectors(
                                        &mut controller,
                                        in_ring,
                                        out_ring,
                                        lba as u32,
                                        sectors,
                                        phys,
                                    ) {
                                        Ok(()) => Ok(data),
                                        Err(e) => Err(e),
                                    }
                                }
                                None => Err(XhciError::InvalidDevice),
                            }
                        } else {
                            Err(XhciError::InvalidDevice)
                        };
                        req.reply(UsbBlockResponse::WriteResult(result));
                    }
                }
            }
        }
    }
}

/// Iterator over descriptors in a USB configuration descriptor blob.
///
/// Each call to `next` returns `(descriptor_type, descriptor_bytes)` for one descriptor.
struct DescriptorIter<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> DescriptorIter<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }
}

impl<'a> Iterator for DescriptorIter<'a> {
    type Item = (u8, &'a [u8]); // (descriptor_type, descriptor_bytes)

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset + 2 > self.data.len() {
            return None;
        }
        let length = self.data[self.offset] as usize;
        let desc_type = self.data[self.offset + 1];
        if length == 0 || self.offset + length > self.data.len() {
            return None;
        }
        let bytes = &self.data[self.offset..self.offset + length];
        self.offset += length;
        Some((desc_type, bytes))
    }
}

/// Every HID interface in a configuration that has an interrupt IN endpoint,
/// with the length of the report descriptor its HID descriptor advertises.
///
/// Unlike `find_hid_interface` this matches on the class alone. The interface
/// protocol code only means anything on an interface that declares the boot
/// subclass, so matching on it is what makes every other HID device -- a
/// tablet, a mouse with more than three buttons -- invisible.
fn find_hid_interfaces(config_data: &[u8]) -> Vec<(InterfaceDescriptor, EndpointDescriptor, u16)> {
    /// HID class descriptor, which carries the report descriptor's length.
    const DESC_HID: u8 = 0x21;

    let mut found = Vec::new();
    let mut current_iface: Option<InterfaceDescriptor> = None;
    let mut report_len: u16 = 0;

    for (desc_type, bytes) in DescriptorIter::new(config_data) {
        if desc_type == DESC_INTERFACE && bytes.len() >= 9 {
            // SAFETY: `InterfaceDescriptor` is the 9-byte wire layout (USB 2.0
            // §9.6.5) and the `bytes.len() >= 9` guard above bounds the read.
            // Unaligned because the type is `packed` and the source is a `[u8]`
            // at an arbitrary offset into the configuration blob.
            let iface =
                unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const InterfaceDescriptor) };
            current_iface = (iface.b_interface_class == USB_CLASS_HID).then_some(iface);
            report_len = 0;
        } else if desc_type == DESC_HID && bytes.len() >= 9 && current_iface.is_some() {
            // bLength, bDescriptorType, bcdHID(2), bCountryCode, bNumDescriptors,
            // then per subordinate descriptor: bDescriptorType, wDescriptorLength.
            if bytes[6] == 0x22 {
                report_len = u16::from_le_bytes([bytes[7], bytes[8]]);
            }
        } else if desc_type == DESC_ENDPOINT
            && bytes.len() >= 7
            && let Some(iface) = current_iface
        {
            // SAFETY: `EndpointDescriptor` is the 7-byte wire layout (USB 2.0
            // §9.6.6) and the `bytes.len() >= 7` guard above bounds the read.
            // Unaligned for the same reason as the interface descriptor.
            let ep =
                unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const EndpointDescriptor) };
            if ep.b_endpoint_address & 0x80 != 0 && ep.bm_attributes & 0x03 == 3 {
                found.push((iface, ep, report_len));
                current_iface = None;
            }
        }
    }
    found
}

/// Search a configuration descriptor blob for a HID interface with the given boot protocol
/// and its interrupt IN endpoint.
///
/// `protocol` should be `HID_PROTOCOL_KEYBOARD` or `HID_PROTOCOL_MOUSE`.
///
/// Returns `(InterfaceDescriptor, EndpointDescriptor)` for the first matching pair,
/// or `None` if no matching interface is found.
fn find_hid_interface(
    config_data: &[u8],
    protocol: u8,
) -> Option<(InterfaceDescriptor, EndpointDescriptor)> {
    let mut current_iface: Option<InterfaceDescriptor> = None;

    for (desc_type, bytes) in DescriptorIter::new(config_data) {
        if desc_type == DESC_INTERFACE && bytes.len() >= 9 {
            // SAFETY: `InterfaceDescriptor` is the 9-byte wire layout (USB 2.0
            // §9.6.5) and the `bytes.len() >= 9` guard above bounds the read.
            // Unaligned because the type is `packed` and the source is a `[u8]`
            // at an arbitrary offset into the configuration blob.
            let iface =
                unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const InterfaceDescriptor) };
            if iface.b_interface_class == USB_CLASS_HID && iface.b_interface_protocol == protocol {
                current_iface = Some(iface);
            } else {
                current_iface = None;
            }
        } else if desc_type == DESC_ENDPOINT
            && bytes.len() >= 7
            && let Some(iface) = current_iface
        {
            // SAFETY: `EndpointDescriptor` is the 7-byte wire layout (USB 2.0
            // §9.6.6) and the `bytes.len() >= 7` guard above bounds the read.
            // Unaligned for the same reason as the interface descriptor.
            let ep =
                unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const EndpointDescriptor) };
            // Accept only IN interrupt endpoints
            if ep.b_endpoint_address & 0x80 != 0 && ep.bm_attributes & 0x03 == 3 {
                return Some((iface, ep));
            }
        }
    }
    None
}

/// Search a configuration descriptor blob for a USB Mass Storage interface
/// (class=0x08, subclass=0x06 SCSI, protocol=0x50 BOT) and its bulk IN and OUT endpoints.
///
/// Returns `(InterfaceDescriptor, ep_in, ep_out)` for the first matching interface,
/// or `None` if no mass storage interface is found.
fn find_mass_storage(
    config_data: &[u8],
) -> Option<(InterfaceDescriptor, EndpointDescriptor, EndpointDescriptor)> {
    const USB_SUBCLASS_SCSI: u8 = 0x06;
    const USB_PROTOCOL_BOT: u8 = 0x50;

    let mut current_iface: Option<InterfaceDescriptor> = None;
    let mut ep_in: Option<EndpointDescriptor> = None;
    let mut ep_out: Option<EndpointDescriptor> = None;

    for (desc_type, bytes) in DescriptorIter::new(config_data) {
        if desc_type == DESC_INTERFACE && bytes.len() >= 9 {
            // Starting a new interface; reset endpoint state.
            // SAFETY: `InterfaceDescriptor` is the 9-byte wire layout (USB 2.0
            // §9.6.5) and the `bytes.len() >= 9` guard above bounds the read.
            // Unaligned because the type is `packed` and the source is a `[u8]`
            // at an arbitrary offset into the configuration blob.
            let iface =
                unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const InterfaceDescriptor) };
            if iface.b_interface_class == USB_CLASS_MASS_STORAGE
                && iface.b_interface_sub_class == USB_SUBCLASS_SCSI
                && iface.b_interface_protocol == USB_PROTOCOL_BOT
            {
                current_iface = Some(iface);
                ep_in = None;
                ep_out = None;
            } else {
                current_iface = None;
                ep_in = None;
                ep_out = None;
            }
        } else if desc_type == DESC_ENDPOINT && bytes.len() >= 7 && current_iface.is_some() {
            // SAFETY: `EndpointDescriptor` is the 7-byte wire layout (USB 2.0
            // §9.6.6) and the `bytes.len() >= 7` guard above bounds the read.
            // Unaligned for the same reason as the interface descriptor.
            let ep =
                unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const EndpointDescriptor) };
            // Only accept bulk endpoints (bmAttributes bits [1:0] == 2)
            if ep.bm_attributes & 0x03 == 2 {
                if ep.b_endpoint_address & 0x80 != 0 {
                    ep_in = Some(ep);
                } else {
                    ep_out = Some(ep);
                }
            }

            // Return as soon as we have both endpoints for this interface.
            if let (Some(iface), Some(ein), Some(eout)) = (current_iface, ep_in, ep_out) {
                return Some((iface, ein, eout));
            }
        }
    }
    None
}
