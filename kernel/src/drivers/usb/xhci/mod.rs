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
    interrupts::{InterruptIndex, io::XHCI_DRIVER_THREAD_ID},
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
    println,
    thread::scheduler::sched,
};

use self::{
    device::{
        ConfigDescriptor, DESC_CONFIGURATION, DESC_DEVICE, DESC_ENDPOINT, DESC_INTERFACE,
        DeviceDescriptor, EndpointDescriptor, HID_PROTOCOL_KEYBOARD, HID_PROTOCOL_MOUSE,
        InterfaceDescriptor, SetupPacket, USB_CLASS_HID, UsbDevice, UsbSpeed,
    },
    registers::{XhciRegisters, reg_read, reg_write},
    rings::{
        COMP_SHORT_PACKET, COMP_SUCCESS, CommandRing, EventRing, TRB_DIR_IN, TRB_IDT, TRB_IOC,
        TRB_TYPE_COMMAND_COMPLETION, TRB_TYPE_DATA_STAGE, TRB_TYPE_NORMAL,
        TRB_TYPE_PORT_STATUS_CHANGE, TRB_TYPE_SETUP_STAGE, TRB_TYPE_STATUS_STAGE,
        TRB_TYPE_TRANSFER, TransferRing, Trb,
    },
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

    /// Submit a command TRB and wait (by polling) for the matching Command Completion Event.
    ///
    /// All commands and event ring polling happen in the driver thread; there are no cross-thread
    /// races to worry about here.
    pub fn submit_command(&mut self, trb: Trb) -> Result<Trb, XhciError> {
        let cmd_phys = self.command_ring.as_mut().unwrap().push(trb);

        // Ring doorbell 0 — Host Controller Command doorbell.
        unsafe {
            reg_write(self.regs.doorbell(0), 0);
        }

        // Poll the event ring until we see the Command Completion Event whose parameter
        // field contains the physical address of the command TRB we just submitted.
        for _ in 0..5_000_000u32 {
            if let Some(event) = self.event_ring.as_mut().unwrap().poll() {
                // Acknowledge the event by advancing the ERDP and clearing EHB (bit 3).
                let erdp = self.event_ring.as_ref().unwrap().dequeue_phys();
                let intr = self.regs.interrupter(0);
                unsafe {
                    reg_write(&mut (*intr).erdp_lo, (erdp as u32) | (1 << 3));
                    reg_write(&mut (*intr).erdp_hi, (erdp >> 32) as u32);
                }

                if event.trb_type() == TRB_TYPE_COMMAND_COMPLETION {
                    if event.parameter == cmd_phys {
                        let comp_code = ((event.status >> 24) & 0xFF) as u8;
                        if comp_code == COMP_SUCCESS {
                            return Ok(event);
                        } else {
                            return Err(XhciError::TransferError(comp_code));
                        }
                    }
                }

                // Log port status changes that arrive while we wait for the command to complete.
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
        if data_len > 0 {
            if let Some(buf_phys) = data_buf_phys {
                let dir_bit = if direction_in { TRB_DIR_IN } else { 0 };
                let data_trb = Trb {
                    parameter: buf_phys,
                    status: data_len as u32,
                    control: ((TRB_TYPE_DATA_STAGE as u32) << 10) | dir_bit,
                };
                ring.push(data_trb);
            }
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
        unsafe {
            reg_write(self.regs.doorbell(device.slot_id), 1);
        }

        // Poll for the Transfer Event that corresponds to our Status Stage TRB.
        for _ in 0..5_000_000u32 {
            if let Some(event) = self.event_ring.as_mut().unwrap().poll() {
                let erdp = self.event_ring.as_ref().unwrap().dequeue_phys();
                let intr = self.regs.interrupter(0);
                unsafe {
                    reg_write(&mut (*intr).erdp_lo, (erdp as u32) | (1 << 3));
                    reg_write(&mut (*intr).erdp_hi, (erdp >> 32) as u32);
                }

                if event.trb_type() == TRB_TYPE_TRANSFER {
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
            unsafe {
                let sc = reg_read(&(*port).portsc);
                reg_write(&mut (*port).portsc, (sc & pp_bit) | (1 << 4)); // PR – Port Reset
            }

            for _ in 0..1_000_000u32 {
                let sc = unsafe { reg_read(&(*port).portsc) };
                if sc & (1 << 21) != 0 {
                    // Clear PRC (Port Reset Change) by writing 1 to it.
                    unsafe { reg_write(&mut (*port).portsc, (sc & pp_bit) | (1 << 21)) };
                    break;
                }
                core::hint::spin_loop();
            }

            // Re-read PORTSC after the reset completes to get the updated speed and PED.
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
        println!("xhci: assigned slot {}", slot_id);

        // Step 2 — Allocate the Input Context.
        // Layout: InputControlContext (1 × context_size) + SlotContext (1 × context_size)
        //         + 31 Endpoint Contexts = 33 × context_size bytes total.
        let ctx_size = self.context_size;
        let input_ctx =
            DmaBuffer::allocate_sized(33 * ctx_size).map_err(|_| XhciError::InvalidDevice)?;

        // Step 3 — Allocate EP0 Transfer Ring (64 TRBs is ample for control transfers).
        let ep0_ring = TransferRing::new(64);

        // Step 4 — Fill Input Control Context.
        // Offset 0 = Drop Context Flags, offset 4 = Add Context Flags.
        // We add Slot (bit 0) and EP0 (bit 1) → Add Flags = 0b11 = 0x3.
        let icc_ptr = input_ctx.as_ptr() as *mut u32;
        unsafe {
            core::ptr::write_volatile(icc_ptr, 0); // Drop Context Flags = 0
            core::ptr::write_volatile(icc_ptr.add(1), 0x3); // Add Context Flags: Slot + EP0
        }

        // Step 5 — Fill Slot Context (at offset 1 × context_size).
        let slot_ctx_ptr = unsafe { input_ctx.as_ptr().add(ctx_size) as *mut u32 };
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
        let ep0_ctx_ptr = unsafe { input_ctx.as_ptr().add(2 * ctx_size) as *mut u32 };
        let max_packet = speed.default_max_packet_size();
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
        let output_ctx =
            DmaBuffer::allocate_sized(32 * ctx_size).map_err(|_| XhciError::InvalidDevice)?;
        let output_ctx_phys = output_ctx.phys_addr().as_u64();

        if let Some(ref dcbaa) = self.dcbaa {
            unsafe {
                let entry = (dcbaa.as_ptr() as *mut u64).add(slot_id as usize);
                core::ptr::write_volatile(entry, output_ctx_phys);
            }
        }

        // The Output Device Context must remain allocated as long as the device is active.
        // We intentionally leak it here; a future device management layer will track it.
        core::mem::forget(output_ctx);

        // Step 8 — Address Device command: assigns the USB address and transitions the
        // device to the Addressed state.
        let input_ctx_phys = input_ctx.phys_addr().as_u64();
        self.submit_command(Trb::address_device(input_ctx_phys, slot_id, false))
            .map_err(|e| {
                println!("xhci: address device failed: {:?}", e);
                XhciError::InvalidDevice
            })?;

        println!("xhci: device addressed on slot {}", slot_id);

        Ok(UsbDevice {
            slot_id,
            speed,
            port_id,
            ep0_ring,
            input_ctx,
            device_descriptor: None,
            config_data: None,
        })
    }

    /// Read the device descriptor from a USB device.
    pub fn get_device_descriptor(
        &mut self,
        device: &mut UsbDevice,
    ) -> Result<DeviceDescriptor, XhciError> {
        let buf = DmaBuffer::allocate_sized(18).map_err(|_| XhciError::InvalidDevice)?;
        let buf_phys = buf.phys_addr().as_u64();

        let setup = SetupPacket {
            bm_request_type: 0x80,              // Device-to-host, Standard, Device
            b_request: 6,                       // GET_DESCRIPTOR
            w_value: (DESC_DEVICE as u16) << 8, // Descriptor type = Device, index = 0
            w_index: 0,
            w_length: 18,
        };

        let transferred = self.control_transfer(device, setup, Some(buf_phys), 18, true)?;

        if transferred < 18 {
            // Try with smaller initial request (some low-speed devices need this)
            let setup8 = SetupPacket {
                w_length: 8,
                ..setup
            };
            self.control_transfer(device, setup8, Some(buf_phys), 8, true)?;
        }

        let desc = unsafe { core::ptr::read(buf.as_ptr() as *const DeviceDescriptor) };
        Ok(desc)
    }

    /// Read the full configuration descriptor (with all interfaces and endpoints).
    ///
    /// First reads 9 bytes to get wTotalLength, then reads the full blob.
    pub fn get_config_descriptor(
        &mut self,
        device: &mut UsbDevice,
        config_index: u8,
    ) -> Result<Vec<u8>, XhciError> {
        // Phase 1: Read just the config descriptor header (9 bytes) to get wTotalLength.
        let buf = DmaBuffer::allocate_sized(256).map_err(|_| XhciError::InvalidDevice)?;
        let buf_phys = buf.phys_addr().as_u64();

        let setup = SetupPacket {
            bm_request_type: 0x80,
            b_request: 6, // GET_DESCRIPTOR
            w_value: ((DESC_CONFIGURATION as u16) << 8) | (config_index as u16),
            w_index: 0,
            w_length: 9,
        };

        self.control_transfer(device, setup, Some(buf_phys), 9, true)?;

        let config_hdr = unsafe { core::ptr::read(buf.as_ptr() as *const ConfigDescriptor) };
        let total_len = config_hdr.w_total_length;

        if total_len <= 9 || total_len > 256 {
            // Just return what we have
            let mut data = alloc::vec![0u8; total_len as usize];
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr(), data.as_mut_ptr(), total_len as usize);
            }
            return Ok(data);
        }

        // Phase 2: Read the full descriptor.
        let setup_full = SetupPacket {
            w_length: total_len,
            ..setup
        };

        self.control_transfer(device, setup_full, Some(buf_phys), total_len, true)?;

        let mut data = alloc::vec![0u8; total_len as usize];
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), data.as_mut_ptr(), total_len as usize);
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

        let ctx_size = self.context_size;
        let ring = TransferRing::new(64);

        let input_ctx = &device.input_ctx;

        // Clear and set Input Control Context
        let icc = input_ctx.as_ptr() as *mut u32;
        unsafe {
            // Drop Context Flags = 0
            core::ptr::write_volatile(icc, 0);
            // Add Context Flags: set bit for slot context (0) and the endpoint (ep_dci)
            core::ptr::write_volatile(icc.add(1), (1 << 0) | (1 << ep_dci));
        }

        // Update Slot Context: set Context Entries to include this endpoint
        let slot_ctx = unsafe { input_ctx.as_ptr().add(ctx_size) } as *mut u32;
        unsafe {
            let dword0 = core::ptr::read_volatile(slot_ctx);
            // Context Entries is bits [31:27] - set to at least ep_dci
            let new_entries = ep_dci as u32;
            let dword0_new = (dword0 & !(0x1F << 27)) | (new_entries << 27);
            core::ptr::write_volatile(slot_ctx, dword0_new);
        }

        // Set up Endpoint Context for the interrupt IN endpoint
        let ep_ctx =
            unsafe { input_ctx.as_ptr().add((ep_dci as usize + 1) * ctx_size) } as *mut u32;
        unsafe {
            // Dword 0: Interval (bits [23:16])
            // For FS/HS interrupt endpoints the xHCI spec wants the exponent: bInterval-1
            let interval_val = if interval > 0 { interval - 1 } else { 0 };
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
        unsafe {
            // Drop Context Flags = 0
            core::ptr::write_volatile(icc, 0);
            // Add Context Flags: slot (bit 0) + EP IN (bit ep_in_dci) + EP OUT (bit ep_out_dci)
            core::ptr::write_volatile(icc.add(1), (1 << 0) | (1 << ep_in_dci) | (1 << ep_out_dci));
        }

        // Update Slot Context: Context Entries must cover the highest DCI used
        let slot_ctx = unsafe { input_ctx.as_ptr().add(ctx_size) as *mut u32 };
        unsafe {
            let dword0 = core::ptr::read_volatile(slot_ctx);
            let current_entries = (dword0 >> 27) & 0x1F;
            let new_entries = current_entries.max(ep_in_dci as u32).max(ep_out_dci as u32);
            core::ptr::write_volatile(slot_ctx, (dword0 & !(0x1F << 27)) | (new_entries << 27));
        }

        // EP Context for bulk IN (type 6)
        let ep_in_ctx =
            unsafe { input_ctx.as_ptr().add((ep_in_dci as usize + 1) * ctx_size) as *mut u32 };
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
        let ep_out_ctx =
            unsafe { input_ctx.as_ptr().add((ep_out_dci as usize + 1) * ctx_size) as *mut u32 };
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
        unsafe {
            reg_write(self.regs.doorbell(slot_id), ep_dci);
        }

        // Poll for a Transfer Event completion
        for _ in 0..10_000_000u32 {
            if let Some(event) = self.event_ring.as_mut().unwrap().poll() {
                let erdp = self.event_ring.as_ref().unwrap().dequeue_phys();
                let intr = self.regs.interrupter(0);
                unsafe {
                    reg_write(&mut (*intr).erdp_lo, (erdp as u32) | (1 << 3));
                    reg_write(&mut (*intr).erdp_hi, (erdp >> 32) as u32);
                }

                if event.trb_type() == TRB_TYPE_TRANSFER {
                    let comp_code = ((event.status >> 24) & 0xFF) as u8;
                    let residual = event.status & 0x00FF_FFFF;
                    if comp_code == COMP_SUCCESS || comp_code == COMP_SHORT_PACKET {
                        return Ok((length.saturating_sub(residual)) as usize);
                    }
                    return Err(XhciError::TransferError(comp_code));
                }
                // Consume non-transfer events (port status changes, etc.)
            }
            core::hint::spin_loop();
        }

        Err(XhciError::CommandTimeout)
    }
}

/// Main xHCI driver entry point, run as a kernel thread.
pub extern "C" fn xhci_driver_main() -> ! {
    println!("xhci: driver thread started");

    let mut controller = match XhciController::find_and_init() {
        Some(c) => c,
        None => {
            println!("xhci: no controller found");
            loop {
                sched().thread_park();
            }
        }
    };

    if let Err(e) = controller.init() {
        println!("xhci: init failed: {}", e);
        loop {
            sched().thread_park();
        }
    }

    // keyboard_device holds the active HID keyboard device and its interrupt IN transfer ring.
    let mut keyboard_device: Option<(UsbDevice, TransferRing)> = None;
    // mouse_device holds the active HID mouse device and its interrupt IN transfer ring.
    let mut mouse_device: Option<(UsbDevice, TransferRing)> = None;
    // Counter used to generate unique /dev/usbN names for mass storage devices.
    let mut usb_storage_count: usize = 0;

    // Scan all ports for already-connected devices.
    let max_ports = controller.regs.max_ports();
    for port in 1..=max_ports {
        let portsc = unsafe { reg_read(&(*controller.regs.port(port - 1)).portsc) };
        let ccs = portsc & 1; // Current Connect Status
        if ccs != 0 {
            println!("xhci: device detected on port {}", port);
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
                            parse_and_log_config(&config_data);

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
                        let kbd_info = device.config_data.as_deref().and_then(find_hid_keyboard);

                        if let Some((iface, ep)) = kbd_info {
                            println!("xhci: configuring HID keyboard on slot {}", device.slot_id);

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

                            // Configure the interrupt IN endpoint
                            match controller.configure_interrupt_endpoint(
                                &mut device,
                                ep_addr,
                                ep_maxpkt,
                                ep_interval,
                            ) {
                                Ok(ring) => {
                                    keyboard_device = Some((device, ring));
                                }
                                Err(e) => {
                                    println!("xhci: configure endpoint failed: {:?}", e);
                                }
                            }
                            continue;
                        }
                    }

                    // Check if this is a HID mouse and set it up (only use the first one found).
                    if mouse_device.is_none() {
                        let mouse_info = device.config_data.as_deref().and_then(find_hid_mouse);

                        if let Some((iface, ep)) = mouse_info {
                            println!("xhci: configuring HID mouse on slot {}", device.slot_id);

                            // Switch to boot protocol (0 = boot, 1 = report)
                            if let Err(e) = controller.set_hid_protocol(
                                &mut device,
                                iface.b_interface_number,
                                0,
                            ) {
                                println!("xhci: set mouse boot protocol failed: {:?}", e);
                            }

                            // Copy packed fields before the mutable borrow
                            let ep_addr = ep.b_endpoint_address;
                            let ep_maxpkt = ep.w_max_packet_size;
                            let ep_interval = ep.b_interval;

                            // Configure the interrupt IN endpoint
                            match controller.configure_interrupt_endpoint(
                                &mut device,
                                ep_addr,
                                ep_maxpkt,
                                ep_interval,
                            ) {
                                Ok(ring) => {
                                    mouse_device = Some((device, ring));
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

                                                    // Register in devfs
                                                    crate::drivers::usb::mass_storage::register_usb_storage(
                                                        usb_storage_count,
                                                        slot_id,
                                                        block_size,
                                                        block_count,
                                                        &data,
                                                    );
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

    // Allocate DMA buffers for HID reports and pre-fill the interrupt rings.
    // Keyboard: 8-byte boot report; mouse: 4-byte boot report (3 bytes + wheel).
    let kbd_report_buf =
        DmaBuffer::allocate_sized(8).expect("xhci: failed to allocate keyboard HID report buf");
    let kbd_report_phys = kbd_report_buf.phys_addr().as_u64();
    let mut prev_kbd_report = [0u8; 8];

    let mouse_report_buf =
        DmaBuffer::allocate_sized(4).expect("xhci: failed to allocate mouse HID report buf");
    let mouse_report_phys = mouse_report_buf.phys_addr().as_u64();

    if let Some((ref mut dev, ref mut ring)) = keyboard_device {
        let ep_dci = 3u32; // EP1 IN DCI = 1*2+1 = 3
        let trb = Trb {
            parameter: kbd_report_phys,
            status: 8,
            control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
        };
        ring.push(trb);
        unsafe { reg_write(controller.regs.doorbell(dev.slot_id), ep_dci) };
        println!("xhci: HID keyboard interrupt transfer queued");
        crate::drivers::keyboard::USB_KEYBOARD_ACTIVE
            .store(true, core::sync::atomic::Ordering::Relaxed);
    }

    if let Some((ref mut dev, ref mut ring)) = mouse_device {
        let ep_dci = 3u32; // EP1 IN DCI = 1*2+1 = 3
        let trb = Trb {
            parameter: mouse_report_phys,
            status: 4,
            control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
        };
        ring.push(trb);
        unsafe { reg_write(controller.regs.doorbell(dev.slot_id), ep_dci) };
        println!("xhci: HID mouse interrupt transfer queued");
        crate::drivers::mouse::USB_MOUSE_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);
    }

    // Main event loop: handle runtime events (hot-plug, transfer completions, etc.)
    loop {
        // Park until an interrupt wakes us.
        sched().thread_park();

        // Process all pending events.
        while let Some(event) = controller.event_ring.as_mut().unwrap().poll() {
            let erdp = controller.event_ring.as_ref().unwrap().dequeue_phys();
            let intr = controller.regs.interrupter(0);
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
                            .map_or(false, |(dev, _)| dev.slot_id == event_slot_id);
                        let is_mouse = mouse_device
                            .as_ref()
                            .map_or(false, |(dev, _)| dev.slot_id == event_slot_id);

                        if is_keyboard {
                            // Read the 8-byte keyboard report from the DMA buffer
                            let mut report = [0u8; 8];
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    kbd_report_buf.as_ptr(),
                                    report.as_mut_ptr(),
                                    8,
                                );
                            }

                            let key_events = crate::drivers::usb::hid::process_boot_keyboard_report(
                                &prev_kbd_report,
                                &report,
                            );
                            if !key_events.is_empty() {
                                crate::drivers::keyboard::KEY_EVENT_BROADCAST
                                    .broadcast_many(&key_events);
                            }
                            prev_kbd_report = report;

                            // Resubmit the TRB to receive the next keyboard report
                            if let Some((ref mut dev, ref mut ring)) = keyboard_device {
                                let ep_dci = 3u32;
                                let trb = Trb {
                                    parameter: kbd_report_phys,
                                    status: 8,
                                    control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
                                };
                                ring.push(trb);
                                unsafe { reg_write(controller.regs.doorbell(dev.slot_id), ep_dci) };
                            }
                        } else if is_mouse {
                            // Read the 4-byte mouse report from the DMA buffer.
                            // Use report_len = 4 to enable scroll wheel support.
                            let mut report = [0u8; 4];
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    mouse_report_buf.as_ptr(),
                                    report.as_mut_ptr(),
                                    4,
                                );
                            }

                            crate::drivers::usb::hid::process_boot_mouse_report(&report, 4);

                            // Resubmit the TRB to receive the next mouse report
                            if let Some((ref mut dev, ref mut ring)) = mouse_device {
                                let ep_dci = 3u32;
                                let trb = Trb {
                                    parameter: mouse_report_phys,
                                    status: 4,
                                    control: ((TRB_TYPE_NORMAL as u32) << 10) | TRB_IOC,
                                };
                                ring.push(trb);
                                unsafe { reg_write(controller.regs.doorbell(dev.slot_id), ep_dci) };
                            }
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

        // Clear IMAN IP bit (W1C) to allow new MSI-X interrupts
        let intr = controller.regs.interrupter(0);
        unsafe {
            let iman = reg_read(&(*intr).iman);
            if iman & 1 != 0 {
                reg_write(&mut (*intr).iman, iman | 1);
            }
        }
    }
}

/// Parse a configuration descriptor blob and log the interfaces and endpoints found.
fn parse_and_log_config(data: &[u8]) {
    let mut offset = 0;
    while offset + 2 <= data.len() {
        let length = data[offset] as usize;
        let desc_type = data[offset + 1];

        if length == 0 {
            break;
        }
        if offset + length > data.len() {
            break;
        }

        if desc_type == DESC_INTERFACE && length >= 9 {
            let iface =
                unsafe { core::ptr::read(data[offset..].as_ptr() as *const InterfaceDescriptor) };
            println!(
                "xhci:   interface {}: class={} subclass={} protocol={} endpoints={}",
                iface.b_interface_number,
                iface.b_interface_class,
                iface.b_interface_sub_class,
                iface.b_interface_protocol,
                iface.b_num_endpoints
            );
        } else if desc_type == DESC_ENDPOINT && length >= 7 {
            let ep =
                unsafe { core::ptr::read(data[offset..].as_ptr() as *const EndpointDescriptor) };
            let dir = if ep.b_endpoint_address & 0x80 != 0 {
                "IN"
            } else {
                "OUT"
            };
            let ep_type = match ep.bm_attributes & 0x3 {
                0 => "Control",
                1 => "Isochronous",
                2 => "Bulk",
                3 => "Interrupt",
                _ => unreachable!(),
            };
            // Copy packed field to local before passing to println.
            let max_pkt = ep.w_max_packet_size;
            println!(
                "xhci:     EP{} {} {} maxpkt={}",
                ep.b_endpoint_address & 0x0F,
                dir,
                ep_type,
                max_pkt
            );
        }

        offset += length;
    }
}

/// Search a configuration descriptor blob for a HID keyboard interface and its interrupt IN endpoint.
///
/// Returns `(InterfaceDescriptor, EndpointDescriptor)` for the first matching pair found,
/// or `None` if the config data contains no HID keyboard.
fn find_hid_keyboard(config_data: &[u8]) -> Option<(InterfaceDescriptor, EndpointDescriptor)> {
    let mut offset = 0;
    let mut current_iface: Option<InterfaceDescriptor> = None;

    while offset + 2 <= config_data.len() {
        let length = config_data[offset] as usize;
        let desc_type = config_data[offset + 1];

        if length == 0 || offset + length > config_data.len() {
            break;
        }

        if desc_type == DESC_INTERFACE && length >= 9 {
            let iface = unsafe {
                core::ptr::read(config_data[offset..].as_ptr() as *const InterfaceDescriptor)
            };
            if iface.b_interface_class == USB_CLASS_HID
                && iface.b_interface_protocol == HID_PROTOCOL_KEYBOARD
            {
                current_iface = Some(iface);
            } else {
                current_iface = None;
            }
        } else if desc_type == DESC_ENDPOINT && length >= 7 {
            if let Some(iface) = current_iface {
                let ep = unsafe {
                    core::ptr::read(config_data[offset..].as_ptr() as *const EndpointDescriptor)
                };
                // Accept only IN interrupt endpoints
                if ep.b_endpoint_address & 0x80 != 0 && ep.bm_attributes & 0x03 == 3 {
                    return Some((iface, ep));
                }
            }
        }

        offset += length;
    }
    None
}

/// Search a configuration descriptor blob for a HID mouse interface and its interrupt IN endpoint.
///
/// Returns `(InterfaceDescriptor, EndpointDescriptor)` for the first matching pair found,
/// or `None` if the config data contains no HID mouse.
fn find_hid_mouse(config_data: &[u8]) -> Option<(InterfaceDescriptor, EndpointDescriptor)> {
    let mut offset = 0;
    let mut current_iface: Option<InterfaceDescriptor> = None;

    while offset + 2 <= config_data.len() {
        let length = config_data[offset] as usize;
        let desc_type = config_data[offset + 1];

        if length == 0 || offset + length > config_data.len() {
            break;
        }

        if desc_type == DESC_INTERFACE && length >= 9 {
            let iface = unsafe {
                core::ptr::read(config_data[offset..].as_ptr() as *const InterfaceDescriptor)
            };
            if iface.b_interface_class == USB_CLASS_HID
                && iface.b_interface_protocol == HID_PROTOCOL_MOUSE
            {
                current_iface = Some(iface);
            } else {
                current_iface = None;
            }
        } else if desc_type == DESC_ENDPOINT && length >= 7 {
            if let Some(iface) = current_iface {
                let ep = unsafe {
                    core::ptr::read(config_data[offset..].as_ptr() as *const EndpointDescriptor)
                };
                // Accept only IN interrupt endpoints
                if ep.b_endpoint_address & 0x80 != 0 && ep.bm_attributes & 0x03 == 3 {
                    return Some((iface, ep));
                }
            }
        }

        offset += length;
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
    const USB_CLASS_MASS_STORAGE: u8 = 0x08;
    const USB_SUBCLASS_SCSI: u8 = 0x06;
    const USB_PROTOCOL_BOT: u8 = 0x50;

    let mut offset = 0;
    let mut current_iface: Option<InterfaceDescriptor> = None;
    let mut ep_in: Option<EndpointDescriptor> = None;
    let mut ep_out: Option<EndpointDescriptor> = None;

    while offset + 2 <= config_data.len() {
        let length = config_data[offset] as usize;
        let desc_type = config_data[offset + 1];

        if length == 0 || offset + length > config_data.len() {
            break;
        }

        if desc_type == DESC_INTERFACE && length >= 9 {
            // Starting a new interface; reset endpoint state.
            let iface = unsafe {
                core::ptr::read(config_data[offset..].as_ptr() as *const InterfaceDescriptor)
            };
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
        } else if desc_type == DESC_ENDPOINT && length >= 7 {
            if current_iface.is_some() {
                let ep = unsafe {
                    core::ptr::read(config_data[offset..].as_ptr() as *const EndpointDescriptor)
                };
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

        offset += length;
    }
    None
}
